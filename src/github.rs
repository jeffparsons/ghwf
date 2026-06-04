use std::io::Write;
use std::process::{Command, Stdio};

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

/// Post a comment to an issue's (or PR's) conversation thread.
pub fn post_issue_comment(issue: &str, body: &str) -> Result<Comment> {
    let endpoint = format!("{}/comments", issue_endpoint(issue)?);
    // Send the request body as JSON on stdin so the comment text needs no shell
    // escaping; `gh` forwards it verbatim as the POST body.
    let payload = serde_json::json!({ "body": body }).to_string();
    let json = gh_api_stdin(
        &["--method", "POST", &endpoint, "--input", "-"],
        &payload,
    )?;
    serde_json::from_str(&json).context("failed to parse created-comment JSON from `gh api`")
}

/// Extract `(owner, repo)` from an issue's (or PR's) `html_url`.
///
/// We take only the first two path segments, so this works whether the URL ends
/// in `/issues/N` or `/pull/N`; callers pair it with `Issue.number`.
pub fn parse_owner_repo(html_url: &str) -> Result<(String, String)> {
    let url = Url::parse(html_url)
        .with_context(|| format!("could not parse issue html_url `{html_url}`"))?;
    let segments: Vec<&str> = url
        .path_segments()
        .ok_or_else(|| anyhow!("issue html_url `{html_url}` has no path"))?
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        [owner, repo, ..] => Ok((owner.to_string(), repo.to_string())),
        _ => bail!("issue html_url `{html_url}` is missing owner/repo"),
    }
}

/// The default branch of a repo (e.g. `main`).
pub fn default_branch(owner: &str, repo: &str) -> Result<String> {
    let out = gh(&[
        "repo",
        "view",
        &format!("{owner}/{repo}"),
        "--json",
        "defaultBranchRef",
        "--jq",
        ".defaultBranchRef.name",
    ])?;
    Ok(out.trim().to_string())
}

/// Find an existing PR (any state) whose head is `branch`, returning its number.
pub fn find_pr(owner: &str, repo: &str, branch: &str) -> Result<Option<u64>> {
    let out = gh(&[
        "pr",
        "list",
        "-R",
        &format!("{owner}/{repo}"),
        "--head",
        branch,
        "--state",
        "all",
        "--json",
        "number",
        "--jq",
        ".[0].number // empty",
    ])?;
    match out.trim() {
        "" => Ok(None),
        n => Ok(Some(n.parse().context("could not parse PR number from `gh`")?)),
    }
}

/// Open a draft PR from `head` into `base`, returning the new PR number.
pub fn create_draft_pr(
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
) -> Result<u64> {
    // `gh pr create` prints the new PR's URL; the trailing path segment is its number.
    let url = gh(&[
        "pr", "create", "-R", &format!("{owner}/{repo}"), "--draft", "--base", base, "--head",
        head, "--title", title, "--body", body,
    ])?;
    let number = url
        .trim()
        .rsplit('/')
        .next()
        .and_then(|n| n.parse().ok())
        .with_context(|| format!("could not parse PR number from `gh pr create` output: {url:?}"))?;
    Ok(number)
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

/// Run `gh` with the given arguments and return its stdout.
fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`gh {}` failed:\n{}", args.join(" "), stderr.trim());
    }

    String::from_utf8(output.stdout).context("`gh` returned non-UTF-8 output")
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

/// Run `gh api` with the given trailing arguments, feeding `input` on stdin, and
/// return its stdout.
fn gh_api_stdin(args: &[&str], input: &str) -> Result<String> {
    let mut child = Command::new("gh")
        .arg("api")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;

    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open stdin for `gh`"))?
        .write_all(input.as_bytes())
        .context("failed to write request body to `gh`")?;

    let output = child
        .wait_with_output()
        .context("failed to wait for `gh` to finish")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`gh api {}` failed:\n{}", args.join(" "), stderr.trim());
    }

    String::from_utf8(output.stdout).context("`gh api` returned non-UTF-8 output")
}
