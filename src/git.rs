use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Run a git command with `-C <dir>` and return its stdout, erroring with stderr
/// on a non-zero exit.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("failed to run `git` — is it installed and on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`git {}` failed:\n{}", args.join(" "), stderr.trim());
    }

    String::from_utf8(output.stdout).context("`git` returned non-UTF-8 output")
}

/// Like [`git`], but only reports success/failure (for probe commands that are
/// expected to fail in the normal course of things).
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The URL of the repo's `origin` remote.
pub fn remote_url(repo: &Path) -> Result<String> {
    Ok(git(repo, &["remote", "get-url", "origin"])?.trim().to_string())
}

/// Fetch the latest refs from origin.
pub fn fetch(repo: &Path) -> Result<()> {
    git(repo, &["fetch", "origin"]).map(|_| ())
}

/// Create a new worktree at `path` on a new `branch` starting from `start`
/// (e.g. `origin/main`).
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, start: &str) -> Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    git(repo, &["worktree", "add", "-b", branch, path, start]).map(|_| ())
}

/// True if `relpath` has no pending changes (committed or absent).
pub fn is_clean(dir: &Path, relpath: &str) -> Result<bool> {
    Ok(git(dir, &["status", "--porcelain", "--", relpath])?
        .trim()
        .is_empty())
}

/// True if `relpath` is tracked by git.
pub fn is_tracked(dir: &Path, relpath: &str) -> bool {
    git_ok(dir, &["ls-files", "--error-unmatch", relpath])
}

/// Stage and commit a single file.
pub fn commit_file(dir: &Path, relpath: &str, message: &str) -> Result<()> {
    git(dir, &["add", "--", relpath])?;
    git(dir, &["commit", "-m", message, "--", relpath]).map(|_| ())
}

/// True if `branch`'s local tip matches `origin/<branch>` (i.e. it is pushed and
/// up to date).
pub fn remote_branch_matches(dir: &Path, branch: &str) -> Result<bool> {
    let local = git(dir, &["rev-parse", branch])?;
    let remote_ref = format!("origin/{branch}");
    if !git_ok(dir, &["rev-parse", "--verify", &remote_ref]) {
        return Ok(false);
    }
    let remote = git(dir, &["rev-parse", &remote_ref])?;
    Ok(local.trim() == remote.trim())
}

/// Push `branch` to origin, setting upstream tracking.
pub fn push(dir: &Path, branch: &str) -> Result<()> {
    git(dir, &["push", "-u", "origin", branch]).map(|_| ())
}
