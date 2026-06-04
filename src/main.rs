mod config;
mod git;
mod github;
mod models;
mod prep;
mod render;
mod seen;
mod state;
mod store;

use std::io::Read;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use render::CommentView;

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::WorkOn { issue, no_branch } => work_on(&issue, no_branch),
        Commands::CreateIssueComment { issue } => create_issue_comment(&issue),
    }
}

fn work_on(issue: &str, no_branch: bool) -> Result<()> {
    // A discovered ghwf.toml is the source of truth for which repo to operate on.
    let repo_ctx = github::config_repo()?;
    let issue_data = github::fetch_issue(issue, repo_ctx.as_ref())?;
    let comments = github::fetch_comments(issue, repo_ctx.as_ref())?;
    let (owner, repo) = github::parse_owner_repo(&issue_data.html_url)?;
    let number = issue_data.number;

    // Load the issue's workflow state once; mutate and save it at the end.
    let mut issue_state = state::load(&owner, &repo, number)?;
    let transition = advance_phase(&mut issue_state, &comments);

    // The phase-specific body. The prep-and-plan phase does real work here, and
    // hard-errors if it needs a config that's missing.
    let phase = issue_state.phase;
    let body = match phase {
        state::Phase::PrePlan => render::PRE_PLAN_BODY.to_string(),
        state::Phase::PrepAndPlan => {
            prep::run(&issue_data, &owner, &repo, number, no_branch, &mut issue_state)?
        }
        state::Phase::Implement => render::IMPLEMENT_BODY.to_string(),
    };

    // We didn't hard-need a config (or we'd have errored above); still nudge if
    // it's absent.
    config::warn_if_absent();

    state::save(&owner, &repo, number, &issue_state)?;

    // Identify this Claude session so we can scope the seen cache and suppress our
    // own comments. Without a session (run outside Claude Code) we show everything
    // and persist nothing.
    let session_id = match std::env::var(store::SESSION_ID_ENV) {
        Ok(id) if !id.is_empty() => Some(id),
        _ => None,
    };
    let my_token = match &session_id {
        Some(id) => Some(store::session_token(id)?),
        None => None,
    };

    let record = match &session_id {
        Some(id) => seen::load(id, &owner, &repo, number)?,
        None => seen::SeenRecord::default(),
    };

    let issue_body_hash = store::content_hash(issue_data.body.as_deref().unwrap_or(""));
    let issue_changed = record.issue_body_hash.as_deref() != Some(&issue_body_hash);

    let mut new = Vec::new();
    for comment in &comments {
        // Don't feed this session's own comments back to it.
        if my_token.is_some() && render::extract_session_token(&comment.body) == my_token {
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

    println!("{}", render::render_phase_banner(phase, transition, &body));
    println!();
    println!("{}", render::render_work_on(&issue_data, issue_changed, &new));

    // Record the current state so the next run only surfaces later changes.
    if let Some(id) = &session_id {
        let updated = seen::SeenRecord {
            issue_body_hash: Some(issue_body_hash),
            comments: comments
                .iter()
                .map(|c| (c.id, store::content_hash(&c.body)))
                .collect(),
        };
        seen::save(id, &owner, &repo, number, &updated)?;
    }

    Ok(())
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
