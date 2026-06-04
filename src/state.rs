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

/// The directive a user posts (at the start of a comment line) to advance the
/// workflow to the next phase.
pub const PROCEED_DIRECTIVE: &str = "/proceed";

/// A phase of work on an issue. Named in the imperative mood.
#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    #[default]
    PrePlan,
    PrepAndPlan,
    Implement,
}

impl Phase {
    /// The phase that follows this one, or `None` if this is the terminal phase.
    pub fn next(self) -> Option<Phase> {
        match self {
            Phase::PrePlan => Some(Phase::PrepAndPlan),
            Phase::PrepAndPlan => Some(Phase::Implement),
            Phase::Implement => None,
        }
    }

    /// Human-readable label, matching the on-disk serialization.
    pub fn label(self) -> &'static str {
        match self {
            Phase::PrePlan => "pre-plan",
            Phase::PrepAndPlan => "prep-and-plan",
            Phase::Implement => "implement",
        }
    }
}

/// Per-issue workflow state. Scoped to the issue (not a session), since the phase
/// reflects the progress of the work across sessions.
#[derive(Default, Serialize, Deserialize)]
pub struct IssueState {
    pub phase: Phase,
    // Ids of comments whose `/proceed` directive has already been acted on.
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
    // The Claude session that started prep-and-plan, when known.
    pub session_id: Option<String>,
    // The branch name, once the worktree has been created.
    pub branch: Option<String>,
    // The worktree path, once created.
    pub worktree_path: Option<PathBuf>,
    // The draft PR's number, once opened.
    pub pr_number: Option<u64>,
}

/// True if `body` contains a `/proceed` directive at the start of a line.
///
/// The directive must begin the line and be followed by a word boundary, so
/// `/proceed` and `/proceed now` count, but `/proceeding` and conversational
/// mid-sentence mentions do not.
pub fn is_proceed_directive(body: &str) -> bool {
    body.lines().any(|line| {
        line.strip_prefix(PROCEED_DIRECTIVE)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    })
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

#[cfg(test)]
mod tests {
    use super::branch_and_slug;

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
