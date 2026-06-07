use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store;

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
    ApproveImplementation,
    // The retired generic command; still recognised so its retirement can be
    // explained rather than the comment being silently ignored.
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
    /// the retired generic `/proceed`.
    pub fn approves(self) -> Option<Phase> {
        match self {
            Directive::ApprovePrePlan => Some(Phase::PrePlan),
            Directive::ApprovePlan => Some(Phase::PrepAndPlan),
            Directive::ApproveImplementation => Some(Phase::Implement),
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
/// Unlike `parse_directive`, mentions count mid-line (status prose backticks
/// them). Last-mention-wins makes ghwf comments self-describing 👍 targets:
/// status updates end with the "Next: comment `/approve-X`" prompt, and
/// misfire notes end with "the command that advances it is `/approve-X`".
/// The retired `/proceed` never maps — a 👍 can only mean a live approval.
pub fn parse_prompted_directive(body: &str) -> Option<Directive> {
    let mut last: Option<(usize, Directive)> = None;
    for &(command, directive) in DIRECTIVE_COMMANDS {
        if directive == Directive::Proceed {
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
    /// or `None` for the terminal phase.
    pub fn approval_command(self) -> Option<&'static str> {
        match self {
            Phase::PrePlan => Some("/approve-pre-plan"),
            Phase::PrepAndPlan => Some("/approve-plan"),
            Phase::Implement => Some("/approve-implementation"),
            Phase::Review => None,
        }
    }
}

/// Per-issue workflow state. Scoped to the issue (not a session), since the phase
/// reflects the progress of the work across sessions.
#[derive(Default, Serialize, Deserialize)]
pub struct IssueState {
    pub phase: Phase,
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
    // Whether the PR has been flipped from draft to ready-for-review (on entering
    // the review phase). Kept idempotent so we only mark it once.
    #[serde(default)]
    pub pr_ready: bool,
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
        branch_and_slug, issue_fingerprint, parse_directive, parse_prompted_directive, Directive,
        IssueState, Phase, PostedRef, ReactionWatch, WaitState,
    };

    #[test]
    fn old_state_files_load_without_wait_fields() {
        let state: IssueState =
            serde_json::from_str(r#"{"phase":"implement","consumed_directives":[1]}"#).unwrap();
        assert!(state.wait.is_none());
        assert!(state.last_posted.is_none());
        assert!(!state.issue_closed);
        assert_eq!(state.stop_nudges, 0);
        assert!(state.consumed_reactions.is_empty());
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
        for phase in [Phase::PrePlan, Phase::PrepAndPlan, Phase::Implement] {
            let command = phase.approval_command().expect("non-terminal phase");
            let directive = parse_directive(command).expect("command parses");
            assert_eq!(directive.approves(), Some(phase));
        }
        assert_eq!(Phase::Review.approval_command(), None);
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
