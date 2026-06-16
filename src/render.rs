use anyhow::{Context, Result};

use crate::models::{Comment, Issue, PullRequest, ReviewComment};
use crate::state::{Phase, PrOutcome};

/// Opening shared by every hidden ghwf metadata marker, for detection/stripping.
const MARKER_SCAN_PREFIX: &str = "<!-- ghwf:v1 ";
/// Opening of the session marker embedded in Claude-authored comments.
const SESSION_MARKER_PREFIX: &str = "<!-- ghwf:v1 session=";
/// Closing of the hidden metadata marker.
const MARKER_SUFFIX: &str = " -->";
/// The complete marker embedded in ghwf-authored status comments.
const STATUS_MARKER: &str = "<!-- ghwf:v1 status -->";
/// Opening of the hidden stable id marker on each option line of an `ask`
/// comment. Deliberately outside the `ghwf:v1` family: these markers sit
/// mid-body, and `strip_ghwf_marker` truncates at the first `ghwf:v1` marker.
const OPT_MARKER_PREFIX: &str = "<!-- ghwf-opt:";
/// The hidden marker on the final submit checkbox of an `ask` comment.
const SUBMIT_MARKER: &str = "<!-- ghwf-submit -->";

/// The hidden metadata marker found in a ghwf-posted comment.
pub enum Marker {
    /// Claude-authored via `create-issue-comment`, tagged with the authoring
    /// session's token.
    Session(String),
    /// A ghwf-authored status update.
    Status,
}

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

/// An `ask` question the user has just answered, prepared for the digest.
pub struct OptionSubmission {
    /// "issue thread" / "PR thread", naming where the question lives.
    pub thread_noun: &'static str,
    /// The question comment's URL.
    pub url: String,
    /// The labels of the ticked options.
    pub selected: Vec<String>,
    /// The labels of the un-ticked options.
    pub unselected: Vec<String>,
}

/// Render a single comment (e.g. one just created) as pretty-printed JSON.
pub fn comment_json(comment: &Comment) -> Result<String> {
    serde_json::to_string_pretty(comment).context("failed to serialize comment as JSON")
}

/// Render an issue (e.g. one just created) as pretty-printed JSON.
pub fn issue_json(issue: &Issue) -> Result<String> {
    serde_json::to_string_pretty(issue).context("failed to serialize issue as JSON")
}

/// A human-readable overview of a PR's metadata: a header line with number,
/// title, and state, the URL, then the body (or a placeholder when empty).
pub fn pr_overview(pr: &PullRequest) -> String {
    let state = if pr.draft {
        format!("{}, draft", pr.state)
    } else {
        pr.state.clone()
    };
    let body = match pr.body.as_deref().map(str::trim) {
        Some(body) if !body.is_empty() => body,
        _ => "(no body)",
    };
    format!(
        "#{} {}  ({state})\n{}\n\n{body}",
        pr.number, pr.title, pr.html_url
    )
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
        body.push_str(&format!(
            "\n\n{SESSION_MARKER_PREFIX}{token}{MARKER_SUFFIX}"
        ));
    }
    body
}

/// Assemble the body of a ghwf-authored status comment: a visible attribution
/// header, the status text, and the hidden status marker.
pub fn build_status_comment_body(text: &str) -> String {
    format!("**ghwf:**\n<hr>\n\n{}\n\n{STATUS_MARKER}", text.trim())
}

/// Extract the hidden ghwf marker from a comment body, if present.
pub fn extract_marker(body: &str) -> Option<Marker> {
    if body.contains(STATUS_MARKER) {
        return Some(Marker::Status);
    }
    let start = body.find(SESSION_MARKER_PREFIX)? + SESSION_MARKER_PREFIX.len();
    let rest = &body[start..];
    let end = rest.find(MARKER_SUFFIX)?;
    Some(Marker::Session(rest[..end].to_string()))
}

/// Whether a comment must be hidden from digests and wake decisions: ghwf
/// status updates always (they are machinery, whichever session posted them),
/// and Claude-authored comments only when the current session wrote them.
/// `my_token` is `None` outside a Claude session, where only status comments
/// hide.
pub fn hidden_from_digest(body: &str, my_token: Option<&str>) -> bool {
    match extract_marker(body) {
        Some(Marker::Status) => true,
        Some(Marker::Session(token)) => Some(token.as_str()) == my_token,
        None => false,
    }
}

/// Remove the hidden ghwf marker (and the blank line before it) from a comment
/// body, leaving just the displayable content.
pub fn strip_ghwf_marker(body: &str) -> String {
    match body.find(MARKER_SCAN_PREFIX) {
        Some(idx) => body[..idx].trim_end().to_string(),
        None => body.to_string(),
    }
}

/// One option parsed out of an `ask` comment: its visible label and whether
/// the user has ticked it. The hidden `ghwf-opt` id marker anchors the line as
/// a ghwf-managed option (so a `- [ ]` inside a label can't be mistaken for
/// one); ghwf maps a tick back by label, so the id value itself isn't kept.
pub struct ParsedOption {
    pub label: String,
    pub checked: bool,
}

/// The state of an `ask` comment: its options and its submit checkbox. `submit`
/// is `None` when the comment carries no submit marker — either it isn't an
/// `ask` comment at all, or it has already been rewritten on submission.
pub struct ParsedOptions {
    pub options: Vec<ParsedOption>,
    pub submit: Option<bool>,
}

/// Assemble the inner body of an `ask` comment: the question, a multi-select
/// task list with a hidden stable id on each option, and a final submit
/// checkbox. [`build_comment_body`] wraps this with the attribution header and
/// session marker before it is posted.
pub fn build_options_body(question: &str, options: &[String]) -> String {
    let mut body = format!("{}\n", question.trim());
    for (i, label) in options.iter().enumerate() {
        let id = format!("opt{}", i + 1);
        body.push_str(&format!(
            "\n- [ ] {} {OPT_MARKER_PREFIX}{id}{MARKER_SUFFIX}",
            label.trim()
        ));
    }
    body.push_str(&format!("\n\n- [ ] **Submit my answers** {SUBMIT_MARKER}"));
    body
}

/// Parse a markdown task-list line into its checked state and the text after
/// the checkbox. `None` if the line isn't a `- [ ]` / `- [x]` item.
fn parse_task_line(line: &str) -> Option<(bool, &str)> {
    let rest = line.trim_start().strip_prefix("- [")?;
    let mut chars = rest.chars();
    let checked = match chars.next()? {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    let rest = chars.as_str().strip_prefix("] ")?;
    Some((checked, rest))
}

/// Read the options and submit state out of an `ask` comment body. Option lines
/// are task-list items carrying an [`OPT_MARKER_PREFIX`] id; the submit line is
/// the task-list item carrying [`SUBMIT_MARKER`]. Other lines (the question,
/// the attribution header, the session marker) are ignored.
pub fn parse_options_comment(body: &str) -> ParsedOptions {
    let mut options = Vec::new();
    let mut submit = None;
    for line in body.lines() {
        let Some((checked, rest)) = parse_task_line(line) else {
            continue;
        };
        if rest.contains(SUBMIT_MARKER) {
            submit = Some(checked);
        } else if let Some(start) = rest.find(OPT_MARKER_PREFIX) {
            options.push(ParsedOption {
                label: rest[..start].trim().to_string(),
                checked,
            });
        }
    }
    ParsedOptions { options, submit }
}

/// Rewrite an `ask` comment on submission: replace the submit checkbox line
/// with a plain (non-checkbox) confirmation line, leaving the option lines
/// (with the user's ticks) and the session marker untouched. Dropping the
/// `- [ ]` means the box can no longer be toggled, and [`parse_options_comment`]
/// then reports `submit: None`, so the comment is never re-watched or
/// re-surfaced.
pub fn rewrite_submitted_body(body: &str, when: &str) -> String {
    body.lines()
        .map(|line| match parse_task_line(line) {
            Some((_, rest)) if rest.contains(SUBMIT_MARKER) => {
                format!("_Answers submitted at {when}._")
            }
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One-line reminder, shared across phase banners, to route a question that
/// blocks progress to the thread rather than an interactive prompt.
pub fn question_instruction() -> String {
    "If you need an answer from the user to proceed, never use an interactive \
     prompt (no AskUserQuestion, and don't ask in prose and stop): post the \
     question with `ghwf hand-off --question` (body from stdin) — that \
     flips the issue to \"needs you\" — then `ghwf wait` for the reply. \
     When the answer is a choice among discrete options, use `ghwf ask \
     --option \"…\" --option \"…\"` (question on stdin) instead: ghwf renders the \
     options as checkboxes, appends a submit box, and wakes you only once it's \
     ticked. Offer an \"other / none of these\" option where it fits. \
     If the user replies to only part of what you raised, assume more comments \
     may be coming: answer what arrived and wait again rather than treating the \
     unanswered questions, options, or suggested defaults as settled — only an \
     explicit phase approval does that."
        .to_string()
}

/// One-line reminder, shared by the phase banners where a PR exists, to answer
/// each question in the thread it was asked in rather than defaulting to the
/// issue thread.
pub fn reply_where_asked_instruction() -> String {
    "Answer each question in the place it was asked: a comment on the issue thread on the \
     issue (`ghwf create-issue-comment`), a comment on the PR conversation thread on the PR \
     (`ghwf create-issue-comment <PR#>`), and an inline review comment in its own inline \
     thread (`ghwf reply-review-comment --id <id>`). Blocking questions back to the user and \
     phase hand-offs still go on the issue thread via `ghwf hand-off` / `ghwf ask`."
        .to_string()
}

/// How Claude should wait for the next human response, appended to every
/// banner body that ends in a waiting state.
pub fn wait_instruction() -> String {
    "Once you have posted your comment(s) and have nothing else to do, run \
     `ghwf wait` — it blocks until there is new activity (up to ~9 minutes; \
     give it a 10-minute command timeout). Exit 0 means new activity arrived: run \
     `ghwf work-on` to process it. Exit 2 means nothing yet: run \
     `ghwf wait` again. Do not poll with your own sleep loops."
        .to_string()
}

/// The banner body shown in place of the phase body once the PR has left the
/// open state: the workflow is over, so the wait/work-on loop must stop.
pub fn concluded_body(outcome: PrOutcome, pr_url: Option<&str>, number: u64) -> String {
    let pr = match pr_url {
        Some(url) => format!("The PR ({url})"),
        None => "The PR".to_string(),
    };
    match outcome {
        PrOutcome::Merged => format!(
            "{pr} has been merged — the workflow for issue #{number} is complete.\n\n\
             Stop the wait/work-on loop: do not run `ghwf wait` or \
             `ghwf work-on` again unless the user asks."
        ),
        PrOutcome::Closed => format!(
            "{pr} was closed without being merged — the workflow for issue #{number} \
             has halted.\n\n\
             Surface this to the user and stop the wait/work-on loop: do not run \
             `ghwf wait` or `ghwf work-on` again unless the user asks. \
             (Reopening the PR resumes the workflow on the next `ghwf work-on`.)"
        ),
    }
}

/// Guidance shown to Claude during the pre-plan phase.
pub fn pre_plan_body() -> String {
    format!(
        "Pre-plan — gathering the information needed to write a plan.\n\n\
         Discuss on the issue itself. Post questions and clarifications as comments with \
         `ghwf create-issue-comment`; if an answer is needed before you can \
         proceed, use `ghwf hand-off --question` instead so the issue flips to \
         \"needs you\" (or, for a choice among discrete options, `ghwf ask \
         --option \"…\" --option \"…\"`, which presents them as checkboxes). Either way, \
         never raise an interactive prompt (no AskUserQuestion). \
         A reply that addresses only part of what you asked is not a sign the user is \
         done — assume more may be coming, so respond and keep waiting rather than \
         proceeding on the unanswered points. \
         When you have enough information, hand off \
         with `ghwf hand-off` (body from stdin): a comment that summarises your \
         understanding and clearly states you are ready to write a plan. ghwf appends the \
         approval prompt itself — do not write one.\n\n\
         Do not start planning or advance the workflow yourself. Wait for the user's \
         approval; ghwf will then advance to the prep-and-plan phase.\n\n{}",
        wait_instruction()
    )
}

/// The conflict-resolution block shown above the phase body when the branch
/// conflicts with the base. An instruction to act now (auto-resolve), not a
/// workflow transition — so it leads the banner but is never posted to a
/// thread, and clears itself once the merge is pushed.
pub fn conflict_notice(base: &str, number: u64) -> String {
    format!(
        "⚠️ Merge conflict with `origin/{base}`\n\n\
         The base branch has moved on and the branch for issue #{number} now conflicts \
         with it. Resolve this before other work:\n\n\
         1. In the worktree, run `git merge origin/{base}`.\n\
         2. Resolve the conflicted files, then `git add` them and commit the merge.\n\
         3. Push the branch (the PR updates automatically).\n\n\
         Then carry on below."
    )
}

/// The block shown above the phase body when the base has moved on cleanly but
/// auto-merge is off, so the branch is left behind. Prompts Claude to weigh the
/// new commits against its plan and integrate them. Like [`conflict_notice`], it
/// leads the banner but is never posted to a thread, and clears itself once the
/// branch is up to date.
pub fn base_behind_notice(base: &str, number: u64) -> String {
    format!(
        "ℹ️ `origin/{base}` has moved on\n\n\
         New commits have landed on the base branch since the branch for issue \
         #{number} diverged. Before continuing:\n\n\
         1. Review what changed: `git log HEAD..origin/{base}` (add `-p` for diffs).\n\
         2. Decide whether any of it affects your plan or implementation — a \
         refactor to follow, work that supersedes yours, a new helper to reuse — \
         and adjust if so.\n\
         3. Bring the branch up to date (`git merge origin/{base}`, or rebase) and \
         push (the PR updates automatically).\n\n\
         Then carry on below."
    )
}

/// The block shown above the phase body when ghwf has just auto-merged the base
/// into a behind-but-clean branch (the `auto_merge_base` config). Like
/// [`conflict_notice`], it leads the banner but is never posted to a thread.
pub fn base_merged_notice(base: &str, number: u64) -> String {
    format!(
        "ℹ️ Brought the branch up to date with `origin/{base}`\n\n\
         The base branch had moved on, so ghwf merged `origin/{base}` into the branch \
         for issue #{number} (a clean merge) and pushed it. Review the commits it \
         brought in (`git log -p ORIG_HEAD..HEAD`, or inspect the merge commit) and \
         check whether they change your plan or implementation; adjust if needed. \
         Then carry on below."
    )
}

/// A phase transition that fired this run, for banner reporting.
pub struct Transition {
    pub from: Phase,
    pub to: Phase,
    pub trigger: Trigger,
}

/// What fired a phase transition.
pub enum Trigger {
    /// An approval directive: a typed command, or a 👍 reaction on a ghwf
    /// prompt comment (`via_reaction`).
    Directive {
        /// The canonical spelling of the directive.
        command: &'static str,
        /// Who posted it.
        by: String,
        via_reaction: bool,
    },
    /// The user marked the draft PR ready for review (advances implement).
    PrReady,
}

impl Transition {
    /// The directive command that fired this transition, if it was one.
    fn command(&self) -> Option<&'static str> {
        match self.trigger {
            Trigger::Directive { command, .. } => Some(command),
            Trigger::PrReady => None,
        }
    }
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
    /// True when the directive arrived as a 👍 reaction on a ghwf prompt
    /// comment rather than as a typed command.
    pub via_reaction: bool,
}

/// A comment or 👍 reaction ghwf ignored because its author isn't allow-listed
/// (not the operator, in `allowed_users`, or a repo collaborator).
pub struct IgnoredInput {
    /// The login of whoever posted it.
    pub by: String,
    /// Which conversation thread it was on ("issue" / "PR").
    pub source: &'static str,
    pub kind: IgnoredKind,
}

/// What kind of input was ignored, for the wording of its banner line.
pub enum IgnoredKind {
    Comment,
    Reaction,
}

/// One banner line reporting an ignored, non-allow-listed input.
fn render_ignored(ignored: &IgnoredInput) -> String {
    let IgnoredInput { by, source, kind } = ignored;
    match kind {
        IgnoredKind::Comment => format!(
            "Note: ignored a comment from {by} (on the {source}) — not allow-listed, \
             so it was not surfaced."
        ),
        IgnoredKind::Reaction => format!(
            "Note: ignored a 👍 reaction from {by} (on the {source}) — not allow-listed, \
             so it did not advance the workflow."
        ),
    }
}

/// Render the phase banner shown atop `work-on` output: the current phase, any
/// transitions and directive notes from this run, then the phase-specific `body`.
pub fn render_phase_banner(
    phase: Phase,
    transitions: &[Transition],
    notes: &[DirectiveNote],
    ignored: &[IgnoredInput],
    status_posted: bool,
    body: &str,
) -> String {
    let mut out = format!("Phase: {}", phase.label());

    for transition in transitions {
        out.push_str(&format!("\n{}", render_transition(transition)));
    }

    for note in notes {
        out.push_str(&format!("\n{}", render_note(note)));
    }

    for item in ignored {
        out.push_str(&format!("\n{}", render_ignored(item)));
    }
    if status_posted {
        out.push_str(
            "\nghwf has posted a status update covering the above to the conversation \
             thread(s); do not relay it yourself.",
        );
    }

    out.push_str("\n\n");
    out.push_str(body);
    out
}

/// One line reporting a fired phase transition, shared by the banner and
/// status comments.
fn render_transition(transition: &Transition) -> String {
    let trigger = match &transition.trigger {
        Trigger::Directive {
            command,
            by,
            via_reaction: true,
        } => format!("a 👍 reaction from {by}, equivalent to `{command}`"),
        Trigger::Directive {
            command,
            by,
            via_reaction: false,
        } => format!("`{command}` from {by}"),
        Trigger::PrReady => "the PR being marked ready for review".to_string(),
    };
    format!(
        "Phase advanced: {} → {} (triggered by {trigger}).",
        transition.from.label(),
        transition.to.label(),
    )
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
    // How the directive arrived, for the sentence's subject.
    let what = if note.via_reaction {
        format!("A 👍 reaction (equivalent to `{command}`) from {by}")
    } else {
        format!("`{command}` from {by}")
    };
    // What would advance the workflow from where it stands.
    let next_step = advance_hint(*phase_at);
    match note.kind {
        NoteKind::Stale => format!(
            "Note: {what} (on the {source}) was ignored — the workflow is \
             already past the phase it approves."
        ),
        NoteKind::Premature => format!(
            "Note: {what} (on the {source}) was ignored — the workflow is \
             only in the {} phase; {next_step}.",
            phase_at.label()
        ),
        NoteKind::Retired => format!(
            "Note: {what} (on the {source}) was ignored — `{command}` is \
             retired; the workflow is in the {} phase, and {next_step}.",
            phase_at.label()
        ),
    }
}

/// What advances the workflow from `phase`, for misfire notes.
fn advance_hint(phase: Phase) -> String {
    match (phase.approval_command(), phase) {
        (Some(cmd), _) => format!("the command that advances it is `{cmd}`"),
        (None, Phase::Implement) => "marking the draft PR ready for review advances it".to_string(),
        (None, _) => "there is nothing further to approve".to_string(),
    }
}

/// Render the text of a ghwf status update for the conversation threads: what
/// just happened (transitions and misfired directives), the current phase, and
/// what the next approval command triggers. `None` when there is nothing worth
/// posting.
///
/// A newly observed PR `conclusion` is always worth posting; its prose
/// replaces the phase description, never prompting a further approval.
///
/// A stale note whose command also fired a transition this run is the
/// duplicate-across-threads echo — skipped; the transition line already tells
/// the story.
pub fn render_status_comment(
    phase: Phase,
    transitions: &[Transition],
    notes: &[DirectiveNote],
    intro: bool,
    pr_url: Option<&str>,
    conclusion: Option<PrOutcome>,
) -> Option<String> {
    let notes: Vec<&DirectiveNote> = notes
        .iter()
        .filter(|note| {
            !(matches!(note.kind, NoteKind::Stale)
                && transitions
                    .iter()
                    .any(|t| t.command() == Some(note.command)))
        })
        .collect();
    if !intro && transitions.is_empty() && notes.is_empty() && conclusion.is_none() {
        return None;
    }

    let mut paragraphs: Vec<String> = Vec::new();
    if intro {
        paragraphs.push(
            "ghwf is tracking this issue; status updates like this one are posted as the \
             workflow advances."
                .to_string(),
        );
    }
    for transition in transitions {
        paragraphs.push(render_transition(transition));
    }
    for note in notes {
        paragraphs.push(render_note(note));
    }
    paragraphs.push(match conclusion {
        Some(outcome) => conclusion_status_prose(outcome),
        None => phase_status_prose(phase, pr_url),
    });
    Some(paragraphs.join("\n\n"))
}

/// The user-facing description of a concluded workflow. Must mention no
/// approval command: a concluded status is not a 👍 target.
fn conclusion_status_prose(outcome: PrOutcome) -> String {
    match outcome {
        PrOutcome::Merged => {
            "The PR was merged; the workflow for this issue is **complete**.".to_string()
        }
        PrOutcome::Closed => "The PR was closed without being merged; the workflow has \
             **halted**. Reopening the PR resumes it."
            .to_string(),
    }
}

/// The user-facing description of where the workflow stands and how the next
/// advance will arrive — the single source of that prose for status updates.
///
/// Status updates never prompt for an approval (and so are never 👍 targets):
/// the prompt belongs on the hand-off comment, once there is actually
/// something to approve.
fn phase_status_prose(phase: Phase, pr_url: Option<&str>) -> String {
    match phase {
        Phase::PrePlan => "The workflow is in the **pre-plan** phase: Claude gathers the \
             information needed to write a plan and posts its understanding here. When it \
             has enough, it will hand off and prompt for your approval to advance to \
             prep-and-plan."
            .to_string(),
        Phase::PrepAndPlan => "The workflow is in the **prep-and-plan** phase: Claude is \
             writing the implementation plan; ghwf opens it as a draft PR. Claude will \
             hand off and prompt for your approval when the plan is ready."
            .to_string(),
        Phase::Implement => "The workflow is in the **implement** phase: Claude codes the \
             change in the worktree, pushing to the draft PR as it goes. When it hands \
             off, review the PR and mark it ready for review to advance to the review \
             phase."
            .to_string(),
        Phase::Review => match pr_url {
            Some(url) => format!(
                "The workflow is in the **review** phase: the PR is ready for human \
                 review: {url}\n\n\
                 Merging or closing the PR concludes the workflow."
            ),
            None => "The workflow is in the **review** phase: the work is complete and \
                 awaiting human review."
                .to_string(),
        },
        Phase::Finished => "The workflow is **finished**: the PR was merged and the work \
             is complete."
            .to_string(),
    }
}

/// The next-step paragraph ghwf appends to a hand-off comment — the single
/// source of approval-prompt prose, making the hand-off the thread's 👍
/// target where a command applies. `None` when the phase has nothing to hand
/// off (review: the PR is already with the user).
pub fn hand_off_prompt(phase: Phase, no_branch: bool) -> Option<&'static str> {
    match (phase, no_branch) {
        (Phase::PrePlan, _) => Some(
            "Next: comment `/approve-pre-plan` (alias `/approve-preplan`) — or react 👍 \
             to this comment — to advance to prep-and-plan, where a branch and worktree \
             are created and Claude writes an implementation plan, opened as a draft PR.",
        ),
        (Phase::PrepAndPlan, _) => Some(
            "Next: comment `/approve-plan` (on the issue or the PR) — or react 👍 to \
             this comment — to advance to implement, where Claude codes the change.",
        ),
        (Phase::Implement, false) => Some(
            "Next: when you're happy with the change, mark the draft PR ready for \
             review (the \"Ready for review\" button) to advance to the review phase.",
        ),
        // No draft PR exists to mark ready, so nothing advances the phase
        // mechanically; the issue is wrapped up by hand.
        (Phase::Implement, true) => Some(
            "The work is complete. With `--no-branch` there is no draft PR to mark \
             ready; review the change and close the issue (or merge it yourself) to \
             wrap up.",
        ),
        (Phase::Review | Phase::Finished, _) => None,
    }
}

/// Which conversation thread gets the full status update in this phase: the
/// issue while planning, the PR once code is in motion. The other thread gets
/// a one-line stub linking to it.
pub fn status_primary_is_pr(phase: Phase) -> bool {
    matches!(phase, Phase::Implement | Phase::Review | Phase::Finished)
}

/// Render the one-line stub posted to the secondary conversation thread,
/// pointing at the full status update on the primary one. `primary_noun` is
/// "issue" or "PR"; `full_url` is the posted full comment's html_url.
pub fn render_status_stub(
    transitions: &[Transition],
    primary_noun: &str,
    full_url: &str,
) -> String {
    // A multi-transition run collapses to its endpoints.
    match (transitions.first(), transitions.last()) {
        (Some(first), Some(last)) => format!(
            "Phase advanced: {} → {} — full update: {full_url}",
            first.from.label(),
            last.to.label()
        ),
        _ => format!("Status update posted on the {primary_noun}: {full_url}"),
    }
}

/// Render the markdown digest of what's new or changed across the threads —
/// the issue and, once a PR exists, its conversation thread and inline review
/// comments too. The issue is always the primary subject (header + body); the
/// PR's body is never digested.
pub fn render_work_on(
    issue: &Issue,
    body_changed: bool,
    new_issue: &[CommentView],
    pr_number: Option<u64>,
    new_pr: &[CommentView],
    new_review: &[ReviewCommentView],
    submissions: &[OptionSubmission],
) -> String {
    if !body_changed
        && new_issue.is_empty()
        && new_pr.is_empty()
        && new_review.is_empty()
        && submissions.is_empty()
    {
        let threads = match pr_number {
            Some(pr) => format!("issue #{} \"{}\" or PR #{pr}", issue.number, issue.title),
            None => format!("issue #{} \"{}\"", issue.number, issue.title),
        };
        return format!("No new activity on {threads} since you last ran `ghwf work-on`.");
    }

    let mut out = format!("## #{}: {}  ({})\n", issue.number, issue.title, issue.state);

    if body_changed {
        out.push_str(&format!("\nIssue body by {}:\n\n", issue.user.login));
        out.push_str(&blockquote(issue.body.as_deref().unwrap_or("")));
        out.push('\n');
    }

    let mut prior_section = body_changed;
    push_comment_section(
        &mut out,
        &mut prior_section,
        "New comments on the issue thread since you last ran `ghwf work-on`:",
        new_issue,
    );
    if let Some(pr) = pr_number {
        push_comment_section(
            &mut out,
            &mut prior_section,
            &format!(
                "New comments on the PR (#{pr}) conversation thread since you last ran \
                 `ghwf work-on`:"
            ),
            new_pr,
        );
    }

    if !new_review.is_empty() {
        if prior_section {
            out.push_str("\n<hr>\n");
        }
        prior_section = true;
        out.push_str(
            "\nNew inline review comments since you last ran `ghwf work-on`. Reply to each in \
             its own inline thread:\n",
        );
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
            out.push_str(&format!(
                "\nReply in this thread with `ghwf reply-review-comment --id {}` (body from \
                 stdin).\n",
                view.comment.id
            ));
        }
    }

    if !submissions.is_empty() {
        if prior_section {
            out.push_str("\n<hr>\n");
        }
        out.push_str("\nThe user submitted answers to your options question(s):\n");
        for (i, sub) in submissions.iter().enumerate() {
            if i > 0 {
                out.push_str("\n<hr>\n");
            }
            out.push_str(&format!("\nOn the {} ({}):\n\n", sub.thread_noun, sub.url));
            if sub.selected.is_empty() {
                out.push_str("- Selected: *(none)*\n");
            } else {
                out.push_str("- Selected:\n");
                for label in &sub.selected {
                    out.push_str(&format!("  - {label}\n"));
                }
            }
            if !sub.unselected.is_empty() {
                out.push_str("- Not selected:\n");
                for label in &sub.unselected {
                    out.push_str(&format!("  - {label}\n"));
                }
            }
        }
        out.push_str(
            "\nIf the selection looks contradictory, or an \"other / none of these\" \
             option was picked, treat the question as responded but not resolved and \
             continue the conversation rather than assuming it's settled.\n",
        );
    }

    out.trim_end().to_string()
}

/// Append one thread's new-comments section under `heading`, `<hr>`-separating
/// it from any prior section. `prior_section` tracks whether one was rendered.
fn push_comment_section(
    out: &mut String,
    prior_section: &mut bool,
    heading: &str,
    views: &[CommentView],
) {
    if views.is_empty() {
        return;
    }
    if *prior_section {
        out.push_str("\n<hr>\n");
    }
    *prior_section = true;
    out.push_str(&format!("\n{heading}\n"));
    for (i, view) in views.iter().enumerate() {
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
        base_behind_notice, base_merged_notice, build_comment_body, build_status_comment_body,
        conflict_notice, extract_marker, hand_off_prompt, hidden_from_digest, pr_overview,
        render_phase_banner, render_status_comment, render_status_stub, render_work_on,
        status_primary_is_pr, strip_ghwf_marker, CommentView, DirectiveNote, Marker, NoteKind,
        OptionSubmission, ReviewCommentView, Transition, Trigger,
    };
    use crate::models::{Comment, Head, Issue, PullRequest, ReviewComment, User};
    use crate::state::{Directive, Phase, PrOutcome};

    #[test]
    fn conflict_notice_names_base_command_and_issue() {
        let notice = conflict_notice("main", 45);
        assert!(notice.contains("origin/main"));
        assert!(notice.contains("git merge origin/main"));
        assert!(notice.contains("#45"));
    }

    #[test]
    fn base_behind_notice_prompts_review_and_integrate() {
        let notice = base_behind_notice("main", 45);
        assert!(notice.contains("origin/main"));
        assert!(notice.contains("#45"));
        // Names the review step and the integrate step.
        assert!(notice.contains("git log HEAD..origin/main"));
        assert!(notice.contains("git merge origin/main"));
        assert!(notice.contains("plan or implementation"));
    }

    #[test]
    fn base_merged_notice_prompts_review_of_merged_commits() {
        let notice = base_merged_notice("main", 45);
        assert!(notice.contains("origin/main"));
        assert!(notice.contains("#45"));
        // Confirms the merge but now also prompts a review of what came in.
        assert!(notice.contains("merged"));
        assert!(notice.contains("ORIG_HEAD..HEAD"));
        assert!(notice.contains("plan or implementation"));
    }

    fn pr(body: Option<&str>, draft: bool) -> PullRequest {
        PullRequest {
            number: 53,
            title: "Add a thing".to_string(),
            state: "open".to_string(),
            merged: false,
            draft,
            body: body.map(str::to_string),
            html_url: "https://github.com/o/r/pull/53".to_string(),
            head: Head {
                ref_name: "branch".to_string(),
                sha: "deadbeef".to_string(),
            },
        }
    }

    #[test]
    fn pr_overview_shows_metadata_and_body() {
        let out = pr_overview(&pr(Some("The description."), true));
        assert!(out.contains("#53 Add a thing"));
        assert!(out.contains("(open, draft)"));
        assert!(out.contains("https://github.com/o/r/pull/53"));
        assert!(out.contains("The description."));
    }

    #[test]
    fn pr_overview_placeholders_an_empty_body() {
        assert!(pr_overview(&pr(None, false)).contains("(no body)"));
        assert!(pr_overview(&pr(Some("   "), false)).contains("(no body)"));
        // A ready PR omits the draft marker.
        assert!(!pr_overview(&pr(None, false)).contains("draft"));
    }

    fn note(kind: NoteKind, command: &'static str, phase_at: Phase) -> DirectiveNote {
        DirectiveNote {
            kind,
            command,
            by: "user".to_string(),
            source: "PR",
            phase_at,
            via_reaction: false,
        }
    }

    fn transition(from: Phase, to: Phase, command: &'static str) -> Transition {
        Transition {
            from,
            to,
            trigger: Trigger::Directive {
                command,
                by: "user".to_string(),
                via_reaction: false,
            },
        }
    }

    #[test]
    fn marker_session_round_trips() {
        let body = build_comment_body("hello", Some("tok"));
        assert!(matches!(extract_marker(&body), Some(Marker::Session(t)) if t == "tok"));
    }

    #[test]
    fn marker_status_round_trips_and_strips() {
        let body = build_status_comment_body("update");
        assert!(matches!(extract_marker(&body), Some(Marker::Status)));
        assert!(body.starts_with("**ghwf:**\n<hr>\n\nupdate"));
        assert_eq!(strip_ghwf_marker(&body), "**ghwf:**\n<hr>\n\nupdate");
    }

    #[test]
    fn unmarked_body_has_no_marker() {
        assert!(extract_marker("just text").is_none());
    }

    #[test]
    fn pre_plan_body_includes_wait_instruction() {
        let body = super::pre_plan_body();
        assert!(body.contains("`ghwf wait`"));
        assert!(body.contains("`ghwf work-on`"));
    }

    #[test]
    fn question_instruction_names_the_command_without_a_number() {
        let out = super::question_instruction();
        assert!(out.contains("`ghwf hand-off --question`"));
        assert!(out.contains("`ghwf wait`"));
        assert!(out.contains("AskUserQuestion"));
        // A bare number would resolve against the cwd's repo, so it must not be
        // suggested — the inferred form anchors to the bound issue.
        assert!(!out.contains("hand-off 7"));
    }

    #[test]
    fn question_instruction_warns_against_treating_partial_replies_as_done() {
        let out = super::question_instruction();
        assert!(out.contains("part of what you raised"));
        assert!(out.contains("more comments"));
    }

    #[test]
    fn pre_plan_body_steers_questions_off_interactive_prompts() {
        let body = super::pre_plan_body();
        assert!(body.contains("`ghwf hand-off --question`"));
        assert!(body.contains("AskUserQuestion"));
    }

    #[test]
    fn pre_plan_body_warns_against_treating_partial_replies_as_done() {
        let body = super::pre_plan_body();
        assert!(body.contains("part of what you asked"));
    }

    #[test]
    fn banner_transition_names_command_and_author() {
        let transitions = [transition(
            Phase::PrepAndPlan,
            Phase::Implement,
            "/approve-plan",
        )];
        let out = render_phase_banner(Phase::Implement, &transitions, &[], &[], false, "body");
        assert!(out.contains(
            "Phase advanced: prep-and-plan → implement (triggered by `/approve-plan` from user)."
        ));
    }

    #[test]
    fn banner_premature_note_suggests_current_command() {
        let notes = [note(NoteKind::Premature, "/approve-plan", Phase::PrePlan)];
        let out = render_phase_banner(Phase::PrePlan, &[], &notes, &[], false, "body");
        assert!(out.contains("`/approve-plan` from user (on the PR) was ignored"));
        assert!(out.contains("only in the pre-plan phase"));
        assert!(out.contains("the command that advances it is `/approve-pre-plan`"));
    }

    #[test]
    fn banner_retired_note_in_terminal_phase() {
        let notes = [note(NoteKind::Retired, "/proceed", Phase::Review)];
        let out = render_phase_banner(Phase::Review, &[], &notes, &[], false, "body");
        assert!(out.contains("`/proceed` is retired"));
        assert!(out.contains("there is nothing further to approve"));
    }

    #[test]
    fn banner_retired_approve_implementation_names_the_button() {
        let notes = [note(
            NoteKind::Retired,
            "/approve-implementation",
            Phase::Implement,
        )];
        let out = render_phase_banner(Phase::Implement, &[], &notes, &[], false, "body");
        assert!(out.contains("`/approve-implementation` is retired"));
        assert!(out.contains("marking the draft PR ready for review advances it"));
    }

    #[test]
    fn banner_pr_ready_transition_names_the_trigger() {
        let transitions = [Transition {
            from: Phase::Implement,
            to: Phase::Review,
            trigger: Trigger::PrReady,
        }];
        let out = render_phase_banner(Phase::Review, &transitions, &[], &[], false, "body");
        assert!(out.contains(
            "Phase advanced: implement → review (triggered by the PR being marked ready \
             for review)."
        ));
    }

    #[test]
    fn banner_status_posted_line_only_when_posted() {
        let out = render_phase_banner(Phase::Implement, &[], &[], &[], true, "body");
        assert!(out.contains("posted a status update"));
        let out = render_phase_banner(Phase::Implement, &[], &[], &[], false, "body");
        assert!(!out.contains("posted a status update"));
    }

    #[test]
    fn banner_reaction_transition_names_reactor_and_command() {
        let transitions = [Transition {
            from: Phase::PrePlan,
            to: Phase::PrepAndPlan,
            trigger: Trigger::Directive {
                command: "/approve-pre-plan",
                by: "user".to_string(),
                via_reaction: true,
            },
        }];
        let out = render_phase_banner(Phase::PrepAndPlan, &transitions, &[], &[], false, "body");
        assert!(out.contains(
            "Phase advanced: pre-plan → prep-and-plan (triggered by a 👍 reaction from user, \
             equivalent to `/approve-pre-plan`)."
        ));
    }

    #[test]
    fn banner_reaction_note_names_reactor_and_command() {
        let mut notes = [note(
            NoteKind::Stale,
            "/approve-pre-plan",
            Phase::PrepAndPlan,
        )];
        notes[0].via_reaction = true;
        let out = render_phase_banner(Phase::PrepAndPlan, &[], &notes, &[], false, "body");
        assert!(out.contains(
            "Note: A 👍 reaction (equivalent to `/approve-pre-plan`) from user (on the PR) \
             was ignored"
        ));
    }

    #[test]
    fn status_prose_never_prompts_an_approval() {
        // The prompt belongs on the hand-off comment; a status update must
        // never be a 👍 target.
        for phase in [
            Phase::PrePlan,
            Phase::PrepAndPlan,
            Phase::Implement,
            Phase::Review,
        ] {
            let out = render_status_comment(phase, &[], &[], true, None, None).unwrap();
            assert!(
                crate::state::parse_prompted_directive(&out).is_none(),
                "{} prose mentions an approval command",
                phase.label()
            );
        }
    }

    #[test]
    fn hand_off_prompt_maps_thumbs_to_the_phase_approval() {
        // Pre-plan and prep-and-plan prompts are 👍 targets for their phase's
        // approval; the implement prompt names the button and maps nothing.
        let out = hand_off_prompt(Phase::PrePlan, false).unwrap();
        assert_eq!(
            crate::state::parse_prompted_directive(out).unwrap(),
            Directive::ApprovePrePlan
        );
        let out = hand_off_prompt(Phase::PrepAndPlan, false).unwrap();
        assert_eq!(
            crate::state::parse_prompted_directive(out).unwrap(),
            Directive::ApprovePlan
        );
        let out = hand_off_prompt(Phase::Implement, false).unwrap();
        assert!(out.contains("ready for review"));
        assert!(crate::state::parse_prompted_directive(out).is_none());
        assert!(hand_off_prompt(Phase::Review, false).is_none());
    }

    #[test]
    fn no_branch_implement_hand_off_skips_the_pr_button() {
        let out = hand_off_prompt(Phase::Implement, true).unwrap();
        assert!(out.contains("--no-branch"));
        assert!(!out.contains("button"));
        assert!(crate::state::parse_prompted_directive(out).is_none());
        // Earlier phases are unaffected by the mode.
        assert_eq!(
            hand_off_prompt(Phase::PrePlan, true),
            hand_off_prompt(Phase::PrePlan, false)
        );
    }

    #[test]
    fn status_nothing_to_report_is_none() {
        assert!(render_status_comment(Phase::PrePlan, &[], &[], false, None, None).is_none());
    }

    #[test]
    fn status_transition_names_command_and_next_step() {
        let transitions = [transition(
            Phase::PrepAndPlan,
            Phase::Implement,
            "/approve-plan",
        )];
        let out =
            render_status_comment(Phase::Implement, &transitions, &[], false, None, None).unwrap();
        assert!(out.contains(
            "Phase advanced: prep-and-plan → implement (triggered by `/approve-plan` from user)."
        ));
        assert!(out.contains("**implement** phase"));
        assert!(out.contains("mark it ready for review"));
    }

    #[test]
    fn status_intro_renders_for_every_phase() {
        for phase in [
            Phase::PrePlan,
            Phase::PrepAndPlan,
            Phase::Implement,
            Phase::Review,
        ] {
            let out = render_status_comment(phase, &[], &[], true, None, None).unwrap();
            assert!(out.contains("ghwf is tracking this issue"));
            assert!(out.contains(&format!("**{}** phase", phase.label())));
        }
    }

    #[test]
    fn status_premature_note_names_correct_command() {
        let notes = [note(NoteKind::Premature, "/approve-plan", Phase::PrePlan)];
        let out = render_status_comment(Phase::PrePlan, &[], &notes, false, None, None).unwrap();
        assert!(out.contains("was ignored"));
        assert!(out.contains("the command that advances it is `/approve-pre-plan`"));
    }

    #[test]
    fn status_same_run_duplicate_stale_is_skipped() {
        let transitions = [transition(
            Phase::PrepAndPlan,
            Phase::Implement,
            "/approve-plan",
        )];
        let stale = [note(NoteKind::Stale, "/approve-plan", Phase::Implement)];
        let out = render_status_comment(Phase::Implement, &transitions, &stale, false, None, None)
            .unwrap();
        assert!(!out.contains("was ignored"));
        // Alone — a genuinely late approval, not a same-run echo — it is reported.
        let out = render_status_comment(Phase::Implement, &[], &stale, false, None, None).unwrap();
        assert!(out.contains("was ignored"));
    }

    #[test]
    fn status_review_names_pr_and_terminality() {
        let url = "https://github.com/o/r/pull/9";
        let out = render_status_comment(Phase::Review, &[], &[], true, Some(url), None).unwrap();
        assert!(out.contains(url));
        assert!(out.contains("Merging or closing the PR concludes the workflow."));
        // Without a PR the prose still closes the workflow out.
        let out = render_status_comment(Phase::Review, &[], &[], true, None, None).unwrap();
        assert!(out.contains("awaiting human review"));
    }

    #[test]
    fn concluded_bodies_name_outcome_and_stop_the_loop() {
        let url = "https://github.com/o/r/pull/9";
        let merged = super::concluded_body(PrOutcome::Merged, Some(url), 7);
        assert!(merged.contains(url));
        assert!(merged.contains("has been merged"));
        assert!(merged.contains("complete"));
        let closed = super::concluded_body(PrOutcome::Closed, Some(url), 7);
        assert!(closed.contains("closed without being merged"));
        assert!(closed.contains("halted"));
        // Neither body tells Claude to keep waiting.
        for body in [&merged, &closed] {
            assert!(body.contains("Stop the wait/work-on loop") || body.contains("stop the wait"));
            assert!(!body.contains("Once you have posted"));
        }
        // Without a recorded PR URL the prose still reads naturally.
        let bare = super::concluded_body(PrOutcome::Merged, None, 7);
        assert!(bare.starts_with("The PR has been merged"));
    }

    #[test]
    fn status_conclusion_posts_alone_and_prompts_nothing() {
        // A conclusion is worth posting even with no transitions or notes.
        let out = render_status_comment(
            Phase::Review,
            &[],
            &[],
            false,
            None,
            Some(PrOutcome::Merged),
        )
        .unwrap();
        assert!(out.contains("**complete**"));
        assert!(crate::state::parse_prompted_directive(&out).is_none());
        let out = render_status_comment(
            Phase::Review,
            &[],
            &[],
            false,
            None,
            Some(PrOutcome::Closed),
        )
        .unwrap();
        assert!(out.contains("**halted**"));
        assert!(out.contains("Reopening the PR resumes it."));
        assert!(crate::state::parse_prompted_directive(&out).is_none());
    }

    #[test]
    fn status_primary_thread_follows_phase() {
        assert!(!status_primary_is_pr(Phase::PrePlan));
        assert!(!status_primary_is_pr(Phase::PrepAndPlan));
        assert!(status_primary_is_pr(Phase::Implement));
        assert!(status_primary_is_pr(Phase::Review));
    }

    #[test]
    fn stub_with_transitions_names_endpoints_and_url() {
        let url = "https://github.com/o/r/pull/9#issuecomment-1";
        let transitions = [
            transition(Phase::PrePlan, Phase::PrepAndPlan, "/approve-pre-plan"),
            transition(Phase::PrepAndPlan, Phase::Implement, "/approve-plan"),
        ];
        let out = render_status_stub(&transitions, "PR", url);
        assert_eq!(
            out,
            format!("Phase advanced: pre-plan → implement — full update: {url}")
        );
    }

    #[test]
    fn stub_without_transitions_names_primary_and_url() {
        let url = "https://github.com/o/r/issues/9#issuecomment-1";
        let out = render_status_stub(&[], "issue", url);
        assert_eq!(out, format!("Status update posted on the issue: {url}"));
    }

    #[test]
    fn stub_comment_body_stays_hidden_from_digest() {
        let body = build_status_comment_body(&render_status_stub(&[], "issue", "url"));
        assert!(hidden_from_digest(&body, None));
    }

    fn issue() -> Issue {
        Issue {
            id: 900,
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
            reactions: None,
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

    fn comment_view(comment: &Comment) -> CommentView<'_> {
        CommentView {
            comment,
            body: comment.body.clone(),
            updated: false,
        }
    }

    #[test]
    fn no_activity_requires_all_inputs_empty() {
        let out = render_work_on(&issue(), false, &[], None, &[], &[], &[]);
        assert!(out.starts_with("No new activity on issue #9 \"A PR\" since"));
    }

    #[test]
    fn no_activity_names_both_threads_with_pr() {
        let out = render_work_on(&issue(), false, &[], Some(20), &[], &[], &[]);
        assert!(out.starts_with("No new activity on issue #9 \"A PR\" or PR #20 since"));
    }

    #[test]
    fn review_comments_alone_are_activity() {
        let review = review_comment();
        let out = render_work_on(
            &issue(),
            false,
            &[],
            Some(20),
            &[],
            &[review_view(&review)],
            &[],
        );
        assert!(out.contains("New inline review comments since you last ran `ghwf work-on`."));
        assert!(out.contains("**reviewer** at 2026-01-02T00:00:00Z said on `src/main.rs:42`:"));
        assert!(out.contains("> rename this"));
        // The comment id and a ready-to-use reply command are surfaced so the
        // reply can actually go in the inline thread it was asked in.
        assert!(out.contains("`ghwf reply-review-comment --id 2`"));
    }

    #[test]
    fn issue_pr_and_review_sections_compose_in_order() {
        let issue_comment = comment();
        let pr_comment = comment();
        let review = review_comment();
        let out = render_work_on(
            &issue(),
            false,
            &[comment_view(&issue_comment)],
            Some(20),
            &[comment_view(&pr_comment)],
            &[review_view(&review)],
            &[],
        );
        let issue_at = out
            .find("New comments on the issue thread since")
            .expect("issue section present");
        let pr_at = out
            .find("New comments on the PR (#20) conversation thread since")
            .expect("PR section present");
        let review_at = out
            .find("New inline review comments since")
            .expect("review section present");
        assert!(issue_at < pr_at);
        assert!(pr_at < review_at);
        assert!(out[issue_at..pr_at].contains("<hr>"));
        assert!(out[pr_at..review_at].contains("<hr>"));
    }

    #[test]
    fn body_section_is_always_the_issues() {
        let out = render_work_on(&issue(), true, &[], Some(20), &[], &[], &[]);
        assert!(out.contains("Issue body by author:"));
        assert!(out.contains("> body"));
    }

    #[test]
    fn updated_review_comment_is_tagged() {
        let review = review_comment();
        let mut view = review_view(&review);
        view.updated = true;
        let out = render_work_on(&issue(), false, &[], Some(20), &[], &[view], &[]);
        assert!(out.contains("said on `src/main.rs:42` (updated):"));
    }

    #[test]
    fn build_options_body_numbers_ids_and_appends_submit() {
        let body = super::build_options_body("Which?", &["First".into(), "Second".into()]);
        assert!(body.starts_with("Which?\n"));
        assert!(body.contains("- [ ] First <!-- ghwf-opt:opt1 -->"));
        assert!(body.contains("- [ ] Second <!-- ghwf-opt:opt2 -->"));
        assert!(body.contains("- [ ] **Submit my answers** <!-- ghwf-submit -->"));
    }

    #[test]
    fn parse_options_round_trips_with_mixed_ticks() {
        // A wrapped, partly-ticked comment: `[x]`/`[X]`, leading whitespace, a
        // label containing its own `- [ ]`, and a trailing session marker.
        let body = "**Claude says:**\n<hr>\n\nPick:\n\n\
             - [x] Alpha <!-- ghwf-opt:opt1 -->\n\
             - [ ] Beta <!-- ghwf-opt:opt2 -->\n\
             - [X] Keep a `- [ ]` literal <!-- ghwf-opt:opt3 -->\n\n\
             - [ ] **Submit my answers** <!-- ghwf-submit -->\n\n\
             <!-- ghwf:v1 session=tok -->";
        let parsed = super::parse_options_comment(body);
        assert_eq!(parsed.submit, Some(false));
        let labels: Vec<_> = parsed
            .options
            .iter()
            .map(|o| (o.label.as_str(), o.checked))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("Alpha", true),
                ("Beta", false),
                ("Keep a `- [ ]` literal", true),
            ]
        );
    }

    #[test]
    fn rewrite_submitted_drops_only_the_submit_checkbox() {
        let posted = build_comment_body(
            &super::build_options_body("Pick", &["A".into(), "B".into()]),
            Some("tok"),
        );
        // Simulate the user ticking option A and the submit box.
        let ticked = posted
            .replacen("- [ ] A", "- [x] A", 1)
            .replace("- [ ] **Submit", "- [x] **Submit");
        let rewritten = super::rewrite_submitted_body(&ticked, "2026-06-10T00:05:00Z");
        // Submit checkbox gone, replaced by the confirmation line.
        assert!(rewritten.contains("_Answers submitted at 2026-06-10T00:05:00Z._"));
        assert!(!rewritten.contains("ghwf-submit"));
        // Option ticks and the session marker survive.
        assert!(rewritten.contains("- [x] A <!-- ghwf-opt:opt1 -->"));
        assert!(rewritten.contains("<!-- ghwf:v1 session=tok -->"));
        // A rewritten comment reports no submit box, so it's never re-consumed.
        assert_eq!(super::parse_options_comment(&rewritten).submit, None);
    }

    #[test]
    fn option_markers_do_not_disturb_session_marker_handling() {
        let posted = build_comment_body(
            &super::build_options_body("Pick", &["A".into()]),
            Some("tok"),
        );
        // The inline ghwf-opt / ghwf-submit markers don't trip the ghwf:v1
        // session-marker machinery.
        assert!(matches!(extract_marker(&posted), Some(super::Marker::Session(t)) if t == "tok"));
        let stripped = strip_ghwf_marker(&posted);
        assert!(stripped.contains("- [ ] A <!-- ghwf-opt:opt1 -->"));
        assert!(!stripped.contains("ghwf:v1"));
    }

    #[test]
    fn submissions_render_selected_and_unselected() {
        let sub = OptionSubmission {
            thread_noun: "issue thread",
            url: "https://github.com/o/r/issues/1#issuecomment-9".to_string(),
            selected: vec!["Alpha".to_string()],
            unselected: vec!["Beta".to_string()],
        };
        let out = render_work_on(&issue(), false, &[], None, &[], &[], &[sub]);
        assert!(out.contains("The user submitted answers to your options question(s):"));
        assert!(
            out.contains("On the issue thread (https://github.com/o/r/issues/1#issuecomment-9):")
        );
        assert!(out.contains("- Selected:\n  - Alpha"));
        assert!(out.contains("- Not selected:\n  - Beta"));
        assert!(out.contains("responded but not resolved"));
    }
}
