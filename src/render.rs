use anyhow::{Context, Result};

use crate::models::{Comment, Issue};
use crate::state::Phase;

/// Opening of the hidden metadata marker embedded in Claude-authored comments.
const MARKER_PREFIX: &str = "<!-- ghwf:v1 session=";
/// Closing of the hidden metadata marker.
const MARKER_SUFFIX: &str = " -->";

/// A new-or-changed comment prepared for rendering.
pub struct CommentView<'a> {
    pub comment: &'a Comment,
    /// The comment body with the hidden ghwf marker stripped, ready to display.
    pub body: String,
    /// True if this comment was seen before but its content has since changed.
    pub updated: bool,
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
        body.push_str(&format!("\n\n{MARKER_PREFIX}{token}{MARKER_SUFFIX}"));
    }
    body
}

/// Extract the session token from a comment's hidden ghwf marker, if present.
pub fn extract_session_token(body: &str) -> Option<String> {
    let start = body.find(MARKER_PREFIX)? + MARKER_PREFIX.len();
    let rest = &body[start..];
    let end = rest.find(MARKER_SUFFIX)?;
    Some(rest[..end].to_string())
}

/// Remove the hidden ghwf marker (and the blank line before it) from a comment
/// body, leaving just the displayable content.
pub fn strip_ghwf_marker(body: &str) -> String {
    match body.find(MARKER_PREFIX) {
        Some(idx) => body[..idx].trim_end().to_string(),
        None => body.to_string(),
    }
}

/// Guidance shown to Claude during the pre-plan phase.
pub const PRE_PLAN_BODY: &str =
    "Pre-plan — gathering the information needed to write a plan.\n\n\
     Discuss on the issue itself. Post questions and clarifications as comments with \
     `ghwf create-issue-comment <issue>`. When you have enough information, post a comment \
     that summarises your understanding and clearly states you are ready to write a plan.\n\n\
     Do not start planning or advance the workflow yourself. Wait for the user to comment \
     `/proceed` on the issue; ghwf will then advance to the prep-and-plan phase.";

/// Render the phase banner shown atop `work-on` output: the current phase, an
/// optional transition note, then the phase-specific `body`.
pub fn render_phase_banner(
    phase: Phase,
    transition: Option<(Phase, Phase, Option<String>)>,
    body: &str,
) -> String {
    let mut out = format!("Phase: {}", phase.label());

    if let Some((from, to, trigger)) = transition {
        let by = trigger
            .map(|login| format!(" from {login}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\nPhase advanced: {} → {} (triggered by `/proceed`{by}).",
            from.label(),
            to.label()
        ));
    }

    out.push_str("\n\n");
    out.push_str(body);
    out
}

/// Render the markdown digest of what's new or changed on the digest `subject` —
/// an issue or a PR. `noun` ("issue" / "PR") tailors the prose for each.
pub fn render_work_on(
    subject: &Issue,
    noun: &str,
    body_changed: bool,
    new: &[CommentView],
) -> String {
    if !body_changed && new.is_empty() {
        return format!(
            "No new activity on {noun} #{} \"{}\" since you last ran `ghwf work-on`.",
            subject.number, subject.title
        );
    }

    let mut out = format!("## #{}: {}  ({})\n", subject.number, subject.title, subject.state);

    if body_changed {
        out.push_str(&format!(
            "\n{} body by {}:\n\n",
            capitalize_first(noun),
            subject.user.login
        ));
        out.push_str(&blockquote(subject.body.as_deref().unwrap_or("")));
        out.push('\n');
    }

    if !new.is_empty() {
        if body_changed {
            out.push_str("\n<hr>\n");
        }
        out.push_str("\nNew comments since you last ran `ghwf work-on`:\n");
        for (i, view) in new.iter().enumerate() {
            if i > 0 {
                out.push_str("\n<hr>\n");
            }
            let tag = if view.updated { " (updated)" } else { "" };
            out.push_str(&format!(
                "\n**{}** at {} said{}:\n\n",
                view.comment.user.login, view.comment.created_at, tag
            ));
            out.push_str(&blockquote(&view.body));
            out.push('\n');
        }
    }

    out.trim_end().to_string()
}

/// Uppercase the first character, leaving the rest untouched (so acronyms like
/// "PR" stay "PR" rather than becoming "Pr").
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Prefix every line with a markdown blockquote marker.
fn blockquote(text: &str) -> String {
    if text.trim().is_empty() {
        return ">".to_string();
    }
    text.lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
