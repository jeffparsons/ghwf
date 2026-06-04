use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use url::Url;

use crate::models::{Comment, Issue};

/// Fetch an issue (or PR) by number or full GitHub URL.
pub fn fetch_issue(issue: &str) -> Result<Issue> {
    let endpoint = issue_endpoint(issue)?;
    let json = gh_api(&[&endpoint])?;
    serde_json::from_str(&json).context("failed to parse issue JSON from `gh api`")
}

/// Fetch the conversation comments on an issue (or PR), following pagination.
pub fn fetch_comments(issue: &str) -> Result<Vec<Comment>> {
    let endpoint = format!("{}/comments", issue_endpoint(issue)?);
    // `--paginate` follows `Link` headers and merges the paged JSON arrays into one.
    let json = gh_api(&["--paginate", &endpoint])?;
    serde_json::from_str(&json).context("failed to parse comments JSON from `gh api`")
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

/// Run `gh api` with the given trailing arguments and return its stdout.
fn gh_api(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .arg("api")
        .args(args)
        .output()
        .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`gh api {}` failed:\n{}", args.join(" "), stderr.trim());
    }

    String::from_utf8(output.stdout).context("`gh api` returned non-UTF-8 output")
}
