use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{store, worktree};

/// Branch/worktree name and plan-file slug derived from an issue.
///
/// Returns `(branch, slug)` where `branch` uses underscores (`issue_<n>_<slug>`,
/// per the org convention) and `slug` is the kebab-case form for the plan
/// filename (`plans/<n>-<slug>.md`).
///
/// `prefix` qualifies the branch for issues that live in a *foreign* repo, so
/// two same-numbered issues in different repos don't collide on one branch (and
/// thus one worktree dir). `None` — the common case, a main-repo issue — leaves
/// the branch unqualified; `Some(p)` prepends a sanitised `p`. The slug is never
/// prefixed: the plan file lives inside the per-branch worktree, so it can't
/// collide across issues.
pub fn branch_and_slug(prefix: Option<&str>, number: u64, title: &str) -> (String, String) {
    let words = slug_words(title);
    let core = format!("issue_{number}_{}", words.join("_"));
    let branch = match prefix.map(slug_words) {
        // A prefix that sanitises to nothing (e.g. all punctuation) is dropped
        // rather than producing a leading underscore.
        Some(parts) if !parts.is_empty() => format!("{}_{core}", parts.join("_")),
        _ => core,
    };
    let slug = words.join("-");
    (branch, slug)
}

/// Split a title into lowercased alphanumeric words, dropping all punctuation.
fn slug_words(title: &str) -> Vec<String> {
    title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// A workflow-advancing command a user posts at the start of a comment line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Directive {
    ApprovePrePlan,
    ApprovePlan,
    // Retired: the implement phase now advances when the user marks the draft
    // PR ready for review. Still recognised so its retirement can be explained
    // rather than the comment being silently ignored.
    ApproveImplementation,
    // The retired generic command; recognised for the same reason.
    Proceed,
}

/// Every recognised command spelling. No command is a prefix of another, so
/// match order doesn't matter beyond the word-boundary rule.
const DIRECTIVE_COMMANDS: &[(&str, Directive)] = &[
    ("/approve-pre-plan", Directive::ApprovePrePlan),
    // Alias.
    ("/approve-preplan", Directive::ApprovePrePlan),
    ("/approve-plan", Directive::ApprovePlan),
    ("/approve-implementation", Directive::ApproveImplementation),
    ("/proceed", Directive::Proceed),
];

impl Directive {
    /// The phase this directive approves (i.e. advances out of), or `None` for
    /// a retired command.
    pub fn approves(self) -> Option<Phase> {
        match self {
            Directive::ApprovePrePlan => Some(Phase::PrePlan),
            Directive::ApprovePlan => Some(Phase::PrepAndPlan),
            Directive::ApproveImplementation => None,
            Directive::Proceed => None,
        }
    }

    /// The canonical spelling, for reporting.
    pub fn command(self) -> &'static str {
        match self {
            Directive::ApprovePrePlan => "/approve-pre-plan",
            Directive::ApprovePlan => "/approve-plan",
            Directive::ApproveImplementation => "/approve-implementation",
            Directive::Proceed => "/proceed",
        }
    }
}

/// The first directive in `body`, if any.
///
/// A command must begin a line and be followed by end-of-line or whitespace,
/// so `/approve-plan now` counts but `/approve-plans` and conversational
/// mid-sentence mentions do not.
pub fn parse_directive(body: &str) -> Option<Directive> {
    body.lines().find_map(|line| {
        DIRECTIVE_COMMANDS.iter().find_map(|&(command, directive)| {
            line.strip_prefix(command)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
                .then_some(directive)
        })
    })
}

/// The approval a ghwf-authored comment prompts for: the *last* word-bounded
/// mention of an approval command anywhere in `body`, or `None` when no
/// command is mentioned.
///
/// Unlike `parse_directive`, mentions count mid-line (hand-off prose backticks
/// them). Last-mention-wins makes ghwf comments self-describing 👍 targets:
/// hand-off comments end with the "comment `/approve-X`" prompt, and misfire
/// notes end with "the command that advances it is `/approve-X`". Retired
/// commands never map — a 👍 can only mean a live approval.
pub fn parse_prompted_directive(body: &str) -> Option<Directive> {
    let mut last: Option<(usize, Directive)> = None;
    for &(command, directive) in DIRECTIVE_COMMANDS {
        if directive.approves().is_none() {
            continue;
        }
        for (idx, _) in body.match_indices(command) {
            // Word boundaries: the preceding character must not be
            // alphanumeric, and the following one must not extend the command
            // word (so `/approve-plans` is not a `/approve-plan` mention).
            let before_ok = body[..idx]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric());
            let after_ok = body[idx + command.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '-');
            if !before_ok || !after_ok {
                continue;
            }
            if last.is_none_or(|(seen, _)| idx > seen) {
                last = Some((idx, directive));
            }
        }
    }
    last.map(|(_, directive)| directive)
}

/// A phase of work on an issue. Named in the imperative mood. Variant order is
/// workflow order, so the derived `Ord` lets directives be classified as stale
/// (approving an earlier phase) or premature (a later one).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    #[default]
    PrePlan,
    PrepAndPlan,
    Implement,
    Review,
    /// Terminal: the PR was merged, so the workflow is complete. Reached by the
    /// merge itself, not by an approval — kept last so the derived `Ord` ranks
    /// it above every working phase.
    Finished,
}

impl Phase {
    /// The phase that follows this one, or `None` if this is the terminal phase.
    pub fn next(self) -> Option<Phase> {
        match self {
            Phase::PrePlan => Some(Phase::PrepAndPlan),
            Phase::PrepAndPlan => Some(Phase::Implement),
            Phase::Implement => Some(Phase::Review),
            Phase::Review => None,
            Phase::Finished => None,
        }
    }

    /// Human-readable label, matching the on-disk serialization.
    pub fn label(self) -> &'static str {
        match self {
            Phase::PrePlan => "pre-plan",
            Phase::PrepAndPlan => "prep-and-plan",
            Phase::Implement => "implement",
            Phase::Review => "review",
            Phase::Finished => "finished",
        }
    }

    /// The canonical command that approves this phase (advancing to the next),
    /// or `None` when no command advances it — implement advances when the
    /// user marks the draft PR ready for review, and review is terminal.
    pub fn approval_command(self) -> Option<&'static str> {
        match self {
            Phase::PrePlan => Some("/approve-pre-plan"),
            Phase::PrepAndPlan => Some("/approve-plan"),
            Phase::Implement => None,
            Phase::Review => None,
            Phase::Finished => None,
        }
    }
}

/// Who the workflow is currently waiting for, orthogonal to the phase. Flips
/// far more often than the phase does: every hand-off and every round of user
/// feedback moves the ball.
//
// The shared `WaitingOn` prefix is deliberate: the kebab-case serializations
// (`waiting-on-user`, …) are the on-disk format and the config keys.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Attention {
    /// Claude has handed off; the user owes an approval, an answer, or a review.
    WaitingOnUser,
    /// Claude has instructions to act on. The default: a `work-on` run always
    /// leaves Claude with something to do.
    #[default]
    WaitingOnClaude,
    /// ghwf's own machinery is doing slow work (worktree/PR prep).
    WaitingOnGhwf,
}

/// How an issue's PR left the open state, concluding (or halting) the workflow.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrOutcome {
    Merged,
    Closed,
}

/// The outcome a fetched PR implies: merged, closed without merging, or
/// `None` while it is still open.
pub fn pr_outcome(pr: &crate::models::PullRequest) -> Option<PrOutcome> {
    if pr.merged {
        Some(PrOutcome::Merged)
    } else if pr.state != "open" {
        Some(PrOutcome::Closed)
    } else {
        None
    }
}

/// Per-issue workflow state. Scoped to the issue (not a session), since the phase
/// reflects the progress of the work across sessions.
#[derive(Default, Serialize, Deserialize)]
pub struct IssueState {
    pub phase: Phase,
    // Who the workflow is waiting for. Defaults to waiting-on-claude for
    // pre-existing state files: a `work-on` run always leaves Claude with
    // instructions.
    #[serde(default)]
    pub attention: Attention,
    // What the last label sync applied, so the steady-state loop skips the
    // API calls entirely. Absent until a sync has run (or when labels are
    // not configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels_synced: Option<LabelSyncRecord>,
    // Ids of comments whose directive has already been acted on. Comment ids
    // are globally unique across the issue and PR conversation threads, so one
    // set dedupes both sources.
    pub consumed_directives: BTreeSet<u64>,
    // Ids of 👍 reactions already acted on. Reaction ids live in their own id
    // namespace, so they get their own set rather than sharing
    // `consumed_directives`. Removing a consumed reaction undoes nothing.
    #[serde(default)]
    pub consumed_reactions: BTreeSet<u64>,
    // Whether the one-time intro status update has been posted to the issue.
    // Defaults to false for pre-existing state files, so issues already
    // mid-flight get one phase-aware intro on their next `work-on`.
    #[serde(default)]
    pub intro_posted: bool,
    // Populated once the prep-and-plan phase begins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prep: Option<PrepState>,
    // What the last `work-on` run observed, for `wait` to poll against.
    // Absent until a `work-on` run records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitState>,
    // Whether the issue was closed the last time `work-on` fetched it. The
    // Stop hook reads this to let a session end once the workflow is done.
    #[serde(default)]
    pub issue_closed: bool,
    // How the PR had left the open state the last time `work-on` fetched it,
    // or `None` while it was still open (or no PR exists). Recomputed every
    // run rather than latched, so a closed-then-reopened PR resumes the loop.
    // The Stop hook reads this too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_outcome: Option<PrOutcome>,
    // Consecutive Stop-hook nudges issued without anything new arriving.
    // Incremented by `ghwf claude-stop-hook` each time it blocks a stop; reset
    // by `work-on` whenever it observes new activity. The hook stops nudging
    // at a cap, so a stuck session isn't fought forever.
    #[serde(default)]
    pub stop_nudges: u32,
    // Set by the Notification hook when a launched session goes idle or parks on
    // a permission prompt. Read (and cleared) by the supervisor to drive
    // recovery, and cleared by `work-on` whenever it observes new activity, so a
    // session that's working again starts clean. `None` means no outstanding
    // alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_alert: Option<SessionAlert>,
    // The most recent comment ghwf itself posted to either thread. Lives here
    // rather than on `WaitState` because `work-on` rebuilds that wholesale
    // when recording a baseline, and a status update posted mid-run must
    // survive. Feed-mode self-calibration in `wait` reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_posted: Option<PostedRef>,
}

impl IssueState {
    /// Whether the workflow has concluded: the issue was closed, or its PR left
    /// the open state (merged or closed). Either way there is nothing left to
    /// wait for. The Stop hook uses this to let a session end, and the
    /// `forever` supervisor uses it to know when to bring a session down.
    pub fn is_concluded(&self) -> bool {
        self.issue_closed || self.pr_outcome.is_some()
    }
}

/// A reference to a comment ghwf posted, for feed-lag self-calibration.
#[derive(Clone, Serialize, Deserialize)]
pub struct PostedRef {
    pub id: u64,
    pub created_at: String,
}

/// What kind of stuck state a [`SessionAlert`] reports — distinguished because
/// the supervisor treats them differently (see the recovery policy).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlertKind {
    /// Claude went idle, waiting for input (Notification `idle_prompt`).
    Idle,
    /// A permission dialog appeared (Notification `permission_prompt`).
    Permission,
}

/// A signal recorded by the Notification hook that a launched session has gone
/// idle or parked on a permission prompt. The supervisor reads it to drive
/// recovery; `work-on` clears it once the session is working again.
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionAlert {
    /// Which stuck state fired.
    pub kind: AlertKind,
    /// The session the alert is about, so a stale signal left by a prior
    /// session is ignored.
    pub session_id: String,
    /// Epoch seconds the episode began. Held stable while the same kind of
    /// alert for the same session keeps re-firing, so the supervisor's grace
    /// window measures from when the session first got stuck, not the latest
    /// notification.
    pub at: u64,
}

/// What the last label sync applied: the inputs the desired label set is
/// computed from. A sync is skipped when these all match the current state.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelSyncRecord {
    pub phase: Phase,
    // `None` once the workflow has concluded (no attention label applies).
    pub attention: Option<Attention>,
    // The PR that was labelled alongside the issue, if any. A PR appearing
    // later forces a re-sync.
    pub pr_number: Option<u64>,
}

/// What `work-on` last observed on the issue and its PR, recorded for `wait`
/// to poll against.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct WaitState {
    // Watermark for `?since=` polling: the max `updated_at` across everything
    // the recording run fetched. Server-side timestamps, so local clock skew
    // can't lose activity; ISO-8601 Zulu strings compare lexicographically.
    pub since: String,
    // Content hash over the issue's title, body, and state, for detecting
    // edits the comment lists can't show.
    pub issue_fingerprint: String,
    // Comment id -> body hash for both conversation threads merged (ids are
    // globally unique across them).
    pub comments: BTreeMap<u64, String>,
    // Inline review comment id -> body hash.
    pub review_comments: BTreeMap<u64, String>,
    // Endpoint key -> last ETag, updated by `wait` as it polls. The events
    // feed's ETag lives here too, under the key `events`.
    #[serde(default)]
    pub etags: BTreeMap<String, String>,
    // Thread key (`issue` / `pr`) -> the approval-prompting comment whose 👍
    // reactions `wait` watches. Only the latest prompt per thread is watched;
    // a 👍 on an older prompt is still honoured on the next `work-on`.
    #[serde(default)]
    pub reaction_watches: BTreeMap<String, ReactionWatch>,
    // Thread key (`issue` / `pr`) -> the outstanding `ask` options comment
    // whose submit checkbox `wait` polls. Recorded only while that box is
    // unticked, so any ticked submit on the watched comment wakes; `work-on`
    // re-derives it from the comments each run. Latest outstanding per thread.
    #[serde(default)]
    pub options_watches: BTreeMap<String, OptionsWatch>,
    // Whether the PR was a draft when the baseline was recorded. A flip in
    // either direction wakes the wait: ready-for-review advances the
    // implement phase. Absent when no PR exists (or for old state files).
    #[serde(default)]
    pub pr_draft: Option<bool>,
    // The branch's sync state against its base as of `wait`'s last probe. Tracks
    // the edge across `wait` invocations so a fresh advance (clean or
    // conflicting) wakes the session once, not every cycle; `work-on` seeds it
    // from the verdict it surfaced. Defaults to up-to-date for old state files.
    #[serde(default)]
    pub last_base_sync: crate::implement::BaseSync,
}

/// A comment whose 👍 reactions `wait` polls, with the reaction ids already
/// seen (so only a fresh 👍 wakes).
#[derive(Clone, Serialize, Deserialize)]
pub struct ReactionWatch {
    pub comment_id: u64,
    pub plus_one_ids: BTreeSet<u64>,
}

/// A ghwf-posted `ask` options comment whose submit checkbox `wait` polls.
/// Recorded only while the box is unticked (an outstanding question), so any
/// ticked submit on the comment is a wake. No baseline beyond the id is needed:
/// the submit box can only be ticked once before `work-on` rewrites it away.
#[derive(Clone, Serialize, Deserialize)]
pub struct OptionsWatch {
    pub comment_id: u64,
}

/// The fingerprint recorded in `WaitState::issue_fingerprint`.
pub fn issue_fingerprint(title: &str, body: Option<&str>, state: &str) -> String {
    // A separator no GitHub field can contain, so field boundaries can't be
    // confused.
    store::content_hash(&format!("{title}\u{0}{}\u{0}{state}", body.unwrap_or("")))
}

/// Advance a `since` watermark to `candidate` when it is later. GitHub's
/// ISO-8601 Zulu timestamps compare lexicographically.
pub fn fold_since(since: &mut String, candidate: &str) {
    if candidate > since.as_str() {
        *since = candidate.to_string();
    }
}

/// State accumulated during the prep-and-plan phase for an issue.
#[derive(Default, Serialize, Deserialize)]
pub struct PrepState {
    // Whether this issue is being worked without a dedicated branch/worktree/PR.
    pub no_branch: bool,
    // The Claude session most recently seen running `work-on` inside this
    // issue's worktree. The outside-Claude launcher resumes it by id.
    pub worktree_session_id: Option<String>,
    // The branch name, once the worktree has been created.
    pub branch: Option<String>,
    // The worktree path, once created.
    pub worktree_path: Option<PathBuf>,
    // The draft PR's number, once opened.
    pub pr_number: Option<u64>,
}

/// Path to the per-issue state file under the ghwf data dir.
fn state_path(owner: &str, repo: &str, number: u64) -> Result<PathBuf> {
    Ok(store::data_dir()?
        .join("issues")
        .join(owner)
        .join(repo)
        .join(format!("{number}.json")))
}

/// Load the state for an issue, or a fresh default if none exists.
pub fn load(owner: &str, repo: &str, number: u64) -> Result<IssueState> {
    Ok(load_if_exists(owner, repo, number)?.unwrap_or_default())
}

/// Load the state for an issue, or `None` if none has been recorded.
pub fn load_if_exists(owner: &str, repo: &str, number: u64) -> Result<Option<IssueState>> {
    read_state(&state_path(owner, repo, number)?)
}

/// Find the workflow issue a conversation thread belongs to: `number` itself
/// when it has recorded state, otherwise the issue whose recorded PR is
/// `number` (a comment posted on the PR thread). `None` when neither matches.
pub fn find_workflow_issue(
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<Option<(u64, IssueState)>> {
    if let Some(state) = load_if_exists(owner, repo, number)? {
        return Ok(Some((number, state)));
    }
    let dir = store::data_dir()?.join("issues").join(owner).join(repo);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(issue_number) = stem.parse::<u64>() else {
            continue;
        };
        let Some(state) = load_if_exists(owner, repo, issue_number)? else {
            continue;
        };
        if state.prep.as_ref().and_then(|p| p.pr_number) == Some(number) {
            return Ok(Some((issue_number, state)));
        }
    }
    Ok(None)
}

/// Find the issue whose recorded worktree contains `dir`, walking every
/// `<owner>/<repo>/<number>.json` under `issues_root` (the data dir's `issues`
/// directory; parameterized for tests). Returns `(owner, repo, number)`, or
/// `None` when no recorded worktree contains `dir`. Worktrees are per-issue,
/// so multiple matches shouldn't happen; if they do, the first wins with a
/// warning.
pub fn find_issue_for_dir(issues_root: &Path, dir: &Path) -> Result<Option<(String, String, u64)>> {
    let mut found: Option<(String, String, u64)> = None;
    for (owner, repo, number, path) in state_files(issues_root) {
        let json = match fs::read_to_string(&path) {
            Ok(json) => json,
            Err(_) => continue,
        };
        let state: IssueState = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse issue state {}", path.display()))?;
        let Some(worktree_path) = state.prep.as_ref().and_then(|p| p.worktree_path.as_ref()) else {
            continue;
        };
        if !worktree::is_inside(dir, worktree_path) {
            continue;
        }
        match &found {
            None => found = Some((owner, repo, number)),
            Some((o, r, n)) => eprintln!(
                "warning: `{}` is inside the worktrees of both {o}/{r}#{n} and \
                 {owner}/{repo}#{number}; using the former.",
                dir.display()
            ),
        }
    }
    Ok(found)
}

/// Every `(owner, repo, number, path)` state file under `issues_root`, in
/// directory-walk order. Unreadable directories and non-numeric filenames are
/// skipped.
fn state_files(issues_root: &Path) -> Vec<(String, String, u64, PathBuf)> {
    let mut files = Vec::new();
    for owner_entry in fs::read_dir(issues_root).into_iter().flatten().flatten() {
        let owner_path = owner_entry.path();
        let Some(owner) = owner_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let owner = owner.to_string();
        for repo_entry in fs::read_dir(&owner_path).into_iter().flatten().flatten() {
            let repo_path = repo_entry.path();
            let Some(repo) = repo_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let repo = repo.to_string();
            for file_entry in fs::read_dir(&repo_path).into_iter().flatten().flatten() {
                let path = file_entry.path();
                let Some(number) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
                else {
                    continue;
                };
                files.push((owner.clone(), repo.clone(), number, path));
            }
        }
    }
    files
}

/// Find the issue whose recorded prep branch is `branch`, scanning the repo's
/// state files. `None` when no state file records that branch.
pub fn find_issue_for_branch(owner: &str, repo: &str, branch: &str) -> Result<Option<u64>> {
    let dir = store::data_dir()?.join("issues").join(owner).join(repo);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(number) = stem.parse::<u64>() else {
            continue;
        };
        let Some(state) = load_if_exists(owner, repo, number)? else {
            continue;
        };
        if state.prep.as_ref().and_then(|p| p.branch.as_deref()) == Some(branch) {
            return Ok(Some(number));
        }
    }
    Ok(None)
}

/// Every issue number with a recorded state file under `owner/repo`, in
/// directory-walk order. An absent or unreadable directory yields an empty
/// list; non-numeric filenames (e.g. the `<n>.lease.json` siblings) are
/// skipped.
pub fn issue_numbers(owner: &str, repo: &str) -> Vec<u64> {
    let Ok(dir) = store::data_dir().map(|d| d.join("issues").join(owner).join(repo)) else {
        return Vec::new();
    };
    let mut numbers = Vec::new();
    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        // A `<n>.lease.json` sibling has the stem `<n>.lease`, which doesn't
        // parse as a number, so it's skipped here.
        if let Some(number) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
        {
            numbers.push(number);
        }
    }
    numbers
}

/// Remove an issue's state file. Absence is not an error.
pub fn delete(owner: &str, repo: &str, number: u64) -> Result<()> {
    let path = state_path(owner, repo, number)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("failed to remove issue state {}", path.display()))
        }
    }
}

/// Atomically record an issue as started, claiming it against concurrent local
/// workers. Returns `true` when this call created the state file, `false` when
/// some session already holds state for the issue.
///
/// The per-issue state file is already the "started" marker that makes `next`
/// skip an issue; creating it exclusively turns that marker into a claim. The
/// atomicity is the filesystem's `O_EXCL`, so this only coordinates workers on
/// one machine — which is the whole contention scope we need.
pub fn claim(owner: &str, repo: &str, number: u64) -> Result<bool> {
    let path = state_path(owner, repo, number)?;
    let dir = path
        .parent()
        .expect("state path always has a parent directory");
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    claim_file(&path)
}

/// Exclusively create `path` and seed it with default state, so the claimed
/// issue's file parses everywhere it is later read. Returns `false` without
/// touching the file when it already exists. Split from [`claim`] so tests can
/// drive a concrete path.
fn claim_file(path: &Path) -> Result<bool> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => {
            let json = serde_json::to_string_pretty(&IssueState::default())
                .context("failed to serialize claimed issue state")?;
            // A single write is enough; the file was created empty and we hold
            // the only handle.
            (&file).write_all(json.as_bytes()).with_context(|| {
                format!("failed to write claimed issue state {}", path.display())
            })?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => {
            Err(err).with_context(|| format!("failed to claim issue state {}", path.display()))
        }
    }
}

/// Persist the state for an issue.
///
/// The write is atomic (temp + rename) and serialised against concurrent
/// [`mutate`] calls through the per-issue lock, so a Stop/Notification hook
/// bumping its own field can never interleave with this write and lose it (or
/// clobber the fields written here).
pub fn save(owner: &str, repo: &str, number: u64, state: &IssueState) -> Result<()> {
    let path = state_path(owner, repo, number)?;
    let dir = path
        .parent()
        .expect("state path always has a parent directory");
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(state).context("failed to serialize issue state")?;
    with_issue_lock(&path, || {
        store::atomic_write(&path, json.as_bytes())
            .with_context(|| format!("failed to write issue state {}", path.display()))
    })
}

/// Read-modify-write an issue's state under the per-issue lock, so a narrow
/// writer (a Stop/Notification hook, the supervisor) changes only the field it
/// owns without clobbering a concurrent [`save`].
///
/// The latest file is re-read *inside* the lock, `f` is applied to it, and the
/// result is written atomically — all while holding the lock. Because [`save`]
/// also takes the same lock for its write, the two critical sections can't
/// interleave: this either runs fully before a concurrent save (and that save's
/// snapshot wins, dropping only this call's narrow field) or fully after it
/// (re-reading the saved snapshot, so the save's phase/consumed-sets survive).
///
/// Does nothing when no state file exists yet: a narrow writer has nothing to
/// update before the issue has been claimed.
pub fn mutate(owner: &str, repo: &str, number: u64, f: impl FnOnce(&mut IssueState)) -> Result<()> {
    let path = state_path(owner, repo, number)?;
    let dir = path
        .parent()
        .expect("state path always has a parent directory");
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    mutate_at(&path, f)
}

/// Read-modify-write the state at a concrete `path`. Split from [`mutate`] so
/// tests can drive a scratch path without a real data dir.
fn mutate_at(path: &Path, f: impl FnOnce(&mut IssueState)) -> Result<()> {
    with_issue_lock(path, || {
        let Some(mut state) = read_state(path)? else {
            return Ok(());
        };
        f(&mut state);
        let json =
            serde_json::to_string_pretty(&state).context("failed to serialize issue state")?;
        store::atomic_write(path, json.as_bytes())
            .with_context(|| format!("failed to write issue state {}", path.display()))
    })
}

/// Read and parse the state file at `path`, or `None` when it doesn't exist.
/// The path-based parse shared by [`load_if_exists`] and [`mutate`]'s
/// re-read-under-lock.
fn read_state(path: &Path) -> Result<Option<IssueState>> {
    match fs::read_to_string(path) {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .with_context(|| format!("failed to parse issue state {}", path.display())),
        Err(_) => Ok(None),
    }
}

/// Hold an exclusive lock on an issue's lock file for the duration of `f`.
///
/// `state_file` is the issue's `<n>.json`; the lock lives on its `<n>.json.lock`
/// sibling, which is never renamed (unlike the state file, replaced on every
/// atomic write), so the whole-machine lock lives on a stable inode. The
/// `.json.lock` suffix leaves the stem `<n>.json`, which doesn't parse as a
/// number, so the state-dir scanners ([`state_files`], [`issue_numbers`], …)
/// skip it just as they skip the `<n>.lease.json` lease sibling.
///
/// Uses the stdlib `File::lock` (blocking, exclusive); the lock releases when
/// the file handle drops at the end of this function. A failure to open or lock
/// the file is surfaced rather than silently skipped, so a caller's `Result`
/// reflects it.
fn with_issue_lock<T>(state_file: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock = lock_path(state_file);
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock)
        .with_context(|| format!("failed to open lock file {}", lock.display()))?;
    file.lock()
        .with_context(|| format!("failed to lock {}", lock.display()))?;
    let out = f();
    // The lock releases on drop; dropping explicitly keeps the ordering obvious.
    drop(file);
    out
}

/// The lock-file sibling for a state file: `<n>.json` → `<n>.json.lock`.
/// Appends (rather than replacing the extension) so the stem stays `<n>.json`
/// and the state-dir scanners skip it.
fn lock_path(state_file: &Path) -> PathBuf {
    let mut name = state_file.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Delete an issue's state file only when it represents a *bare claim* — a
/// reservation with nothing durable behind it (no worktree, no recorded
/// session) and not yet concluded. Used to undo a claim a launch never managed
/// to act on, throwing the issue back to the pool as `Fresh` instead of leaving
/// it locked. An issue carrying real progress, or a concluded one, is left
/// untouched.
pub fn release_if_unstarted(owner: &str, repo: &str, number: u64) -> Result<()> {
    match load_if_exists(owner, repo, number)? {
        Some(state) if is_unstarted(&state) => delete(owner, repo, number),
        _ => Ok(()),
    }
}

/// Whether a state file is a bare claim with no durable work behind it.
fn is_unstarted(state: &IssueState) -> bool {
    if state.is_concluded() {
        return false;
    }
    // A deliberate "needs you" park (e.g. a `Model:` line the launcher refused
    // to start on) is not a bare claim to reclaim, even with no worktree yet —
    // discarding it would lose the signal and re-offer the issue as fresh.
    if !matches!(state.attention, Attention::WaitingOnClaude) {
        return false;
    }
    match &state.prep {
        None => true,
        Some(prep) => prep.worktree_path.is_none() && prep.worktree_session_id.is_none(),
    }
}

/// How often a held lease's heartbeat is refreshed.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// A lease whose heartbeat is older than this is treated as stale even if its
/// pid still resolves — the guard against a crashed launcher whose pid has been
/// recycled by an unrelated process. Several heartbeat intervals, so a busy
/// machine missing a few beats doesn't trigger a false reclaim.
const LEASE_TTL: u64 = 120;

/// A liveness record for the launcher process currently running an issue's
/// session. Written to a sibling of the state file (`<n>.lease.json`) so it
/// never races the in-session `work-on`'s rewrites of `<n>.json`, and so the
/// directory walkers (which parse a numeric file stem) skip it: the stem
/// `<n>.lease` is not a number.
#[derive(Serialize, Deserialize)]
pub struct SessionLease {
    /// The launcher process id, probed with `kill(pid, 0)` for liveness.
    pub pid: u32,
    /// Epoch seconds of the last heartbeat; refreshed by the holding guard.
    pub heartbeat: u64,
}

/// Whether an issue currently has a live launcher session.
pub enum Liveness {
    Live,
    NotLive,
}

/// Epoch seconds now, or 0 if the clock is somehow before the epoch.
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The current time as Unix epoch milliseconds, the resolution the
/// graceful-shutdown flag is stamped with. Millisecond granularity (rather than
/// the seconds [`now_epoch`] uses) keeps a freshly started worker from ever
/// mistaking a same-second stale flag for a fresh stop request.
pub fn now_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write `at_millis` as the flag's contents under `dir`, recording when a stop
/// was requested. Split out from [`request_stop`] so it's testable without the
/// real data dir.
fn write_stop_flag_in(dir: &Path, at_millis: u64) -> Result<()> {
    let path = dir.join("forever-stop");
    fs::write(&path, at_millis.to_string())
        .with_context(|| format!("failed to write stop flag {}", path.display()))
}

/// Read the stop flag under `dir`, returning the request timestamp it holds, or
/// `None` when the flag is absent or its contents don't parse. Split out from
/// [`stop_requested_since`] so it's testable without the real data dir.
fn stop_flag_at(dir: &Path) -> Option<u64> {
    let contents = fs::read_to_string(dir.join("forever-stop")).ok()?;
    contents.trim().parse().ok()
}

/// Request a graceful shutdown of every running `forever` worker by stamping the
/// flag with the current time. A worker honours it only if it started before
/// this request (see [`stop_requested_since`]), so a worker launched afterwards
/// ignores the now-stale flag and the file can be left in place.
pub fn request_stop() -> Result<()> {
    write_stop_flag_in(&store::data_dir()?, now_epoch_millis())
}

/// Whether a graceful shutdown has been requested since a worker that started at
/// `worker_start_millis`. True only when the flag exists and its timestamp is
/// strictly newer than the worker's start, so a stale flag from before this
/// worker began never stops it. An absent or unparseable flag reads as no stop.
pub fn stop_requested_since(worker_start_millis: u64) -> Result<bool> {
    Ok(stop_flag_at(&store::data_dir()?).is_some_and(|at| at > worker_start_millis))
}

/// Whether a lease is live: its process still exists and its heartbeat is
/// recent. The pid check catches a crashed launcher immediately; the heartbeat
/// bound catches the rare recycled-pid case after [`LEASE_TTL`].
fn is_live(lease: &SessionLease, now: u64) -> bool {
    process_alive(lease.pid) && now.saturating_sub(lease.heartbeat) <= LEASE_TTL
}

/// Whether a process with `pid` currently exists.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // `kill(pid, 0)` probes without signalling: 0 means it exists and we may
    // signal it; EPERM means it exists but isn't ours; ESRCH means gone.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Without a portable probe, fall back to heartbeat staleness alone.
#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

/// Path to an issue's lease file, a sibling of its state file.
fn lease_path(owner: &str, repo: &str, number: u64) -> Result<PathBuf> {
    Ok(store::data_dir()?
        .join("issues")
        .join(owner)
        .join(repo)
        .join(format!("{number}.lease.json")))
}

/// Read a lease file, or `None` when it's absent or unparseable.
fn load_lease(path: &Path) -> Option<SessionLease> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Whether an issue currently has a live session, for selection. An absent or
/// stale lease reads as [`Liveness::NotLive`].
pub fn lease_liveness(owner: &str, repo: &str, number: u64) -> Liveness {
    let Ok(path) = lease_path(owner, repo, number) else {
        return Liveness::NotLive;
    };
    match load_lease(&path) {
        Some(lease) if is_live(&lease, now_epoch()) => Liveness::Live,
        _ => Liveness::NotLive,
    }
}

/// Whether an issue's lease file exists but is not live — the signature of a
/// crashed or killed session, as distinct from an absent lease (no session has
/// started, or one exited cleanly and removed it). An absent or unreadable
/// lease reads as `false`: only a lease left behind by a dead process counts.
pub fn lease_is_stale(owner: &str, repo: &str, number: u64) -> bool {
    let Ok(path) = lease_path(owner, repo, number) else {
        return false;
    };
    lease_is_stale_at(&path, now_epoch())
}

/// Whether the lease at a concrete `path` exists but is not live as of `now`.
/// Split from [`lease_is_stale`] so tests can drive a scratch path.
fn lease_is_stale_at(path: &Path, now: u64) -> bool {
    match load_lease(path) {
        Some(lease) => !is_live(&lease, now),
        None => false,
    }
}

/// Exclusively create a lease file and write `lease` into it in one shot (the
/// content lands within the `O_EXCL` create, so there's no empty window another
/// acquirer could mistake for a stale lease). Returns `false` without touching
/// the file when it already exists.
fn create_lease_exclusive(path: &Path, lease: &SessionLease) -> Result<bool> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => {
            let json = serde_json::to_string(lease).context("failed to serialize session lease")?;
            (&file)
                .write_all(json.as_bytes())
                .with_context(|| format!("failed to write session lease {}", path.display()))?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => {
            Err(err).with_context(|| format!("failed to create session lease {}", path.display()))
        }
    }
}

/// Acquire the session lease for an issue, returning a guard that heartbeats it
/// and removes it on drop, or `None` when a live session already holds it.
///
/// Creates the lease exclusively; if one already exists, reclaims it only when
/// it is not live (its process is gone, or its heartbeat has aged past
/// [`LEASE_TTL`]). The exclusive create is the serialisation point, so two
/// workers reclaiming the same stale lease can't both win — the loser sees the
/// winner's fresh lease and backs off.
pub fn acquire_lease(owner: &str, repo: &str, number: u64) -> Result<Option<LeaseGuard>> {
    let path = lease_path(owner, repo, number)?;
    let dir = path
        .parent()
        .expect("lease path always has a parent directory");
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    acquire_lease_at(path)
}

/// Acquire the lease at a concrete `path`. Split from [`acquire_lease`] so tests
/// can drive a scratch path without a real data dir.
fn acquire_lease_at(path: PathBuf) -> Result<Option<LeaseGuard>> {
    let lease = SessionLease {
        pid: std::process::id(),
        heartbeat: now_epoch(),
    };
    if create_lease_exclusive(&path, &lease)? {
        return Ok(Some(LeaseGuard::start(path)));
    }
    // A lease already exists: take it over only if it isn't live.
    match load_lease(&path) {
        Some(existing) if is_live(&existing, now_epoch()) => Ok(None),
        _ => {
            let _ = fs::remove_file(&path);
            if create_lease_exclusive(&path, &lease)? {
                Ok(Some(LeaseGuard::start(path)))
            } else {
                // Another worker won the reclaim race.
                Ok(None)
            }
        }
    }
}

/// Holds an issue's session lease for the life of a launcher process: a
/// background thread refreshes the heartbeat, and dropping the guard stops it
/// and removes the lease file. A launcher that exits via `std::process::exit`
/// (which skips destructors) leaves the file behind, but with a now-dead pid,
/// so the next acquirer reclaims it.
pub struct LeaseGuard {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LeaseGuard {
    /// Start a guard for an already-created lease at `path`, spawning the
    /// heartbeat thread.
    fn start(path: PathBuf) -> LeaseGuard {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let path = path.clone();
            let stop = Arc::clone(&stop);
            thread::spawn(move || heartbeat_loop(&path, &stop))
        };
        LeaseGuard {
            path,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

/// Refresh `path`'s heartbeat every [`HEARTBEAT_INTERVAL`] until asked to stop,
/// waking each second so a drop is noticed promptly.
fn heartbeat_loop(path: &Path, stop: &AtomicBool) {
    loop {
        for _ in 0..HEARTBEAT_INTERVAL.as_secs() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let lease = SessionLease {
            pid: std::process::id(),
            heartbeat: now_epoch(),
        };
        if let Err(err) = write_lease(path, &lease) {
            eprintln!(
                "warning: failed to refresh session lease {}: {err:#}",
                path.display()
            );
        }
    }
}

/// Atomically overwrite a lease file via a temp file and rename, so a concurrent
/// reader never sees a half-written lease.
fn write_lease(path: &Path, lease: &SessionLease) -> Result<()> {
    let json = serde_json::to_string(lease).context("failed to serialize session lease")?;
    store::atomic_write(path, json.as_bytes())
        .with_context(|| format!("failed to install session lease {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_lease_at, branch_and_slug, find_issue_for_dir, is_live, is_unstarted,
        issue_fingerprint, lease_is_stale_at, load_lease, mutate_at, parse_directive,
        parse_prompted_directive, pr_outcome, process_alive, read_state, stop_flag_at,
        write_stop_flag_in, Attention, Directive, IssueState, OptionsWatch, Phase, PostedRef,
        PrOutcome, PrepState, ReactionWatch, SessionLease, WaitState,
    };
    use crate::models::PullRequest;
    use std::path::{Path, PathBuf};

    /// A unique scratch directory for building fake issues roots and worktrees.
    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ghwf-state-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a state file for `owner/repo#number` under `issues_root`, recording
    /// `worktree` when given.
    fn write_state(
        issues_root: &Path,
        owner: &str,
        repo: &str,
        number: u64,
        worktree: Option<&Path>,
    ) {
        let state = IssueState {
            prep: worktree.map(|path| PrepState {
                worktree_path: Some(path.to_path_buf()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let dir = issues_root.join(owner).join(repo);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{number}.json")),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn find_issue_for_dir_matches_worktree_and_descendants() {
        let root = scratch("find-match");
        let issues_root = root.join("issues");
        let worktree = root.join("wt_7");
        std::fs::create_dir_all(worktree.join("sub")).unwrap();
        write_state(&issues_root, "o", "r", 7, Some(&worktree));
        // Another issue without a worktree must not interfere.
        write_state(&issues_root, "o", "r", 8, None);
        // A lease file and a lock file sit beside the state files; their numeric
        // prefixes must not make the walker mistake them for state files (their
        // stems, `7.lease` and `7.json`, aren't numbers). An empty lock file
        // would also fail to parse as state if the walker picked it up.
        std::fs::write(issues_root.join("o").join("r").join("7.lease.json"), "{}").unwrap();
        std::fs::write(issues_root.join("o").join("r").join("7.json.lock"), "").unwrap();

        let expected = Some(("o".to_string(), "r".to_string(), 7));
        assert_eq!(
            find_issue_for_dir(&issues_root, &worktree).unwrap(),
            expected
        );
        assert_eq!(
            find_issue_for_dir(&issues_root, &worktree.join("sub")).unwrap(),
            expected
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn mutate_at_applies_change_and_preserves_other_fields() {
        let dir = scratch("mutate-apply");
        let path = dir.join("5.json");
        // Seed a state that a `work-on` save might have just written: a phase
        // advance plus a consumed-set, and a non-zero nudge counter.
        let mut seeded = IssueState {
            phase: Phase::Review,
            stop_nudges: 5,
            ..Default::default()
        };
        seeded.consumed_directives.insert(42);
        std::fs::write(&path, serde_json::to_string_pretty(&seeded).unwrap()).unwrap();

        // A narrow writer bumps only its own field.
        mutate_at(&path, |s| s.stop_nudges += 1).unwrap();

        let after = read_state(&path).unwrap().expect("state still present");
        assert_eq!(after.stop_nudges, 6, "the bump is applied");
        // The fields the narrow writer doesn't own survive — it re-read the
        // file rather than writing back a stale snapshot.
        assert_eq!(after.phase, Phase::Review);
        assert!(after.consumed_directives.contains(&42));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mutate_at_is_noop_when_no_state_exists() {
        let dir = scratch("mutate-missing");
        let path = dir.join("9.json");
        // No state file yet: the closure must not run and nothing is created.
        mutate_at(&path, |s| s.stop_nudges += 1).unwrap();
        assert!(!path.exists(), "no state file is created");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_issue_for_dir_outside_any_worktree() {
        let root = scratch("find-miss");
        let issues_root = root.join("issues");
        let worktree = root.join("wt_7");
        std::fs::create_dir_all(&worktree).unwrap();
        write_state(&issues_root, "o", "r", 7, Some(&worktree));

        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(find_issue_for_dir(&issues_root, &elsewhere).unwrap(), None);
        // A missing issues root is a miss, not an error.
        assert_eq!(
            find_issue_for_dir(&root.join("no-issues"), &worktree).unwrap(),
            None
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn old_state_files_load_without_wait_fields() {
        let state: IssueState =
            serde_json::from_str(r#"{"phase":"implement","consumed_directives":[1]}"#).unwrap();
        assert!(state.wait.is_none());
        assert!(state.last_posted.is_none());
        assert!(!state.issue_closed);
        assert_eq!(state.stop_nudges, 0);
        assert!(state.consumed_reactions.is_empty());
        assert!(state.pr_outcome.is_none());
        assert_eq!(state.attention, Attention::WaitingOnClaude);
        assert!(state.labels_synced.is_none());
    }

    #[test]
    fn old_prep_state_with_pr_ready_still_loads() {
        // The retired `pr_ready` flag is an unknown field to serde now.
        let state: IssueState = serde_json::from_str(
            r#"{"phase":"review","consumed_directives":[],"prep":{"no_branch":false,
                "worktree_session_id":null,"branch":"b","worktree_path":"/wt",
                "pr_number":34,"pr_ready":true}}"#,
        )
        .unwrap();
        assert_eq!(state.prep.unwrap().pr_number, Some(34));
    }

    #[test]
    fn attention_round_trips() {
        for (attention, label) in [
            (Attention::WaitingOnUser, "waiting-on-user"),
            (Attention::WaitingOnClaude, "waiting-on-claude"),
            (Attention::WaitingOnGhwf, "waiting-on-ghwf"),
        ] {
            let state = IssueState {
                attention,
                ..Default::default()
            };
            let json = serde_json::to_string(&state).unwrap();
            assert!(json.contains(&format!(r#""attention":"{label}""#)));
            let back: IssueState = serde_json::from_str(&json).unwrap();
            assert_eq!(back.attention, attention);
        }
    }

    fn pull_request(state: &str, merged: bool) -> PullRequest {
        PullRequest {
            number: 33,
            title: "PR".to_string(),
            state: state.to_string(),
            merged,
            draft: false,
            body: None,
            html_url: "https://github.com/o/r/pull/33".to_string(),
            head: crate::models::Head {
                ref_name: "branch".to_string(),
                sha: "sha".to_string(),
            },
        }
    }

    #[test]
    fn pr_outcome_maps_merged_closed_and_open() {
        assert_eq!(
            pr_outcome(&pull_request("closed", true)),
            Some(PrOutcome::Merged)
        );
        assert_eq!(
            pr_outcome(&pull_request("closed", false)),
            Some(PrOutcome::Closed)
        );
        assert_eq!(pr_outcome(&pull_request("open", false)), None);
    }

    #[test]
    fn is_concluded_tracks_issue_close_and_pr_outcome() {
        let open = IssueState::default();
        assert!(!open.is_concluded());

        let closed_issue = IssueState {
            issue_closed: true,
            ..Default::default()
        };
        assert!(closed_issue.is_concluded());

        for outcome in [PrOutcome::Merged, PrOutcome::Closed] {
            let concluded_pr = IssueState {
                pr_outcome: Some(outcome),
                ..Default::default()
            };
            assert!(concluded_pr.is_concluded());
        }
    }

    #[test]
    fn pr_outcome_round_trips() {
        let state = IssueState {
            pr_outcome: Some(PrOutcome::Merged),
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains(r#""pr_outcome":"merged""#));
        let back: IssueState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pr_outcome, Some(PrOutcome::Merged));
    }

    #[test]
    fn old_wait_state_loads_without_reaction_watches() {
        let wait: WaitState = serde_json::from_str(
            r#"{"since":"2026-06-06T10:00:00Z","issue_fingerprint":"abc","comments":{},"review_comments":{}}"#,
        )
        .unwrap();
        assert!(wait.reaction_watches.is_empty());
        assert_eq!(wait.last_base_sync, crate::implement::BaseSync::UpToDate);
    }

    #[test]
    fn wait_state_round_trips() {
        let state = IssueState {
            wait: Some(WaitState {
                since: "2026-06-06T10:00:00Z".to_string(),
                issue_fingerprint: "abc".to_string(),
                comments: [(1, "h1".to_string())].into(),
                review_comments: [(2, "h2".to_string())].into(),
                etags: [("issue".to_string(), "W/\"x\"".to_string())].into(),
                reaction_watches: [(
                    "issue".to_string(),
                    ReactionWatch {
                        comment_id: 9,
                        plus_one_ids: [100].into(),
                    },
                )]
                .into(),
                options_watches: [("issue".to_string(), OptionsWatch { comment_id: 11 })].into(),
                pr_draft: Some(true),
                last_base_sync: crate::implement::BaseSync::Conflict,
            }),
            last_posted: Some(PostedRef {
                id: 7,
                created_at: "2026-06-06T11:00:00Z".to_string(),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: IssueState = serde_json::from_str(&json).unwrap();
        let wait = back.wait.unwrap();
        assert_eq!(wait.since, "2026-06-06T10:00:00Z");
        assert_eq!(wait.comments.get(&1).map(String::as_str), Some("h1"));
        assert_eq!(wait.etags.get("issue").map(String::as_str), Some("W/\"x\""));
        assert_eq!(wait.pr_draft, Some(true));
        assert_eq!(wait.last_base_sync, crate::implement::BaseSync::Conflict);
        let watch = wait.reaction_watches.get("issue").unwrap();
        assert_eq!(watch.comment_id, 9);
        assert!(watch.plus_one_ids.contains(&100));
        assert_eq!(back.last_posted.unwrap().id, 7);
    }

    #[test]
    fn fingerprint_distinguishes_fields_and_absent_body() {
        let a = issue_fingerprint("title", Some("body"), "open");
        assert_eq!(a, issue_fingerprint("title", Some("body"), "open"));
        assert_ne!(a, issue_fingerprint("title", Some("body"), "closed"));
        assert_ne!(a, issue_fingerprint("title", None, "open"));
        // Field boundaries can't bleed into each other.
        assert_ne!(
            issue_fingerprint("ab", Some("c"), "open"),
            issue_fingerprint("a", Some("bc"), "open")
        );
    }

    #[test]
    fn parse_each_command() {
        assert_eq!(
            parse_directive("/approve-pre-plan"),
            Some(Directive::ApprovePrePlan)
        );
        assert_eq!(
            parse_directive("/approve-preplan"),
            Some(Directive::ApprovePrePlan)
        );
        assert_eq!(
            parse_directive("/approve-plan"),
            Some(Directive::ApprovePlan)
        );
        assert_eq!(
            parse_directive("/approve-implementation"),
            Some(Directive::ApproveImplementation)
        );
        assert_eq!(parse_directive("/proceed"), Some(Directive::Proceed));
    }

    #[test]
    fn parse_allows_trailing_words() {
        assert_eq!(
            parse_directive("/approve-plan looks good!"),
            Some(Directive::ApprovePlan)
        );
    }

    #[test]
    fn parse_requires_word_boundary() {
        assert_eq!(parse_directive("/approve-plans"), None);
        assert_eq!(parse_directive("/proceeding with caution"), None);
    }

    #[test]
    fn parse_requires_line_start() {
        assert_eq!(parse_directive("please /approve-plan"), None);
        assert_eq!(
            parse_directive("looks good\n/approve-plan"),
            Some(Directive::ApprovePlan)
        );
    }

    #[test]
    fn parse_first_match_wins() {
        assert_eq!(
            parse_directive("/approve-plan\n/approve-implementation"),
            Some(Directive::ApprovePlan)
        );
    }

    #[test]
    fn approval_command_round_trips_through_parse() {
        for phase in [Phase::PrePlan, Phase::PrepAndPlan] {
            let command = phase.approval_command().expect("command-approved phase");
            let directive = parse_directive(command).expect("command parses");
            assert_eq!(directive.approves(), Some(phase));
        }
        // Implement advances via the PR's ready-for-review flip, not a command.
        assert_eq!(Phase::Implement.approval_command(), None);
        assert_eq!(Phase::Review.approval_command(), None);
    }

    #[test]
    fn approve_implementation_is_retired() {
        assert_eq!(Directive::ApproveImplementation.approves(), None);
        // Still recognised as a typed command, so it can be explained…
        assert_eq!(
            parse_directive("/approve-implementation"),
            Some(Directive::ApproveImplementation)
        );
        // …but never a 👍 target.
        assert_eq!(
            parse_prompted_directive("comment `/approve-implementation` to advance"),
            None
        );
    }

    #[test]
    fn prompted_directive_matches_backticked_mid_line_mention() {
        assert_eq!(
            parse_prompted_directive("Next: comment `/approve-plan` to advance."),
            Some(Directive::ApprovePlan)
        );
    }

    #[test]
    fn prompted_directive_last_mention_wins() {
        // A transition status update mentions the fired command first and the
        // next prompt last; the prompt is what a 👍 means.
        let body = "Phase advanced (triggered by `/approve-pre-plan`).\n\n\
                    Next: comment `/approve-plan` to advance.";
        assert_eq!(parse_prompted_directive(body), Some(Directive::ApprovePlan));
        // The alias mentions in pre-plan prose both map to the same directive.
        let body = "comment `/approve-pre-plan` (alias `/approve-preplan`) to advance";
        assert_eq!(
            parse_prompted_directive(body),
            Some(Directive::ApprovePrePlan)
        );
    }

    #[test]
    fn prompted_directive_requires_word_boundary() {
        assert_eq!(parse_prompted_directive("see /approve-plans"), None);
        assert_eq!(parse_prompted_directive("x/approve-plan"), None);
        assert_eq!(
            parse_prompted_directive("(/approve-plan)"),
            Some(Directive::ApprovePlan)
        );
    }

    #[test]
    fn prompted_directive_ignores_proceed_and_plain_prose() {
        assert_eq!(parse_prompted_directive("`/proceed` is retired"), None);
        assert_eq!(parse_prompted_directive("no commands here"), None);
    }

    #[test]
    fn claim_is_exclusive_and_seeds_default_state() {
        use super::claim_file;
        let root = scratch("claim");
        let path = root.join("o").join("r").join("7.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // First claim wins and writes parseable default state.
        assert!(claim_file(&path).unwrap());
        let state: IssueState =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(state.phase, Phase::PrePlan);

        // A second claim loses without disturbing the file.
        assert!(!claim_file(&path).unwrap());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn naming_basic() {
        // A main-repo issue (no prefix) keeps the bare scheme.
        let (branch, slug) = branch_and_slug(None, 1, "Basic workflow");
        assert_eq!(branch, "issue_1_basic_workflow");
        assert_eq!(slug, "basic-workflow");
    }

    #[test]
    fn naming_strips_punctuation() {
        let (branch, slug) = branch_and_slug(None, 42, "Fix: the `foo`/bar bug!");
        assert_eq!(branch, "issue_42_fix_the_foo_bar_bug");
        assert_eq!(slug, "fix-the-foo-bar-bug");
    }

    #[test]
    fn naming_prefixes_foreign_repo() {
        // A foreign-repo issue is qualified so it can't collide with a
        // same-numbered main-repo issue; the slug stays unprefixed.
        let (branch, slug) = branch_and_slug(Some("documentation"), 42, "Basic workflow");
        assert_eq!(branch, "documentation_issue_42_basic_workflow");
        assert_eq!(slug, "basic-workflow");
    }

    #[test]
    fn naming_sanitises_and_drops_empty_prefix() {
        // A prefix is sanitised like a slug word…
        let (branch, _) = branch_and_slug(Some("My Docs!"), 5, "X");
        assert_eq!(branch, "my_docs_issue_5_x");
        // …and one that sanitises to nothing leaves no leading underscore.
        let (branch, _) = branch_and_slug(Some("!!!"), 5, "X");
        assert_eq!(branch, "issue_5_x");
    }

    /// A scratch lease path under a unique temp dir.
    fn lease_scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ghwf-lease-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("1.lease.json")
    }

    #[test]
    fn process_alive_self_is_alive() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn is_live_true_for_self_with_fresh_heartbeat() {
        let lease = SessionLease {
            pid: std::process::id(),
            heartbeat: 1_000,
        };
        // Heartbeat well within the TTL of `now`.
        assert!(is_live(&lease, 1_010));
    }

    #[test]
    fn is_live_false_when_heartbeat_is_stale() {
        // Our own (alive) pid, but a heartbeat older than the TTL — the
        // recycled-pid guard.
        let lease = SessionLease {
            pid: std::process::id(),
            heartbeat: 1_000,
        };
        assert!(!is_live(&lease, 1_000 + super::LEASE_TTL + 1));
    }

    /// A positive pid (as `i32`) far beyond any real process, so `kill(pid, 0)`
    /// reports it gone. Avoids 0 and negatives, which target process groups.
    const DEAD_PID: u32 = 0x7FFF_FFFE;

    #[test]
    fn is_live_false_for_dead_pid() {
        // Even with a fresh heartbeat, a gone process reads as not live.
        let lease = SessionLease {
            pid: DEAD_PID,
            heartbeat: 1_000,
        };
        assert!(!process_alive(DEAD_PID));
        assert!(!is_live(&lease, 1_000));
    }

    #[test]
    fn lease_is_stale_distinguishes_absent_live_and_dead() {
        let path = lease_scratch("stale-check");
        let _ = std::fs::remove_file(&path);

        // No lease file: not a crash signature.
        assert!(!lease_is_stale_at(&path, 1_000));

        // A live lease (our own pid, fresh heartbeat): not stale.
        std::fs::write(
            &path,
            serde_json::to_string(&SessionLease {
                pid: std::process::id(),
                heartbeat: 1_000,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(!lease_is_stale_at(&path, 1_010));

        // The same live pid but a heartbeat past the TTL: stale.
        assert!(lease_is_stale_at(&path, 1_000 + super::LEASE_TTL + 1));

        // A dead pid, as a crashed launcher would leave behind: stale.
        std::fs::write(
            &path,
            serde_json::to_string(&SessionLease {
                pid: DEAD_PID,
                heartbeat: 1_000,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(lease_is_stale_at(&path, 1_000));

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn acquire_lease_is_exclusive_then_reclaimable() {
        let path = lease_scratch("exclusive");
        let _ = std::fs::remove_file(&path);

        // First acquirer wins and writes a live lease.
        let guard = acquire_lease_at(path.clone()).unwrap();
        assert!(guard.is_some());
        assert!(path.is_file());
        let lease = load_lease(&path).unwrap();
        assert_eq!(lease.pid, std::process::id());

        // A second acquirer is turned away while the first holds it.
        assert!(acquire_lease_at(path.clone()).unwrap().is_none());

        // Dropping the guard releases the lease file…
        drop(guard);
        assert!(!path.is_file());
        // …so it can be acquired again.
        assert!(acquire_lease_at(path.clone()).unwrap().is_some());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn acquire_lease_reclaims_a_stale_lease() {
        let path = lease_scratch("stale");
        let _ = std::fs::remove_file(&path);
        // Hand-write a stale lease (dead pid), as a crashed launcher would leave.
        std::fs::write(
            &path,
            serde_json::to_string(&SessionLease {
                pid: DEAD_PID,
                heartbeat: 0,
            })
            .unwrap(),
        )
        .unwrap();

        // It is reclaimed: the acquirer wins and overwrites it with its own.
        let guard = acquire_lease_at(path.clone()).unwrap();
        assert!(guard.is_some());
        assert_eq!(load_lease(&path).unwrap().pid, std::process::id());

        drop(guard);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn is_unstarted_only_for_a_bare_claim() {
        // A default state file is a bare claim.
        assert!(is_unstarted(&IssueState::default()));

        // A prep with no worktree and no session is still bare.
        let bare_prep = IssueState {
            prep: Some(PrepState::default()),
            ..Default::default()
        };
        assert!(is_unstarted(&bare_prep));

        // A recorded worktree means real progress.
        let with_worktree = IssueState {
            prep: Some(PrepState {
                worktree_path: Some(PathBuf::from("/tmp/wt")),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!is_unstarted(&with_worktree));

        // A recorded session means real progress.
        let with_session = IssueState {
            prep: Some(PrepState {
                worktree_session_id: Some("abc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!is_unstarted(&with_session));

        // A concluded issue is never "unstarted", whatever its prep.
        let concluded = IssueState {
            issue_closed: true,
            ..Default::default()
        };
        assert!(!is_unstarted(&concluded));

        // A bare claim parked for the user (a refusal) is kept, not reclaimed.
        let parked = IssueState {
            attention: Attention::WaitingOnUser,
            ..Default::default()
        };
        assert!(!is_unstarted(&parked));
    }

    #[test]
    fn stop_flag_round_trips_and_ignores_garbage() {
        let dir = scratch("stop-flag");
        // No flag yet.
        assert_eq!(stop_flag_at(&dir), None);
        // A written timestamp reads back exactly.
        write_stop_flag_in(&dir, 1_700_000_000_123).unwrap();
        assert_eq!(stop_flag_at(&dir), Some(1_700_000_000_123));
        // Unparseable contents read as "no stop", not an error.
        std::fs::write(dir.join("forever-stop"), "not a number").unwrap();
        assert_eq!(stop_flag_at(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_flag_gate_is_strictly_newer_than_worker_start() {
        let dir = scratch("stop-gate");
        // Helper mirroring `stop_requested_since`'s comparison against the flag.
        let requested_since = |start: u64| stop_flag_at(&dir).is_some_and(|at| at > start);

        write_stop_flag_in(&dir, 1_000).unwrap();
        // A worker started before the request honours it.
        assert!(requested_since(999));
        // One started at the same millisecond or later treats it as stale.
        assert!(!requested_since(1_000));
        assert!(!requested_since(1_001));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
