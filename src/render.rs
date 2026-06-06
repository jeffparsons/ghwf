use anyhow::{Context, Result};

use crate::models::{Comment, Issue, ReviewComment};
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

/// A new-or-changed inline review comment prepared for rendering.
pub struct ReviewCommentView<'a> {
    pub comment: &'a ReviewComment,
    /// The comment body with the hidden ghwf marker stripped, ready to display.
    pub body: String,
    /// The code the comment is anchored to, e.g. `src/main.rs:42`.
    pub location: String,
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
     that summarises your understanding, clearly states you are ready to write a plan, and \
     ends by prompting the user to comment `/approve-pre-plan` (alias `/approve-preplan`) \
     on the issue when they're happy to advance.\n\n\
     Do not start planning or advance the workflow yourself. Wait for the user's \
     `/approve-pre-plan`; ghwf will then advance to the prep-and-plan phase.";

/// A phase transition fired by an approval directive, for banner reporting.
pub struct Transition {
    pub from: Phase,
    pub to: Phase,
    /// The canonical spelling of the directive that fired.
    pub command: &'static str,
    /// Who posted it.
    pub by: String,
}

/// Why a consumed directive did not fire.
pub enum NoteKind {
    /// Approves a phase the workflow has already moved past (e.g. the same
    /// approval posted on both threads).
    Stale,
    /// Approves a phase the workflow has not reached yet.
    Premature,
    /// The retired generic `/proceed`.
    Retired,
}

/// A consumed-but-not-fired directive, for banner reporting.
pub struct DirectiveNote {
    pub kind: NoteKind,
    /// The canonical spelling of the directive.
    pub command: &'static str,
    /// Who posted it.
    pub by: String,
    /// Which conversation thread it was posted on ("issue" / "PR").
    pub source: &'static str,
    /// The workflow phase at the moment the directive was processed.
    pub phase_at: Phase,
}

/// Render the phase banner shown atop `work-on` output: the current phase, any
/// transitions and directive notes from this run, then the phase-specific `body`.
pub fn render_phase_banner(
    phase: Phase,
    transitions: &[Transition],
    notes: &[DirectiveNote],
    body: &str,
) -> String {
    let mut out = format!("Phase: {}", phase.label());

    for transition in transitions {
        out.push_str(&format!(
            "\nPhase advanced: {} → {} (triggered by `{}` from {}).",
            transition.from.label(),
            transition.to.label(),
            transition.command,
            transition.by
        ));
    }

    for note in notes {
        out.push_str(&format!("\n{}", render_note(note)));
    }
    // Stale notes are harmless echoes; the others are mistakes the user should
    // hear about.
    if notes
        .iter()
        .any(|n| matches!(n.kind, NoteKind::Premature | NoteKind::Retired))
    {
        out.push_str(
            "\nRelay the ignored-directive notes above to the user in a comment, so they \
             know the correct command.",
        );
    }

    out.push_str("\n\n");
    out.push_str(body);
    out
}

/// One banner line explaining a consumed-but-not-fired directive.
fn render_note(note: &DirectiveNote) -> String {
    let DirectiveNote {
        command,
        by,
        source,
        phase_at,
        ..
    } = note;
    // What would advance the workflow from where it stands.
    let next_step = match phase_at.approval_command() {
        Some(cmd) => format!("the command that advances it is `{cmd}`"),
        None => "there is nothing further to approve".to_string(),
    };
    match note.kind {
        NoteKind::Stale => format!(
            "Note: `{command}` from {by} (on the {source}) was ignored — the workflow is \
             already past the phase it approves."
        ),
        NoteKind::Premature => format!(
            "Note: `{command}` from {by} (on the {source}) was ignored — the workflow is \
             only in the {} phase; {next_step}.",
            phase_at.label()
        ),
        NoteKind::Retired => format!(
            "Note: `{command}` from {by} (on the {source}) was ignored — `/proceed` is \
             retired; the workflow is in the {} phase, and {next_step}.",
            phase_at.label()
        ),
    }
}

/// Render the markdown digest of what's new or changed on the digest `subject` —
/// an issue or a PR. `noun` ("issue" / "PR") tailors the prose for each.
pub fn render_work_on(
    subject: &Issue,
    noun: &str,
    body_changed: bool,
    new: &[CommentView],
    new_review: &[ReviewCommentView],
) -> String {
    if !body_changed && new.is_empty() && new_review.is_empty() {
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

    if !new_review.is_empty() {
        if body_changed || !new.is_empty() {
            out.push_str("\n<hr>\n");
        }
        out.push_str("\nNew inline review comments since you last ran `ghwf work-on`:\n");
        for (i, view) in new_review.iter().enumerate() {
            if i > 0 {
                out.push_str("\n<hr>\n");
            }
            let tag = if view.updated { " (updated)" } else { "" };
            out.push_str(&format!(
                "\n**{}** at {} said on `{}`{}:\n\n",
                view.comment.user.login, view.comment.created_at, view.location, tag
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

#[cfg(test)]
mod tests {
    use super::{
        render_phase_banner, render_work_on, CommentView, DirectiveNote, NoteKind,
        ReviewCommentView, Transition,
    };
    use crate::models::{Comment, Issue, ReviewComment, User};
    use crate::state::Phase;

    fn note(kind: NoteKind, command: &'static str, phase_at: Phase) -> DirectiveNote {
        DirectiveNote {
            kind,
            command,
            by: "user".to_string(),
            source: "PR",
            phase_at,
        }
    }

    #[test]
    fn banner_transition_names_command_and_author() {
        let transition = Transition {
            from: Phase::PrepAndPlan,
            to: Phase::Implement,
            command: "/approve-plan",
            by: "user".to_string(),
        };
        let out = render_phase_banner(Phase::Implement, &[transition], &[], "body");
        assert!(out.contains(
            "Phase advanced: prep-and-plan → implement (triggered by `/approve-plan` from user)."
        ));
    }

    #[test]
    fn banner_premature_note_suggests_current_command() {
        let notes = [note(NoteKind::Premature, "/approve-implementation", Phase::PrePlan)];
        let out = render_phase_banner(Phase::PrePlan, &[], &notes, "body");
        assert!(out.contains("`/approve-implementation` from user (on the PR) was ignored"));
        assert!(out.contains("only in the pre-plan phase"));
        assert!(out.contains("the command that advances it is `/approve-pre-plan`"));
        assert!(out.contains("Relay the ignored-directive notes"));
    }

    #[test]
    fn banner_retired_note_in_terminal_phase() {
        let notes = [note(NoteKind::Retired, "/proceed", Phase::Review)];
        let out = render_phase_banner(Phase::Review, &[], &notes, "body");
        assert!(out.contains("`/proceed` is retired"));
        assert!(out.contains("there is nothing further to approve"));
    }

    #[test]
    fn banner_stale_note_alone_has_no_relay_instruction() {
        let notes = [note(NoteKind::Stale, "/approve-plan", Phase::Implement)];
        let out = render_phase_banner(Phase::Implement, &[], &notes, "body");
        assert!(out.contains("already past the phase it approves"));
        assert!(!out.contains("Relay the ignored-directive notes"));
    }

    fn issue() -> Issue {
        Issue {
            number: 9,
            title: "A PR".to_string(),
            state: "open".to_string(),
            user: User {
                login: "author".to_string(),
            },
            body: Some("body".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            html_url: "https://github.com/o/r/pull/9".to_string(),
            author_association: "OWNER".to_string(),
        }
    }

    fn comment() -> Comment {
        Comment {
            id: 1,
            user: User {
                login: "reviewer".to_string(),
            },
            body: "looks good".to_string(),
            created_at: "2026-01-02T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            html_url: "https://github.com/o/r/pull/9#issuecomment-1".to_string(),
            author_association: "OWNER".to_string(),
        }
    }

    fn review_comment() -> ReviewComment {
        ReviewComment {
            id: 2,
            user: User {
                login: "reviewer".to_string(),
            },
            body: "rename this".to_string(),
            created_at: "2026-01-02T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            html_url: "https://github.com/o/r/pull/9#discussion_r2".to_string(),
            author_association: "OWNER".to_string(),
            path: "src/main.rs".to_string(),
            line: Some(42),
            original_line: Some(42),
        }
    }

    fn review_view(comment: &ReviewComment) -> ReviewCommentView<'_> {
        ReviewCommentView {
            comment,
            body: comment.body.clone(),
            location: comment.location(),
            updated: false,
        }
    }

    #[test]
    fn no_activity_requires_all_inputs_empty() {
        let out = render_work_on(&issue(), "PR", false, &[], &[]);
        assert!(out.starts_with("No new activity on PR #9"));
    }

    #[test]
    fn review_comments_alone_are_activity() {
        let review = review_comment();
        let out = render_work_on(&issue(), "PR", false, &[], &[review_view(&review)]);
        assert!(out.contains("New inline review comments since you last ran `ghwf work-on`:"));
        assert!(out.contains("**reviewer** at 2026-01-02T00:00:00Z said on `src/main.rs:42`:"));
        assert!(out.contains("> rename this"));
    }

    #[test]
    fn conversation_and_review_sections_compose() {
        let conversation = comment();
        let conversation_view = CommentView {
            comment: &conversation,
            body: conversation.body.clone(),
            updated: false,
        };
        let review = review_comment();
        let out = render_work_on(
            &issue(),
            "PR",
            false,
            &[conversation_view],
            &[review_view(&review)],
        );
        let conversation_at = out
            .find("New comments since")
            .expect("conversation section present");
        let review_at = out
            .find("New inline review comments since")
            .expect("review section present");
        assert!(conversation_at < review_at);
        assert!(out[conversation_at..review_at].contains("<hr>"));
    }

    #[test]
    fn updated_review_comment_is_tagged() {
        let review = review_comment();
        let mut view = review_view(&review);
        view.updated = true;
        let out = render_work_on(&issue(), "PR", false, &[], &[view]);
        assert!(out.contains("said on `src/main.rs:42` (updated):"));
    }
}
