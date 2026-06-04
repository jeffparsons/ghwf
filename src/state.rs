use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store;

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
