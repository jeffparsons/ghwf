mod config;
mod git;
mod github;
mod implement;
mod launch;
mod models;
mod prep;
mod render;
mod seen;
mod state;
mod store;
mod worktree;

use std::io::Read;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use render::{CommentView, ReviewCommentView};

#[derive(Parser)]
#[command(name = "ghwf", about = "GitHub WorkFlow")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Advance the workflow on an issue and report what's new and what to do next.
    WorkOn {
        /// An issue number (resolved against the current repo) or a full GitHub issue URL.
        issue: String,
        /// Work without a dedicated branch/worktree/PR (just write the plan file).
        #[arg(long)]
        no_branch: bool,
    },
    /// Post a comment to an issue (or PR), reading the body from stdin.
    ///
    /// The comment is prefixed with a "Claude says" header and tagged with hidden
    /// metadata identifying the authoring Claude session.
    CreateIssueComment {
        /// An issue number (resolved against the current repo) or a full GitHub issue URL.
        issue: String,
    },
    /// Print the absolute path of the worktree recorded for an issue.
    WorktreePath {
        /// An issue number (resolved against the current repo) or a full GitHub issue URL.
        issue: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::WorkOn { issue, no_branch } => work_on(&issue, no_branch),
        Commands::CreateIssueComment { issue } => create_issue_comment(&issue),
        Commands::WorktreePath { issue } => worktree_path(&issue),
    }
}

/// Print the absolute worktree path recorded for an issue (for scripts and the
/// `/work-on` slash command). Errors if no worktree has been created yet.
fn worktree_path(issue: &str) -> Result<()> {
    let repo_ctx = github::config_repo()?;
    let (owner, repo, number) = github::resolve_issue_ref(issue, repo_ctx.as_ref())?;
    let state = state::load(&owner, &repo, number)?;
    match state.prep.as_ref().and_then(|p| p.worktree_path.as_ref()) {
        Some(path) => {
            println!("{}", path.display());
            Ok(())
        }
        None => bail!(
            "no worktree recorded for issue #{number}; run `ghwf work-on {number}` \
             (in branch mode) to create one."
        ),
    }
}

fn work_on(issue: &str, no_branch: bool) -> Result<()> {
    // Identify this Claude session so we can scope the seen cache and suppress
    // our own comments. Without one we're running outside Claude Code: act as a
    // launcher instead, preparing the worktree and starting Claude in it.
    let session_id = match std::env::var(store::SESSION_ID_ENV) {
        Ok(id) if !id.is_empty() => id,
        _ => return launch::run(issue, no_branch),
    };

    // A discovered ghwf.toml is the source of truth for which repo to operate on.
    let repo_ctx = github::config_repo()?;
    let issue_data = github::fetch_issue(issue, repo_ctx.as_ref())?;
    let issue_comments = github::fetch_comments(issue, repo_ctx.as_ref())?;
    let (owner, repo) = github::parse_owner_repo(&issue_data.html_url)?;
    let number = issue_data.number;

    // Load the issue's workflow state once; mutate and save it at the end.
    let mut issue_state = state::load(&owner, &repo, number)?;

    // Approval directives are honoured from the issue thread and, once a PR
    // exists, its conversation thread too — fetched now, before directive
    // processing, and reused for the digest below.
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let early_pr_comments = match pr_number {
        Some(pr) => Some(github::fetch_comments(&pr.to_string(), repo_ctx.as_ref())?),
        None => None,
    };
    let outcome = advance_phase(&mut issue_state, &issue_comments, early_pr_comments.as_deref());

    // The phase-specific banner body. Prep-and-plan does real work here (and
    // hard-errors if it needs a config that's missing); implement/review are light.
    let phase = issue_state.phase;
    let body = match phase {
        state::Phase::PrePlan => render::PRE_PLAN_BODY.to_string(),
        state::Phase::PrepAndPlan => {
            prep::run(&issue_data, &owner, &repo, number, no_branch, &mut issue_state)?
        }
        state::Phase::Implement => {
            implement::run(&issue_data, &owner, &repo, number, &issue_state)?
        }
        state::Phase::Review => implement::review(&owner, &repo, number, &mut issue_state)?,
    };

    // We didn't hard-need a config (or we'd have errored above); still nudge if
    // it's absent.
    config::warn_if_absent();

    // Record this session as the worktree's session when running inside it, so
    // the outside-Claude launcher can later resume it by id.
    if let Some(prep) = issue_state.prep.as_mut() {
        if let Some(worktree) = prep.worktree_path.clone() {
            if worktree::cwd_is_inside(&worktree) {
                prep.worktree_session_id = Some(session_id.clone());
            }
        }
    }

    state::save(&owner, &repo, number, &issue_state)?;

    // Hard-error if this phase needs the issue's worktree but Claude isn't running
    // inside it. Done after saving so a just-created worktree is already persisted.
    if needs_worktree_guard(phase, &issue_state) {
        let worktree = issue_state
            .prep
            .as_ref()
            .and_then(|p| p.worktree_path.as_ref())
            .expect("guard requires a recorded worktree path");
        let config_dir = config::find()?.map(|located| located.dir);
        worktree::ensure_inside(worktree, config_dir.as_deref(), number)?;
    }

    let my_token = store::session_token(&session_id)?;

    // Choose which thread to digest: during implement/review, the PR conversation
    // thread (review feedback); otherwise the issue thread. Both share the issues
    // comments endpoint, so the machinery below is identical either way.
    // Re-read the PR number: the prep-and-plan phase body may have just opened it.
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let digest_pr =
        matches!(phase, state::Phase::Implement | state::Phase::Review) && pr_number.is_some();
    // Inline review comments only exist for a PR digest; `None` for the
    // issue-thread phases, which must leave the cached map untouched.
    let (subject, subject_comments, review_comments, subject_noun) = if digest_pr {
        let pr = pr_number.expect("pr number checked above");
        let pr_arg = pr.to_string();
        let pr_data = github::fetch_issue(&pr_arg, repo_ctx.as_ref())?;
        // Reuse the comments fetched for directive scanning; the fallback only
        // applies if the PR appeared during this run's phase body.
        let pr_comments = match early_pr_comments {
            Some(comments) => comments,
            None => github::fetch_comments(&pr_arg, repo_ctx.as_ref())?,
        };
        let pr_review_comments = github::fetch_review_comments(&owner, &repo, pr)?;
        (pr_data, pr_comments, Some(pr_review_comments), "PR")
    } else {
        (issue_data, issue_comments, None, "issue")
    };

    let record = seen::load(&session_id, &owner, &repo, number)?;

    let body_hash = store::content_hash(subject.body.as_deref().unwrap_or(""));
    let body_changed = record.issue_body_hash.as_deref() != Some(&body_hash);

    let mut new = Vec::new();
    for comment in &subject_comments {
        // Don't feed this session's own comments back to it.
        if render::extract_session_token(&comment.body).as_deref() == Some(my_token.as_str()) {
            continue;
        }
        let hash = store::content_hash(&comment.body);
        let previous = record.comments.get(&comment.id);
        if previous != Some(&hash) {
            new.push(CommentView {
                comment,
                body: render::strip_ghwf_marker(&comment.body),
                updated: previous.is_some(),
            });
        }
    }

    let mut new_review = Vec::new();
    for comment in review_comments.iter().flatten() {
        // Same own-comment filter as above, for symmetry — though ghwf never
        // authors inline review comments today.
        if render::extract_session_token(&comment.body).as_deref() == Some(my_token.as_str()) {
            continue;
        }
        let hash = store::content_hash(&comment.body);
        let previous = record.review_comments.get(&comment.id);
        if previous != Some(&hash) {
            new_review.push(ReviewCommentView {
                comment,
                body: render::strip_ghwf_marker(&comment.body),
                location: comment.location(),
                updated: previous.is_some(),
            });
        }
    }

    println!(
        "{}",
        render::render_phase_banner(phase, &outcome.transitions, &outcome.notes, &body)
    );
    println!();
    println!(
        "{}",
        render::render_work_on(&subject, subject_noun, body_changed, &new, &new_review)
    );

    // Record the current state so the next run only surfaces later changes.
    let updated = seen::SeenRecord {
        issue_body_hash: Some(body_hash),
        comments: subject_comments
            .iter()
            .map(|c| (c.id, store::content_hash(&c.body)))
            .collect(),
        review_comments: match review_comments.as_ref() {
            Some(comments) => comments
                .iter()
                .map(|c| (c.id, store::content_hash(&c.body)))
                .collect(),
            // Not fetched this run; carry the cached map over unchanged.
            None => record.review_comments,
        },
    };
    seen::save(&session_id, &owner, &repo, number, &updated)?;

    Ok(())
}

/// Whether this phase requires Claude to be inside the issue's worktree.
///
/// Only branch-mode phases that operate on a created worktree qualify:
/// prep-and-plan (Claude must write the plan there) and implement (Claude codes
/// there). Pre-plan, review, and `--no-branch` work don't need it.
fn needs_worktree_guard(phase: state::Phase, issue_state: &state::IssueState) -> bool {
    let Some(prep) = issue_state.prep.as_ref() else {
        return false;
    };
    if prep.no_branch || prep.worktree_path.is_none() {
        return false;
    }
    matches!(phase, state::Phase::PrepAndPlan | state::Phase::Implement)
}

/// What directive processing did this run: phase transitions that fired, and
/// consumed directives that didn't (with why).
#[derive(Default)]
struct AdvanceOutcome {
    transitions: Vec<render::Transition>,
    notes: Vec<render::DirectiveNote>,
}

/// Process any new approval directives on the issue and PR conversation
/// threads, advancing the issue's phase in `issue_state`.
///
/// Every directive in an unconsumed user comment is consumed exactly once; one
/// that doesn't approve the current phase is recorded as a note (stale,
/// premature, or retired `/proceed`) instead of firing.
fn advance_phase(
    issue_state: &mut state::IssueState,
    issue_comments: &[models::Comment],
    pr_comments: Option<&[models::Comment]>,
) -> AdvanceOutcome {
    // Merge both threads chronologically, so successive approvals posted
    // together fire in the order they were written.
    let mut tagged: Vec<(&models::Comment, &'static str)> = issue_comments
        .iter()
        .map(|comment| (comment, "issue"))
        .chain(pr_comments.into_iter().flatten().map(|comment| (comment, "PR")))
        .collect();
    tagged.sort_by(|a, b| a.0.created_at.cmp(&b.0.created_at));

    let mut outcome = AdvanceOutcome::default();
    for (comment, source) in tagged {
        // Only the user's comments are directives; skip Claude/ghwf-authored ones.
        if render::extract_session_token(&comment.body).is_some() {
            continue;
        }
        if issue_state.consumed_directives.contains(&comment.id) {
            continue;
        }
        let Some(directive) = state::parse_directive(&comment.body) else {
            continue;
        };
        // Consume the directive whatever happens next, so it never re-fires.
        issue_state.consumed_directives.insert(comment.id);

        let phase = issue_state.phase;
        let kind = match directive.approves() {
            // Approves the current phase: advance.
            Some(approved) if approved == phase => {
                let to = phase.next().expect("approvable phases have a successor");
                issue_state.phase = to;
                outcome.transitions.push(render::Transition {
                    from: phase,
                    to,
                    command: directive.command(),
                    by: comment.user.login.clone(),
                });
                continue;
            }
            Some(approved) if approved < phase => render::NoteKind::Stale,
            Some(_) => render::NoteKind::Premature,
            None => render::NoteKind::Retired,
        };
        outcome.notes.push(render::DirectiveNote {
            kind,
            command: directive.command(),
            by: comment.user.login.clone(),
            source,
            phase_at: phase,
        });
    }
    outcome
}

fn create_issue_comment(issue: &str) -> Result<()> {
    let mut user_body = String::new();
    std::io::stdin()
        .read_to_string(&mut user_body)
        .map_err(anyhow::Error::from)?;
    if user_body.trim().is_empty() {
        bail!("no comment body provided on stdin");
    }

    // Tag the comment with the authoring session when running under Claude Code.
    let token = match std::env::var(store::SESSION_ID_ENV) {
        Ok(session_id) if !session_id.is_empty() => Some(store::session_token(&session_id)?),
        _ => None,
    };

    let repo_ctx = github::config_repo()?;
    let body = render::build_comment_body(&user_body, token.as_deref());
    let comment = github::post_issue_comment(issue, &body, repo_ctx.as_ref())?;
    println!("{}", render::comment_json(&comment)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::advance_phase;
    use crate::models::{Comment, User};
    use crate::render::NoteKind;
    use crate::state::{IssueState, Phase};

    fn comment(id: u64, body: &str, created_at: &str) -> Comment {
        Comment {
            id,
            user: User {
                login: "user".to_string(),
            },
            body: body.to_string(),
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            html_url: format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
            author_association: "OWNER".to_string(),
        }
    }

    fn state_in(phase: Phase) -> IssueState {
        IssueState {
            phase,
            ..Default::default()
        }
    }

    #[test]
    fn matching_directive_advances_and_consumes() {
        let mut state = state_in(Phase::PrePlan);
        let comments = [comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrepAndPlan);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(outcome.transitions[0].command, "/approve-pre-plan");
        assert!(state.consumed_directives.contains(&1));
        assert!(outcome.notes.is_empty());
    }

    #[test]
    fn pr_thread_directive_advances() {
        let mut state = state_in(Phase::PrepAndPlan);
        let pr = [comment(2, "/approve-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &[], Some(&pr));
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(outcome.transitions[0].by, "user");
    }

    #[test]
    fn duplicate_across_threads_is_stale() {
        let mut state = state_in(Phase::PrepAndPlan);
        let issue = [comment(1, "/approve-plan", "2026-01-01T00:00:00Z")];
        let pr = [comment(2, "/approve-plan", "2026-01-01T00:01:00Z")];
        let outcome = advance_phase(&mut state, &issue, Some(&pr));
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(outcome.notes.len(), 1);
        assert!(matches!(outcome.notes[0].kind, NoteKind::Stale));
        assert_eq!(outcome.notes[0].source, "PR");
        assert!(state.consumed_directives.contains(&2));
    }

    #[test]
    fn premature_directive_is_noted_not_fired() {
        let mut state = state_in(Phase::PrePlan);
        let comments = [comment(1, "/approve-implementation", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(matches!(outcome.notes[0].kind, NoteKind::Premature));
        // Consumed: it must never fire later once the phase catches up.
        assert!(state.consumed_directives.contains(&1));
    }

    #[test]
    fn retired_proceed_is_noted_not_fired() {
        let mut state = state_in(Phase::PrePlan);
        let comments = [comment(1, "/proceed", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(matches!(outcome.notes[0].kind, NoteKind::Retired));
        assert!(state.consumed_directives.contains(&1));
    }

    #[test]
    fn consumed_and_claude_comments_are_skipped() {
        let mut state = state_in(Phase::PrePlan);
        state.consumed_directives.insert(1);
        let claude_body = crate::render::build_comment_body("/approve-pre-plan", Some("tok"));
        let comments = [
            comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z"),
            comment(2, &claude_body, "2026-01-01T00:01:00Z"),
        ];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(outcome.notes.is_empty());
        assert!(!state.consumed_directives.contains(&2));
    }

    #[test]
    fn successive_approvals_advance_twice_in_chronological_order() {
        let mut state = state_in(Phase::PrePlan);
        // The earlier approval arrives via the PR slice and the later via the
        // issue slice: the chronological merge must fire pre-plan's first.
        let issue = [comment(2, "/approve-plan", "2026-01-01T00:01:00Z")];
        let pr = [comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &issue, Some(&pr));
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 2);
        assert_eq!(outcome.transitions[0].command, "/approve-pre-plan");
        assert_eq!(outcome.transitions[1].command, "/approve-plan");
        assert!(outcome.notes.is_empty());
    }
}
