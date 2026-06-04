mod github;
mod models;
mod render;
mod store;

use std::io::Read;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use render::WorkOn;

#[derive(Parser)]
#[command(name = "ghwf", about = "GitHub WorkFlow")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch a GitHub issue and its conversation comments, as normalized JSON.
    ///
    /// Will soon do something more useful than that.
    WorkOn {
        /// An issue number (resolved against the current repo) or a full GitHub issue URL.
        issue: String,
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
        Commands::WorkOn { issue } => work_on(&issue),
        Commands::CreateIssueComment { issue } => create_issue_comment(&issue),
    }
}

fn work_on(issue: &str) -> Result<()> {
    let out = WorkOn {
        issue: github::fetch_issue(issue)?,
        comments: github::fetch_comments(issue)?,
    };
    println!("{}", render::to_json(&out)?);
    Ok(())
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

    let body = render::build_comment_body(&user_body, token.as_deref());
    let comment = github::post_issue_comment(issue, &body)?;
    println!("{}", render::comment_json(&comment)?);
    Ok(())
}
