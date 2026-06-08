use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{store, worktree};

/// Branch/worktree name and plan-file slug derived from an issue.
///
/// Returns `(branch, slug)` where `branch` uses underscores (`issue_<n>_<slug>`,
/// per the org convention) and `slug` is the kebab-case form for the plan
/// filename (`plans/<n>-<slug>.md`).
pub fn branch_and_slug(number: u64, title: &str) -> (String, String) {
    let words = slug_words(title);
    let branch = format!("issue_{number}_{}", words.join("_"));
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
}

impl Phase {
    /// The phase that follows this one, or `None` if this is the terminal phase.
    pub fn next(self) -> Option<Phase> {
        match self {
            Phase::PrePlan => Some(Phase::PrepAndPlan),
            Phase::PrepAndPlan => Some(Phase::Implement),
            Phase::Implement => Some(Phase::Review),
            Phase::Review => None,
        }
    }

    /// Human-readable label, matching the on-disk serialization.
    pub fn label(self) -> &'static str {
        match self {
            Phase::PrePlan => "pre-plan",
            Phase::PrepAndPlan => "prep-and-plan",
            Phase::Implement => "implement",
            Phase::Review => "review",
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
    // The most recent comment ghwf itself posted to either thread. Lives here
    // rather than on `WaitState` because `work-on` rebuilds that wholesale
    // when recording a baseline, and a status update posted mid-run must
    // survive. Feed-mode self-calibration in `wait` reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_posted: Option<PostedRef>,
}

/// A reference to a comment ghwf posted, for feed-lag self-calibration.
#[derive(Clone, Serialize, Deserialize)]
pub struct PostedRef {
    pub id: u64,
    pub created_at: String,
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
    // Whether the PR was a draft when the baseline was recorded. A flip in
    // either direction wakes the wait: ready-for-review advances the
    // implement phase. Absent when no PR exists (or for old state files).
    #[serde(default)]
    pub pr_draft: Option<bool>,
}

/// A comment whose 👍 reactions `wait` polls, with the reaction ids already
/// seen (so only a fresh 👍 wakes).
#[derive(Clone, Serialize, Deserialize)]
pub struct ReactionWatch {
    pub comment_id: u64,
    pub plus_one_ids: BTreeSet<u64>,
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
    let path = state_path(owner, repo, number)?;
    match fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .with_context(|| format!("failed to parse issue state {}", path.display())),
        Err(_) => Ok(None),
    }
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
pub fn save(owner: &str, repo: &str, number: u64, state: &IssueState) -> Result<()> {
    let path = state_path(owner, repo, number)?;
    let dir = path
        .parent()
        .expect("state path always has a parent directory");
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(state).context("failed to serialize issue state")?;
    fs::write(&path, json)
        .with_context(|| format!("failed to write issue state {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        branch_and_slug, find_issue_for_dir, issue_fingerprint, parse_directive,
        parse_prompted_directive, pr_outcome, Attention, Directive, IssueState, Phase, PostedRef,
        PrOutcome, PrepState, ReactionWatch, WaitState,
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
                pr_draft: Some(true),
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
        let (branch, slug) = branch_and_slug(1, "Basic workflow");
        assert_eq!(branch, "issue_1_basic_workflow");
        assert_eq!(slug, "basic-workflow");
    }

    #[test]
    fn naming_strips_punctuation() {
        let (branch, slug) = branch_and_slug(42, "Fix: the `foo`/bar bug!");
        assert_eq!(branch, "issue_42_fix_the_foo_bar_bug");
        assert_eq!(slug, "fix-the-foo-bar-bug");
    }
}
