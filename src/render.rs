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

/// Render a single comment (e.g. one just created) as pretty-printed JSON.
pub fn comment_json(comment: &Comment) -> Result<String> {
    serde_json::to_string_pretty(comment).context("failed to serialize comment as JSON")
}

/// Assemble the body of a Claude-authored comment: a visible attribution header,
/// the user's markdown, and an optional hidden metadata marker identifying the
/// authoring session.
///
/// `<hr>` is used rather than `---`, because `**Claude says:**` immediately
/// followed by a `---` line renders as a setext heading on GitHub.
pub fn build_comment_body(user_body: &str, session_token: Option<&str>) -> String {
    let mut body = format!("**Claude says:**\n<hr>\n\n{}", user_body.trim());
    if let Some(token) = session_token {
        body.push_str(&format!("\n\n<!-- ghwf:v1 session={token} -->"));
    }
    body
}
