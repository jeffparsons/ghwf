use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use url::Url;

#[derive(Parser)]
#[command(name = "ghwf", about = "GitHub WorkFlow")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch a GitHub issue and print its JSON.
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
    let endpoint = issue_endpoint(issue)?;
    let json = gh_api(&endpoint)?;
    print!("{json}");
    Ok(())
}

/// Resolve an issue argument to a `gh api` issues endpoint path.
///
/// A bare number is left for `gh` to resolve against the current repo via its
/// `{owner}`/`{repo}` placeholders; a full URL is parsed into owner/repo/number.
fn issue_endpoint(arg: &str) -> Result<String> {
    if arg.parse::<u64>().is_ok() {
        return Ok(format!("repos/{{owner}}/{{repo}}/issues/{arg}"));
    }

    let url = Url::parse(arg)
        .with_context(|| format!("`{arg}` is neither an issue number nor a valid URL"))?;

    if url.host_str() != Some("github.com") {
        bail!("`{arg}` is not a github.com issue URL");
    }

    let segments: Vec<&str> = url
        .path_segments()
        .ok_or_else(|| anyhow!("`{arg}` has no path"))?
        .filter(|s| !s.is_empty())
        .collect();

    // Expect `/owner/repo/issues/number`.
    match segments.as_slice() {
        [owner, repo, "issues", number] if number.parse::<u64>().is_ok() => {
            Ok(format!("repos/{owner}/{repo}/issues/{number}"))
        }
        _ => bail!("`{arg}` is not a github.com issue URL of the form owner/repo/issues/number"),
    }
}

/// Run `gh api <endpoint>` and return its stdout.
fn gh_api(endpoint: &str) -> Result<String> {
    let output = Command::new("gh")
        .args(["api", endpoint])
        .output()
        .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`gh api {endpoint}` failed:\n{}", stderr.trim());
    }

    String::from_utf8(output.stdout).context("`gh api` returned non-UTF-8 output")
}
