use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::github::Conditional;
use crate::models::IssueListing;
use crate::{config, github, launch, state, wait};

/// Direct polling starts here…
const BACKOFF_FLOOR: Duration = Duration::from_secs(5);
/// …doubles while idle, and caps here.
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// This many consecutive failed polls aborts the wait. Far higher than `wait`'s
/// threshold: a pool worker runs unattended, so a transient network blip
/// shouldn't kill it (~30 min at the cap before giving up).
const MAX_CONSECUTIVE_FAILURES: u32 = 30;

/// Pick the next issue to work on, claim it, print the pick and its rationale,
/// and return its number for the caller to hand to `work-on`.
///
/// Eligible issues are the repo's open issues, excluding PRs and issues assigned
/// to someone other than the user. An issue with a *live* session is skipped (a
/// worker is on it); one that was started and then stopped is *resumed* rather
/// than skipped; a concluded one is left alone. A *fresh* issue that is blocked
/// by an open dependency, or is a tracking issue (its sub-issues are picked
/// instead), is also skipped. When `only_assigned_to_me` is configured,
/// unassigned issues are excluded too. Claiming a fresh winner atomically
/// reserves it against concurrent `next`/`next --wait` runs (a resumed winner is
/// reserved by the launcher's session lease instead), and assigns it on GitHub.
pub fn pick() -> Result<u64> {
    let (priority_labels, only_assigned_to_me) = match config::find()? {
        Some(located) => (
            located.config.priority_labels,
            located.config.only_assigned_to_me,
        ),
        None => (Vec::new(), false),
    };
    let (owner, repo) = github::repo_or_cwd()?;
    let me = github::authenticated_user()?;
    let issues = github::list_open_issues(&owner, &repo)?;

    let selection = claim_pick(
        &issues,
        &me,
        &priority_labels,
        only_assigned_to_me,
        |_| false,
        status_fn(&owner, &repo),
        |n| state::claim(&owner, &repo, n),
    )?;

    match announce_pick(&selection, &me, &priority_labels, &owner, &repo) {
        Some(number) => Ok(number),
        None => bail!(
            "no eligible open issues in {owner}/{repo}: every open issue is a PR, is assigned \
             to someone else, already has a live session, or is complete."
        ),
    }
}

/// Block until an eligible issue can be claimed, returning its number for the
/// caller to hand to `work-on`. With `Some(timeout)`, exits the process with
/// [`wait::EXIT_TIMEOUT`] once it elapses with nothing claimed; with `None`,
/// waits indefinitely — the normal worker-pool case.
///
/// Polls the open-issues listing with conditional (ETag) GETs and the same
/// floor/cap backoff as `wait`; a fresh listing re-arms eager polling. Only one
/// endpoint is polled per cycle, so even at the cap a worker costs ~60 req/hr —
/// no events-feed idle mode is needed.
pub fn wait_for_pick(timeout_secs: Option<u64>) -> Result<u64> {
    wait_for_pick_excluding(timeout_secs, &BTreeSet::new())
}

/// [`wait_for_pick`], but with a set of issue numbers the caller has already
/// given up on this run (a `forever` worker's launch failures), excluded from
/// selection so they can't be re-picked in a loop.
fn wait_for_pick_excluding(timeout_secs: Option<u64>, skip: &BTreeSet<u64>) -> Result<u64> {
    let (priority_labels, only_assigned_to_me) = match config::find()? {
        Some(located) => (
            located.config.priority_labels,
            located.config.only_assigned_to_me,
        ),
        None => (Vec::new(), false),
    };
    let (owner, repo) = github::repo_or_cwd()?;
    let me = github::authenticated_user()?;
    let endpoint = format!("repos/{owner}/{repo}/issues?state=open&per_page=100");

    match timeout_secs {
        Some(secs) => {
            println!("Waiting for an eligible issue to claim in {owner}/{repo} (timeout {secs} s)…")
        }
        None => println!("Waiting for an eligible issue to claim in {owner}/{repo}…"),
    }

    let deadline = timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
    let mut etag: Option<String> = None;
    let mut backoff = BACKOFF_FLOOR;
    let mut consecutive_failures: u32 = 0;

    loop {
        match github::gh_api_conditional(&endpoint, etag.as_deref()) {
            Ok(Conditional::NotModified { .. }) => {
                consecutive_failures = 0;
            }
            Ok(Conditional::Fresh {
                etag: new_etag,
                body,
                ..
            }) => {
                consecutive_failures = 0;
                if let Some(new_etag) = new_etag {
                    etag = Some(new_etag);
                }
                let issues: Vec<IssueListing> = serde_json::from_str(&body)
                    .context("failed to parse open-issues listing while waiting")?;
                let selection = claim_pick(
                    &issues,
                    &me,
                    &priority_labels,
                    only_assigned_to_me,
                    |n| skip.contains(&n),
                    status_fn(&owner, &repo),
                    |n| state::claim(&owner, &repo, n),
                )?;
                if let Some(number) =
                    announce_pick(&selection, &me, &priority_labels, &owner, &repo)
                {
                    return Ok(number);
                }
                // The listing changed but held nothing to claim; poll eagerly
                // again in case more lands.
                backoff = BACKOFF_FLOOR;
            }
            Err(err) if github::is_rate_limited(&err) => {
                eprintln!("warning: rate limited; backing off: {err:#}");
                backoff = BACKOFF_CAP;
            }
            Err(err) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(err.context("giving up after repeated polling failures"));
                }
                eprintln!("warning: poll failed (attempt {consecutive_failures}): {err:#}");
            }
        }

        let pace = backoff;
        backoff = (backoff * 2).min(BACKOFF_CAP);
        match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                std::thread::sleep(pace.min(remaining));
                if Instant::now() >= deadline {
                    break;
                }
            }
            None => std::thread::sleep(pace),
        }
    }

    println!(
        "No eligible issue to claim within {} s. Run `ghwf next --wait` again to keep waiting.",
        timeout_secs.expect("a reached deadline implies a timeout was set")
    );
    std::process::exit(wait::EXIT_TIMEOUT);
}

/// Work issues one after another as a supervised pool worker (`ghwf forever`):
/// claim the next eligible issue, run its Claude session to conclusion, bring it
/// down, and pick again — indefinitely.
///
/// Each round waits (with no timeout) for an eligible issue, so an empty queue
/// parks the worker rather than ending it. The loop stops only when the user
/// quits a session before its workflow concludes — read as "the user has stepped
/// in and wants out" (re-run the command to resume). While a session runs the
/// supervisor ignores terminal Ctrl-C (Claude handles its own); between sessions
/// (waiting for a pick) Ctrl-C stops the worker as usual.
///
/// A transient launch failure never stops the worker or locks the issue: it is
/// logged, a bare claim with nothing behind it is released back to the pool, and
/// the loop picks again. A pick another worker has leased in the meantime is
/// skipped the same way.
pub fn run_forever(no_branch: bool) -> Result<()> {
    // The issue repo selection works against, resolved once so a launch failure
    // can release the matching claim.
    let (owner, repo) = github::repo_or_cwd()?;
    // Issues this worker has failed to launch this run, excluded from further
    // picks so a deterministic failure (e.g. an unusable `Model:` line) can't
    // loop. They're left for a fresh worker (or a manual `ghwf work-on`).
    let mut skip: BTreeSet<u64> = BTreeSet::new();
    loop {
        let number = wait_for_pick_excluding(None, &skip)?;
        let launch = match launch::prepare(&number.to_string(), no_branch) {
            Ok(Some(launch)) => launch,
            // A live session already holds it (leased between selection and now).
            Ok(None) => continue,
            Err(err) => {
                eprintln!("warning: couldn't start #{number}, leaving it for the pool: {err:#}");
                // Don't re-pick it this run.
                skip.insert(number);
                // Undo a bare claim so the issue stays pickable by a fresh
                // worker; an issue with real progress (or one parked for you) is
                // kept and resumed later. Any lease was released as `prepare`
                // unwound.
                if let Err(err) = state::release_if_unstarted(&owner, &repo, number) {
                    eprintln!("warning: failed to release #{number}'s claim: {err:#}");
                }
                continue;
            }
        };
        match launch::supervise_once(&launch)? {
            launch::Outcome::Completed => {
                println!("Issue #{number} concluded; looking for the next one.");
            }
            launch::Outcome::UserQuit => {
                println!(
                    "The session for issue #{number} ended before its workflow concluded, \
                     so the forever worker is stopping. Re-run `ghwf forever` to resume."
                );
                return Ok(());
            }
        }
    }
}

/// The status classifier for a repo, reading each issue's recorded state and
/// session lease.
fn status_fn<'a>(owner: &'a str, repo: &'a str) -> impl Fn(u64) -> IssueStatus + 'a {
    move |number: u64| issue_status(owner, repo, number)
}

/// Classify an issue for selection from its recorded state and lease: no state
/// is `Fresh`; concluded state is `Done`; otherwise a live lease is `Live` and
/// no live lease is `Resumable`. A state file that exists but fails to parse is
/// treated as `Live` — the conservative choice, never barging into something we
/// can't read.
fn issue_status(owner: &str, repo: &str, number: u64) -> IssueStatus {
    match state::load_if_exists(owner, repo, number) {
        Ok(None) => IssueStatus::Fresh,
        Ok(Some(state)) => {
            if state.is_concluded() {
                IssueStatus::Done
            } else {
                match state::lease_liveness(owner, repo, number) {
                    state::Liveness::Live => IssueStatus::Live,
                    state::Liveness::NotLive => IssueStatus::Resumable,
                }
            }
        }
        Err(_) => IssueStatus::Live,
    }
}

/// Whether an issue's work is already underway (started and not concluded), for
/// the tracking-issue redirect's "prefer a started child" rule.
fn is_underway(owner: &str, repo: &str, number: u64) -> bool {
    matches!(
        issue_status(owner, repo, number),
        IssueStatus::Resumable | IssueStatus::Live
    )
}

/// Select the best workable issue and, for a fresh one, claim it — re-selecting
/// when a claim is lost to a concurrent worker. Returns the `Selection` whose
/// `picked` issue is ours to launch, or one with `picked: None` when nothing
/// remains.
///
/// A `Fresh` pick is claimed via `claim` (`true` when this caller won it,
/// `false` when another session already holds it; a lost race excludes that
/// issue and selects again). A `Resumable` pick is returned without claiming:
/// its single-flight is the launcher acquiring the session lease, so a race
/// there is resolved when the launcher backs off. `excluded` drops issues the
/// caller has already given up on this run (a worker's launch failures), so a
/// deterministic failure can't be re-picked in a loop. `excluded`, `status`,
/// and `claim` are injected so the loop is testable without a filesystem or
/// network.
fn claim_pick<'a>(
    issues: &'a [IssueListing],
    me: &str,
    priority_labels: &[String],
    only_assigned_to_me: bool,
    excluded: impl Fn(u64) -> bool,
    status: impl Fn(u64) -> IssueStatus,
    mut claim: impl FnMut(u64) -> Result<bool>,
) -> Result<Selection<'a>> {
    // Issues lost to other workers this call, excluded from re-selection on top
    // of those the caller already excludes.
    let mut lost: BTreeSet<u64> = BTreeSet::new();
    loop {
        let selection = select(
            issues,
            me,
            priority_labels,
            only_assigned_to_me,
            |n| lost.contains(&n) || excluded(n),
            &status,
        );
        let Some(picked) = selection.picked else {
            return Ok(selection);
        };
        // Only fresh picks are claimed here; a resumable one is single-flighted
        // by the launcher's lease.
        if selection.picked_status != Some(IssueStatus::Fresh) || claim(picked.number)? {
            return Ok(selection);
        }
        lost.insert(picked.number);
    }
}

/// Report a claimed selection: print the skipped lines, assign the picked issue
/// to `me` on GitHub (best-effort — the claim or lease already guarantees
/// exclusivity, so a failure is only a lost visibility cue), print the pick and
/// its rationale, and return the picked number. Returns `None` when nothing was
/// picked.
fn announce_pick(
    selection: &Selection,
    me: &str,
    priority_labels: &[String],
    owner: &str,
    repo: &str,
) -> Option<u64> {
    for number in &selection.skipped_live {
        println!("Skipping #{number} — a session is currently running it.");
    }
    for number in &selection.skipped_blocked {
        println!("Skipping #{number} — blocked by an open issue.");
    }
    for number in &selection.skipped_tracking {
        println!(
            "Skipping #{number} — a tracking issue (has sub-issues); \
             work a sub-issue with `ghwf work-on {number}` or `ghwf work-on <sub>`."
        );
    }
    let issue = selection.picked?;
    if !assigned_to(issue, me) {
        if let Err(err) = github::add_assignee(owner, repo, issue.number, me) {
            eprintln!(
                "warning: failed to assign #{} to {me} (the claim still stands): {err:#}",
                issue.number
            );
        }
    }
    // A resumable pick is in-progress work being re-entered, not a new start.
    let verb = if selection.picked_status == Some(IssueStatus::Resumable) {
        "Resuming"
    } else {
        "Picked"
    };
    println!(
        "{verb} #{} \"{}\" — {}.",
        issue.number,
        issue.title,
        rationale(issue, me, priority_labels)
    );
    Some(issue.number)
}

/// An issue's status for selection, derived from its recorded state and session
/// lease. `Fresh` and `Resumable` are both workable (one starts, the other
/// resumes); `Live` and `Done` are not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IssueStatus {
    /// No recorded state — start it.
    Fresh,
    /// Recorded state, not concluded, no live session — resume it.
    Resumable,
    /// A live session is running it — a worker is on it.
    Live,
    /// Recorded state, concluded — nothing left to do.
    Done,
}

/// The outcome of [`select`]: the winning issue, if any, its status (so callers
/// know whether the pick is a start or a resume), and the issues passed over for
/// a reportable reason (each in listing order).
struct Selection<'a> {
    picked: Option<&'a IssueListing>,
    picked_status: Option<IssueStatus>,
    skipped_live: Vec<u64>,
    skipped_blocked: Vec<u64>,
    skipped_tracking: Vec<u64>,
}

/// Choose the most important workable issue.
///
/// Sort key, ascending — first wins: issues assigned to `me` before
/// unassigned ones, then the best (earliest-in-list) priority label, then the
/// lowest issue number. PRs, issues assigned to someone else, and issues
/// excluded by the caller (lost to a concurrent worker this call) are dropped
/// outright. Of the rest, `Live` issues are skipped and reported, `Done` ones
/// skipped silently, and `Resumable` ones are workable regardless of block or
/// tracking state (their work is already underway). A `Fresh` issue that is
/// currently blocked or a tracking issue (has sub-issues) is skipped and
/// reported instead; when it qualifies for both, the precedence is blocked →
/// tracking.
///
/// When `only_assigned_to_me` is set, unassigned issues are excluded too (so the
/// pool is exactly the issues assigned to `me`); like issues assigned to someone
/// else, they're dropped silently rather than reported.
fn select<'a>(
    issues: &'a [IssueListing],
    me: &str,
    priority_labels: &[String],
    only_assigned_to_me: bool,
    excluded: impl Fn(u64) -> bool,
    status: impl Fn(u64) -> IssueStatus,
) -> Selection<'a> {
    let mut skipped_live = Vec::new();
    let mut skipped_blocked = Vec::new();
    let mut skipped_tracking = Vec::new();
    let mut candidates: Vec<&IssueListing> = Vec::new();
    for issue in issues {
        if issue.pull_request.is_some() {
            continue;
        }
        if only_assigned_to_me {
            if !assigned_to(issue, me) {
                continue;
            }
        } else if !issue.assignees.is_empty() && !assigned_to(issue, me) {
            continue;
        }
        if excluded(issue.number) {
            continue;
        }
        match status(issue.number) {
            // A worker is on it; note it so it's reported.
            IssueStatus::Live => {
                skipped_live.push(issue.number);
                continue;
            }
            // Concluded — nothing to do, and not worth a line.
            IssueStatus::Done => continue,
            // Work is underway: resume it whatever its block/tracking state.
            IssueStatus::Resumable => {
                candidates.push(issue);
                continue;
            }
            // A fresh issue still has to clear the freshness gates below.
            IssueStatus::Fresh => {}
        }
        if issue.is_blocked() {
            skipped_blocked.push(issue.number);
            continue;
        }
        if issue.is_tracking() {
            skipped_tracking.push(issue.number);
            continue;
        }
        candidates.push(issue);
    }
    candidates.sort_by_key(|issue| sort_key(issue, me, priority_labels));
    let picked = candidates.first().copied();
    Selection {
        picked,
        picked_status: picked.map(|issue| status(issue.number)),
        skipped_live,
        skipped_blocked,
        skipped_tracking,
    }
}

/// The ascending sort key shared by `select` and the tracking-issue redirect:
/// assigned-to-`me` first, then best priority label, then lowest number.
fn sort_key(issue: &IssueListing, me: &str, priority_labels: &[String]) -> (bool, usize, u64) {
    (
        !assigned_to(issue, me),
        label_rank(issue, priority_labels),
        issue.number,
    )
}

/// Whether `me` is among the issue's assignees. GitHub logins are
/// case-insensitive.
fn assigned_to(issue: &IssueListing, me: &str) -> bool {
    issue
        .assignees
        .iter()
        .any(|a| a.login.eq_ignore_ascii_case(me))
}

/// The index into `priority_labels` of the issue's best (earliest-in-list)
/// priority label; `usize::MAX` when it carries none, so unlabelled issues
/// sort after all labelled ones. Label names compare case-insensitively, as
/// GitHub treats them.
fn label_rank(issue: &IssueListing, priority_labels: &[String]) -> usize {
    issue
        .labels
        .iter()
        .filter_map(|label| {
            priority_labels
                .iter()
                .position(|p| p.eq_ignore_ascii_case(&label.name))
        })
        .min()
        .unwrap_or(usize::MAX)
}

/// A one-line explanation of why the issue won.
fn rationale(issue: &IssueListing, me: &str, priority_labels: &[String]) -> String {
    let mut parts = Vec::new();
    if assigned_to(issue, me) {
        parts.push(format!("assigned to {me}"));
    }
    let rank = label_rank(issue, priority_labels);
    if rank != usize::MAX {
        parts.push(format!("priority label `{}`", priority_labels[rank]));
    }
    if parts.is_empty() {
        "the earliest eligible open issue".to_string()
    } else {
        parts.join(", ")
    }
}

/// Resolve the issue a launch should actually work on.
///
/// For a normal issue this is `number` itself. For a *tracking* issue (one with
/// sub-issues) it is a workable descendant, chosen by the same ordering `select`
/// uses but preferring an already-started one so re-running `work-on <parent>`
/// resumes in-progress work. Recurses through tracking-issue children to reach a
/// workable leaf.
///
/// Best-effort about *discovering* relationships: if the first sub-issues lookup
/// fails (offline, or the feature is unavailable) it warns and returns `number`
/// unchanged, preserving the launcher's offline path. A genuine "this is a
/// tracking issue but nothing under it is workable" outcome is surfaced as an
/// error rather than silently working the parent.
pub fn resolve_workable(owner: &str, repo: &str, number: u64) -> Result<u64> {
    let children = match github::list_sub_issues(owner, repo, number) {
        Ok(children) => children,
        Err(err) => {
            eprintln!(
                "warning: couldn't check issue relationships for #{number} \
                 (working it as given): {err:#}"
            );
            return Ok(number);
        }
    };
    if children.is_empty() {
        // Not a tracking issue: work it directly.
        return Ok(number);
    }

    let priority_labels = match config::find()? {
        Some(located) => located.config.priority_labels,
        None => Vec::new(),
    };
    let me = github::authenticated_user()?;
    pick_workable_leaf(
        number,
        children,
        &me,
        &priority_labels,
        &|n| github::list_sub_issues(owner, repo, n),
        &|n| is_underway(owner, repo, n),
    )
}

/// Choose a workable descendant leaf of tracking issue `parent`, whose direct
/// `children` have already been fetched. Recurses into tracking children via
/// `list_children`; `underway` reports whether a leaf's work has already begun
/// (started and not concluded). Both are injected so the traversal is testable
/// without the network or a filesystem.
///
/// Preference order: an underway leaf (resumed — even if now blocked, since work
/// is underway) before the best fresh, non-blocked one. Errors when no
/// descendant is workable.
fn pick_workable_leaf(
    parent: u64,
    children: Vec<IssueListing>,
    me: &str,
    priority_labels: &[String],
    list_children: &impl Fn(u64) -> Result<Vec<IssueListing>>,
    underway: &impl Fn(u64) -> bool,
) -> Result<u64> {
    // Guard against a malformed cycle in the sub-issue graph, and dedupe a leaf
    // reachable via more than one parent.
    let mut visited: BTreeSet<u64> = BTreeSet::new();
    visited.insert(parent);
    let mut leaves: Vec<IssueListing> = Vec::new();
    for child in children.into_iter().filter(IssueListing::is_open) {
        collect_leaves(child, list_children, &mut visited, &mut leaves)?;
    }

    if let Some(best) = leaves
        .iter()
        .filter(|leaf| underway(leaf.number))
        .min_by_key(|leaf| sort_key(leaf, me, priority_labels))
    {
        return Ok(best.number);
    }
    if let Some(best) = leaves
        .iter()
        .filter(|leaf| !leaf.is_blocked())
        .min_by_key(|leaf| sort_key(leaf, me, priority_labels))
    {
        return Ok(best.number);
    }
    bail!(
        "#{parent} is a tracking issue, but none of its sub-issues are workable \
         (all are blocked, closed, or already complete); pick a specific sub-issue \
         with `ghwf work-on <sub>`."
    )
}

/// Depth-first walk from `node`, pushing each open non-tracking leaf into
/// `leaves`. Tracking nodes are descended into (their children fetched via
/// `list_children`); `visited` dedupes and stops cycles.
fn collect_leaves(
    node: IssueListing,
    list_children: &impl Fn(u64) -> Result<Vec<IssueListing>>,
    visited: &mut BTreeSet<u64>,
    leaves: &mut Vec<IssueListing>,
) -> Result<()> {
    if !visited.insert(node.number) {
        return Ok(());
    }
    if node.is_tracking() {
        for child in list_children(node.number)?
            .into_iter()
            .filter(IssueListing::is_open)
        {
            collect_leaves(child, list_children, visited, leaves)?;
        }
    } else {
        leaves.push(node);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use super::{claim_pick, pick_workable_leaf, select, IssueStatus, Selection};
    use crate::models::{IssueDependenciesSummary, IssueListing, Label, SubIssuesSummary, User};

    /// An open issue with the given assignees and labels, not blocked and not a
    /// tracking issue.
    fn issue(number: u64, assignees: &[&str], labels: &[&str]) -> IssueListing {
        IssueListing {
            number,
            title: format!("issue {number}"),
            state: String::new(),
            assignees: assignees
                .iter()
                .map(|login| User {
                    login: login.to_string(),
                })
                .collect(),
            labels: labels
                .iter()
                .map(|name| Label {
                    name: name.to_string(),
                })
                .collect(),
            pull_request: None,
            issue_dependencies_summary: IssueDependenciesSummary::default(),
            sub_issues_summary: SubIssuesSummary::default(),
        }
    }

    /// A PR entry, as the issues listing reports one.
    fn pr(number: u64) -> IssueListing {
        IssueListing {
            pull_request: Some(serde_json::json!({})),
            ..issue(number, &[], &[])
        }
    }

    /// An issue blocked by `n` open issues.
    fn blocked(number: u64, n: u64) -> IssueListing {
        IssueListing {
            issue_dependencies_summary: IssueDependenciesSummary { blocked_by: n },
            ..issue(number, &[], &[])
        }
    }

    /// A tracking issue: one carrying `n` sub-issues.
    fn tracking(number: u64, n: u64) -> IssueListing {
        IssueListing {
            sub_issues_summary: SubIssuesSummary { total: n },
            ..issue(number, &[], &[])
        }
    }

    /// Set a non-open state on an issue (sub-issue children can be closed).
    fn closed(mut issue: IssueListing) -> IssueListing {
        issue.state = "closed".to_string();
        issue
    }

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// Run `select` with every issue fresh and nothing excluded.
    fn pick<'a>(issues: &'a [IssueListing], me: &str, priority_labels: &[String]) -> Selection<'a> {
        select(
            issues,
            me,
            priority_labels,
            false,
            |_| false,
            |_| IssueStatus::Fresh,
        )
    }

    #[test]
    fn assignment_beats_labels() {
        let issues = [issue(1, &[], &["urgent"]), issue(2, &["me"], &[])];
        let picked = pick(&issues, "me", &labels(&["urgent"])).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn earlier_label_beats_later() {
        let issues = [issue(1, &[], &["soon"]), issue(2, &[], &["urgent"])];
        let picked = pick(&issues, "me", &labels(&["urgent", "soon"]))
            .picked
            .unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn any_priority_label_beats_none() {
        let issues = [issue(1, &[], &[]), issue(2, &[], &["soon"])];
        let picked = pick(&issues, "me", &labels(&["urgent", "soon"]))
            .picked
            .unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn best_label_counts() {
        // An issue's smallest-index label is what ranks it.
        let issues = [issue(1, &[], &["soon"]), issue(2, &[], &["soon", "urgent"])];
        let picked = pick(&issues, "me", &labels(&["urgent", "soon"]))
            .picked
            .unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn labels_rank_within_assigned_group_too() {
        let issues = [issue(1, &["me"], &[]), issue(2, &["me"], &["urgent"])];
        let picked = pick(&issues, "me", &labels(&["urgent"])).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn number_breaks_ties() {
        let issues = [issue(7, &[], &[]), issue(3, &[], &[]), issue(5, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 3);
    }

    #[test]
    fn assigned_to_someone_else_is_excluded() {
        // Even a top-priority label doesn't make someone else's issue mine.
        let issues = [issue(1, &["other"], &["urgent"]), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &labels(&["urgent"])).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn co_assignment_counts_as_mine() {
        let issues = [issue(1, &["other", "me"], &[]), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 1);
    }

    #[test]
    fn login_compares_case_insensitively() {
        let issues = [issue(1, &["Me"], &[]), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 1);
    }

    #[test]
    fn only_assigned_to_me_excludes_unassigned() {
        // With the option on, an unassigned issue is ignored even when it would
        // otherwise outrank the assigned one; the assigned-to-me issue wins.
        let issues = [issue(1, &[], &["urgent"]), issue(2, &["me"], &[])];
        let selection = select(
            &issues,
            "me",
            &labels(&["urgent"]),
            true,
            |_| false,
            |_| IssueStatus::Fresh,
        );
        assert_eq!(selection.picked.unwrap().number, 2);
    }

    #[test]
    fn only_assigned_to_me_drops_unassigned_silently() {
        // An unassigned-only pool yields nothing, and the dropped issue isn't
        // reported as a skip (it's not noteworthy when the option is on).
        let issues = [issue(1, &[], &["urgent"])];
        let selection = select(
            &issues,
            "me",
            &labels(&["urgent"]),
            true,
            |_| false,
            |_| IssueStatus::Fresh,
        );
        assert!(selection.picked.is_none());
        assert!(selection.skipped_live.is_empty());
        assert!(selection.skipped_blocked.is_empty());
        assert!(selection.skipped_tracking.is_empty());
    }

    #[test]
    fn unassigned_eligible_when_option_off() {
        // Guard the default: with the option off, an unassigned issue is still
        // picked (today's behaviour) rather than being filtered out.
        let issues = [issue(1, &[], &[])];
        let selection = select(&issues, "me", &[], false, |_| false, |_| IssueStatus::Fresh);
        assert_eq!(selection.picked.unwrap().number, 1);
    }

    #[test]
    fn prs_are_excluded() {
        let issues = [pr(1), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn live_issues_are_skipped_and_reported() {
        let issues = [
            issue(1, &["me"], &[]),
            // Excluded outright, so not reported as skipped.
            issue(2, &["other"], &[]),
            issue(3, &[], &[]),
        ];
        let selection = select(
            &issues,
            "me",
            &[],
            false,
            |_| false,
            |n| {
                if n == 1 {
                    IssueStatus::Live
                } else {
                    IssueStatus::Fresh
                }
            },
        );
        assert_eq!(selection.picked.unwrap().number, 3);
        assert_eq!(selection.skipped_live, [1]);
    }

    #[test]
    fn resumable_issue_is_selected_as_a_resume() {
        // A resumable issue (work underway, no live session) is a candidate, and
        // a winning resumable pick is flagged so it's announced as a resume.
        let issues = [issue(1, &[], &[])];
        let selection = select(
            &issues,
            "me",
            &[],
            false,
            |_| false,
            |_| IssueStatus::Resumable,
        );
        assert_eq!(selection.picked.unwrap().number, 1);
        assert_eq!(selection.picked_status, Some(IssueStatus::Resumable));
    }

    #[test]
    fn resumable_issue_is_workable_even_when_blocked() {
        // A resumable issue is resumed regardless of a block — its work is
        // already underway — unlike a fresh one, which a block would skip.
        let issues = [blocked(1, 1)];
        let selection = select(
            &issues,
            "me",
            &[],
            false,
            |_| false,
            |_| IssueStatus::Resumable,
        );
        assert_eq!(selection.picked.unwrap().number, 1);
        assert!(selection.skipped_blocked.is_empty());
    }

    #[test]
    fn done_issue_is_skipped_silently() {
        // A concluded issue is neither picked nor reported in any skip bucket.
        let issues = [issue(1, &[], &[])];
        let selection = select(&issues, "me", &[], false, |_| false, |_| IssueStatus::Done);
        assert!(selection.picked.is_none());
        assert!(selection.skipped_live.is_empty());
        assert!(selection.skipped_blocked.is_empty());
        assert!(selection.skipped_tracking.is_empty());
    }

    #[test]
    fn no_priority_labels_degrades_to_assigned_then_number() {
        let issues = [
            issue(1, &[], &["urgent"]),
            issue(2, &[], &[]),
            issue(3, &["me"], &[]),
        ];
        let selection = pick(&issues, "me", &[]);
        assert_eq!(selection.picked.unwrap().number, 3);
    }

    #[test]
    fn empty_pool_picks_nothing() {
        let issues = [pr(1), issue(2, &["other"], &[])];
        let selection = pick(&issues, "me", &[]);
        assert!(selection.picked.is_none());
        assert!(selection.skipped_live.is_empty());
    }

    /// Run `claim_pick` with every issue fresh, claiming the numbers in `won`
    /// and losing every other. Records the claim attempts in order.
    fn claim_with<'a>(
        issues: &'a [IssueListing],
        won: &[u64],
        attempts: &RefCell<Vec<u64>>,
    ) -> Selection<'a> {
        let won: BTreeSet<u64> = won.iter().copied().collect();
        claim_pick(
            issues,
            "me",
            &[],
            false,
            |_| false,
            |_| IssueStatus::Fresh,
            |n| {
                attempts.borrow_mut().push(n);
                Ok(won.contains(&n))
            },
        )
        .unwrap()
    }

    #[test]
    fn claim_pick_returns_the_first_claimable() {
        let issues = [issue(3, &[], &[]), issue(5, &[], &[])];
        let attempts = RefCell::new(Vec::new());
        // #3 sorts first and is claimed on the first try.
        let selection = claim_with(&issues, &[3, 5], &attempts);
        assert_eq!(selection.picked.unwrap().number, 3);
        assert_eq!(*attempts.borrow(), [3]);
    }

    #[test]
    fn claim_pick_falls_through_lost_races() {
        let issues = [issue(3, &[], &[]), issue(5, &[], &[])];
        let attempts = RefCell::new(Vec::new());
        // #3 is lost to another worker, so the next candidate is claimed.
        let selection = claim_with(&issues, &[5], &attempts);
        assert_eq!(selection.picked.unwrap().number, 5);
        assert_eq!(*attempts.borrow(), [3, 5]);
    }

    #[test]
    fn claim_pick_none_when_every_candidate_lost() {
        let issues = [issue(3, &[], &[]), issue(5, &[], &[])];
        let attempts = RefCell::new(Vec::new());
        let selection = claim_with(&issues, &[], &attempts);
        assert!(selection.picked.is_none());
        // Every candidate was tried exactly once.
        assert_eq!(*attempts.borrow(), [3, 5]);
    }

    #[test]
    fn claim_pick_does_not_claim_a_resumable_pick() {
        // A resumable winner is returned without claiming — its single-flight is
        // the launcher's lease, not the state-file claim.
        let issues = [issue(3, &[], &[])];
        let attempts = RefCell::new(Vec::new());
        let selection = claim_pick(
            &issues,
            "me",
            &[],
            false,
            |_| false,
            |_| IssueStatus::Resumable,
            |n| {
                attempts.borrow_mut().push(n);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(selection.picked.unwrap().number, 3);
        assert!(attempts.borrow().is_empty());
    }

    #[test]
    fn claim_pick_skips_caller_excluded_issues() {
        // #3 sorts first but the caller has given up on it this run, so #5 is
        // claimed instead and #3 is never attempted.
        let issues = [issue(3, &[], &[]), issue(5, &[], &[])];
        let attempts = RefCell::new(Vec::new());
        let selection = claim_pick(
            &issues,
            "me",
            &[],
            false,
            |n| n == 3,
            |_| IssueStatus::Fresh,
            |n| {
                attempts.borrow_mut().push(n);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(selection.picked.unwrap().number, 5);
        assert_eq!(*attempts.borrow(), [5]);
    }

    #[test]
    fn blocked_issue_is_skipped_and_reported() {
        // #1 would sort first but is blocked, so #2 wins.
        let issues = [blocked(1, 1), issue(2, &[], &[])];
        let selection = pick(&issues, "me", &[]);
        assert_eq!(selection.picked.unwrap().number, 2);
        assert_eq!(selection.skipped_blocked, [1]);
    }

    #[test]
    fn tracking_issue_is_skipped_and_reported() {
        let issues = [tracking(1, 2), issue(2, &[], &[])];
        let selection = pick(&issues, "me", &[]);
        assert_eq!(selection.picked.unwrap().number, 2);
        assert_eq!(selection.skipped_tracking, [1]);
    }

    #[test]
    fn closed_only_blockers_stay_pickable() {
        // `blocked_by` counts open blockers; closed-only blockers leave it at 0,
        // so the issue is not currently blocked.
        let issues = [blocked(1, 0)];
        let selection = pick(&issues, "me", &[]);
        assert_eq!(selection.picked.unwrap().number, 1);
        assert!(selection.skipped_blocked.is_empty());
    }

    #[test]
    fn live_takes_precedence_over_blocked_and_tracking() {
        // An issue that is live, blocked, and tracking is reported as live only.
        let mut both = blocked(1, 1);
        both.sub_issues_summary = SubIssuesSummary { total: 2 };
        let issues = [both, issue(2, &[], &[])];
        let selection = select(
            &issues,
            "me",
            &[],
            false,
            |_| false,
            |n| {
                if n == 1 {
                    IssueStatus::Live
                } else {
                    IssueStatus::Fresh
                }
            },
        );
        assert_eq!(selection.picked.unwrap().number, 2);
        assert_eq!(selection.skipped_live, [1]);
        assert!(selection.skipped_blocked.is_empty());
        assert!(selection.skipped_tracking.is_empty());
    }

    #[test]
    fn blocked_takes_precedence_over_tracking() {
        let mut both = blocked(1, 1);
        both.sub_issues_summary = SubIssuesSummary { total: 2 };
        let issues = [both];
        let selection = select(&issues, "me", &[], false, |_| false, |_| IssueStatus::Fresh);
        assert_eq!(selection.skipped_blocked, [1]);
        assert!(selection.skipped_tracking.is_empty());
    }

    /// `list_children` that returns nothing — for leaf-only child sets.
    fn no_children(_: u64) -> super::Result<Vec<IssueListing>> {
        Ok(Vec::new())
    }

    #[test]
    fn redirect_picks_best_ordered_child() {
        // Two leaf children; the lower number wins.
        let children = vec![issue(5, &[], &[]), issue(3, &[], &[])];
        let n = pick_workable_leaf(1, children, "me", &[], &no_children, &|_| false).unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn redirect_recurses_through_tracking_child() {
        // #1 → tracking child #2 → leaf grandchild #7.
        let top = vec![tracking(2, 1)];
        let list = |n: u64| {
            if n == 2 {
                Ok(vec![issue(7, &[], &[])])
            } else {
                Ok(Vec::new())
            }
        };
        let n = pick_workable_leaf(1, top, "me", &[], &list, &|_| false).unwrap();
        assert_eq!(n, 7);
    }

    #[test]
    fn redirect_prefers_a_started_child() {
        // #3 sorts first, but #5 is already started, so it is resumed.
        let children = vec![issue(3, &[], &[]), issue(5, &[], &[])];
        let n = pick_workable_leaf(1, children, "me", &[], &no_children, &|x| x == 5).unwrap();
        assert_eq!(n, 5);
    }

    #[test]
    fn redirect_resumes_a_started_child_even_if_blocked() {
        // A started leaf is resumed regardless of its block — work is underway.
        let children = vec![blocked(3, 1)];
        let n = pick_workable_leaf(1, children, "me", &[], &no_children, &|x| x == 3).unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn redirect_skips_blocked_fresh_children() {
        let children = vec![blocked(3, 1), issue(5, &[], &[])];
        let n = pick_workable_leaf(1, children, "me", &[], &no_children, &|_| false).unwrap();
        assert_eq!(n, 5);
    }

    #[test]
    fn redirect_ignores_closed_children() {
        // #3 is blocked and #5 is closed, so nothing is workable.
        let children = vec![blocked(3, 1), closed(issue(5, &[], &[]))];
        let result = pick_workable_leaf(1, children, "me", &[], &no_children, &|_| false);
        assert!(result.is_err());
    }

    #[test]
    fn redirect_errors_when_no_descendant_is_workable() {
        let children = vec![blocked(3, 1), blocked(4, 2)];
        let result = pick_workable_leaf(1, children, "me", &[], &no_children, &|_| false);
        assert!(result.is_err());
    }
}
