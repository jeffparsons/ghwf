use anyhow::{Context, Result};
use serde::Serialize;

use crate::models::{Comment, Issue};

/// The normalized output of `work-on`: an issue together with its comments.
///
/// This is the single place the output format lives; a markdown rendering will
/// slot in here later.
#[derive(Serialize)]
pub struct WorkOn {
    pub issue: Issue,
    pub comments: Vec<Comment>,
}

/// Render a `WorkOn` as pretty-printed JSON.
pub fn to_json(out: &WorkOn) -> Result<String> {
    serde_json::to_string_pretty(out).context("failed to serialize work-on output as JSON")
}
