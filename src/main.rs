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
    // `/proceed` directives are always read from the issue thread.
    let transition = advance_phase(&mut issue_state, &issue_comments);

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
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let digest_pr =
        matches!(phase, state::Phase::Implement | state::Phase::Review) && pr_number.is_some();
    // Inline review comments only exist for a PR digest; `None` for the
    // issue-thread phases, which must leave the cached map untouched.
    let (subject, subject_comments, review_comments, subject_noun) = if digest_pr {
        let pr = pr_number.expect("pr number checked above");
        let pr_arg = pr.to_string();
        let pr_data = github::fetch_issue(&pr_arg, repo_ctx.as_ref())?;
        let pr_comments = github::fetch_comments(&pr_arg, repo_ctx.as_ref())?;
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

    println!("{}", render::render_phase_banner(phase, transition, &body));
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

/// Process any new `/proceed` directives in the comments, advancing the issue's
/// phase in `issue_state`. Returns a description of the transition if one happened.
fn advance_phase(
    issue_state: &mut state::IssueState,
    comments: &[models::Comment],
) -> Option<(state::Phase, state::Phase, Option<String>)> {
    let initial = issue_state.phase;
    let mut trigger = None;

    for comment in comments {
        // Only the user's comments are directives; skip Claude/ghwf-authored ones.
        if render::extract_session_token(&comment.body).is_some() {
            continue;
        }
        if issue_state.consumed_directives.contains(&comment.id) {
            continue;
        }
        if state::is_proceed_directive(&comment.body) {
            // Consume the directive regardless, so it never re-fires.
            issue_state.consumed_directives.insert(comment.id);
            if let Some(next) = issue_state.phase.next() {
                issue_state.phase = next;
                trigger = Some(comment.user.login.clone());
            }
        }
    }

    (issue_state.phase != initial).then_some((initial, issue_state.phase, trigger))
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
