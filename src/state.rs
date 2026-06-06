use std::collections::BTreeSet;
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
    // Populated once the prep-and-plan phase begins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prep: Option<PrepState>,
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
    let path = state_path(owner, repo, number)?;
    match fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json)
            .with_context(|| format!("failed to parse issue state {}", path.display())),
        Err(_) => Ok(IssueState::default()),
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
    fs::write(&path, json).with_context(|| format!("failed to write issue state {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{branch_and_slug, parse_directive, Directive, Phase};

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
        assert_eq!(parse_directive("/approve-plan"), Some(Directive::ApprovePlan));
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
