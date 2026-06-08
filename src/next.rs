use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::github::Conditional;
use crate::models::IssueListing;
use crate::{config, github, state, wait};

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
/// Eligible issues are the repo's open issues, excluding PRs, issues assigned
/// to someone other than the user, and issues that already have recorded ghwf
/// state (some session has already started them — `next` can't tell whether
/// that session is still live, so it never picks them; `ghwf work-on <n>`
/// resumes them explicitly). Claiming the winner atomically reserves it against
/// concurrent `next`/`next --wait` runs, and assigns it on GitHub.
pub fn pick() -> Result<u64> {
    let priority_labels = match config::find()? {
        Some(located) => located.config.priority_labels,
        None => Vec::new(),
    };
    let (owner, repo) = github::repo_or_cwd()?;
    let me = github::authenticated_user()?;
    let issues = github::list_open_issues(&owner, &repo)?;

    let selection = claim_pick(
        &issues,
        &me,
        &priority_labels,
        started_fn(&owner, &repo),
        |n| state::claim(&owner, &repo, n),
    )?;

    match announce_pick(&selection, &me, &priority_labels, &owner, &repo) {
        Some(number) => Ok(number),
        None => bail!(
            "no eligible open issues in {owner}/{repo}: every open issue is a PR, is assigned \
             to someone else, or is already started (resume those with `ghwf work-on <n>`)."
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
    let priority_labels = match config::find()? {
        Some(located) => located.config.priority_labels,
        None => Vec::new(),
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
                    started_fn(&owner, &repo),
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

/// The "already started" predicate for a repo: an issue is started when it has
/// any recorded state. A state file that exists but fails to parse still counts
/// — only its details are unreadable, not the fact that some session began it.
fn started_fn<'a>(owner: &'a str, repo: &'a str) -> impl Fn(u64) -> bool + 'a {
    move |number: u64| {
        state::load_if_exists(owner, repo, number)
            .map(|s| s.is_some())
            .unwrap_or(true)
    }
}

/// Select the best eligible issue and claim it, re-selecting when a claim is
/// lost to a concurrent worker. Returns the `Selection` whose `picked` issue
/// was successfully claimed, or one with `picked: None` when nothing remains.
///
/// `claim` returns `true` when this caller won the issue and `false` when
/// another session already holds it; a lost race excludes that issue and runs
/// selection again. Both predicates are injected so the loop is testable
/// without a filesystem or network.
fn claim_pick<'a>(
    issues: &'a [IssueListing],
    me: &str,
    priority_labels: &[String],
    already_started: impl Fn(u64) -> bool,
    mut claim: impl FnMut(u64) -> Result<bool>,
) -> Result<Selection<'a>> {
    // Issues lost to other workers this call, excluded from re-selection on top
    // of those `already_started` already reports.
    let mut lost: BTreeSet<u64> = BTreeSet::new();
    loop {
        let selection = select(issues, me, priority_labels, |n| {
            already_started(n) || lost.contains(&n)
        });
        let Some(picked) = selection.picked else {
            return Ok(selection);
        };
        if claim(picked.number)? {
            return Ok(selection);
        }
        lost.insert(picked.number);
    }
}

/// Report a claimed selection: print the skipped-started lines, assign the
/// picked issue to `me` on GitHub (best-effort — the claim already guarantees
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
    for number in &selection.skipped_started {
        println!("Skipping #{number} — already started; resume it with `ghwf work-on {number}`.");
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
    println!(
        "Picked #{} \"{}\" — {}.",
        issue.number,
        issue.title,
        rationale(issue, me, priority_labels)
    );
    Some(issue.number)
}

/// The outcome of [`select`]: the winning issue, if any, and the
/// already-started issues passed over (in listing order).
struct Selection<'a> {
    picked: Option<&'a IssueListing>,
    skipped_started: Vec<u64>,
}

/// Choose the most important eligible issue.
///
/// Sort key, ascending — first wins: issues assigned to `me` before
/// unassigned ones, then the best (earliest-in-list) priority label, then the
/// lowest issue number. PRs and issues assigned to someone else are excluded
/// outright; otherwise-eligible issues for which `already_started` returns
/// true are skipped and reported.
fn select<'a>(
    issues: &'a [IssueListing],
    me: &str,
    priority_labels: &[String],
    already_started: impl Fn(u64) -> bool,
) -> Selection<'a> {
    let mut skipped_started = Vec::new();
    let mut candidates: Vec<&IssueListing> = Vec::new();
    for issue in issues {
        if issue.pull_request.is_some() {
            continue;
        }
        if !issue.assignees.is_empty() && !assigned_to(issue, me) {
            continue;
        }
        if already_started(issue.number) {
            skipped_started.push(issue.number);
            continue;
        }
        candidates.push(issue);
    }
    candidates.sort_by_key(|issue| {
        (
            !assigned_to(issue, me),
            label_rank(issue, priority_labels),
            issue.number,
        )
    });
    Selection {
        picked: candidates.first().copied(),
        skipped_started,
    }
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use super::{claim_pick, select, Selection};
    use crate::models::{IssueListing, Label, User};

    /// An open issue with the given assignees and labels.
    fn issue(number: u64, assignees: &[&str], labels: &[&str]) -> IssueListing {
        IssueListing {
            number,
            title: format!("issue {number}"),
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
        }
    }

    /// A PR entry, as the issues listing reports one.
    fn pr(number: u64) -> IssueListing {
        IssueListing {
            pull_request: Some(serde_json::json!({})),
            ..issue(number, &[], &[])
        }
    }

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// Run `select` with nothing already started.
    fn pick<'a>(issues: &'a [IssueListing], me: &str, priority_labels: &[String]) -> Selection<'a> {
        select(issues, me, priority_labels, |_| false)
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
    fn prs_are_excluded() {
        let issues = [pr(1), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn started_issues_are_skipped_and_reported() {
        let issues = [
            issue(1, &["me"], &[]),
            // Excluded outright, so not reported as skipped.
            issue(2, &["other"], &[]),
            issue(3, &[], &[]),
        ];
        let selection = select(&issues, "me", &[], |n| n == 1);
        assert_eq!(selection.picked.unwrap().number, 3);
        assert_eq!(selection.skipped_started, [1]);
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
        assert!(selection.skipped_started.is_empty());
    }

    /// Run `claim_pick` with nothing pre-started, claiming the numbers in
    /// `won` and losing every other. Records the claim attempts in order.
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
            |_| false,
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
}
