mod github;
mod models;
mod render;

use anyhow::Result;
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::WorkOn { issue } => work_on(&issue),
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
