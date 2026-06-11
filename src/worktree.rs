use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Ensure this Claude session is running *inside* `worktree`, hard-erroring
/// otherwise.
///
/// ghwf cannot change a Claude Code session's working directory (it's fixed when
/// `claude` launches), so rather than limp along via absolute paths we refuse and
/// tell the user how to relaunch in the worktree.
///
/// `config_dir` (the `ghwf.toml` directory, when known) tells the user where the
/// relaunch command will find its config; `owner`/`repo`/`number` identify the
/// issue to relaunch for.
pub fn ensure_inside(
    worktree: &Path,
    config_dir: Option<&Path>,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to determine the current directory")?;
    if is_inside(&cwd, worktree) {
        return Ok(());
    }
    bail!(
        "{}",
        relaunch_message(worktree, config_dir, &cwd, owner, repo, number)
    );
}

/// True if the current working directory is `worktree` or a descendant of it.
pub fn cwd_is_inside(worktree: &Path) -> bool {
    std::env::current_dir()
        .map(|cwd| is_inside(&cwd, worktree))
        .unwrap_or(false)
}

/// True if `cwd` is `worktree` or a descendant of it.
///
/// Both paths are canonicalized first so symlinks (e.g. macOS `/var` →
/// `/private/var`) don't produce false negatives.
pub fn is_inside(cwd: &Path, worktree: &Path) -> bool {
    canonical(cwd).starts_with(canonical(worktree))
}

/// Canonicalize `path`, falling back to the path as-is when it can't be resolved
/// (e.g. it doesn't exist yet).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The hard-error message pointing the user at the outside-Claude launcher,
/// which switches to the worktree and resumes the issue's session.
fn relaunch_message(
    worktree: &Path,
    config_dir: Option<&Path>,
    cwd: &Path,
    owner: &str,
    repo: &str,
    number: u64,
) -> String {
    // Name the project root so the user knows where the launcher will find its
    // `ghwf.toml` (it works from anywhere under it).
    let where_from = config_dir
        .map(|dir| {
            format!(
                " from `{}` (the project root, where `ghwf.toml` lives) or anywhere under it",
                dir.display()
            )
        })
        .unwrap_or_default();

    let wt = worktree.display();
    let here = cwd.display();
    // This command runs from a fresh shell outside the bound session, so a bare
    // number would resolve against that shell's repo. Use the full issue URL so
    // the relaunch is unambiguous regardless of the cwd's git remote.
    let url = format!("https://github.com/{owner}/{repo}/issues/{number}");
    format!(
        "This issue's work happens in the worktree `{wt}`.\n\
         This Claude session is running in `{here}`, and ghwf can't move it.\n\
         Exit Claude, then run{where_from}:\n\n    ghwf work-on {url}\n\n\
         ghwf will switch to the worktree and resume this issue's Claude session \
         (or start one there)."
    )
}

#[cfg(test)]
mod tests {
    use super::{is_inside, relaunch_message};
    use std::path::Path;

    #[test]
    fn relaunch_uses_full_issue_url_not_a_bare_number() {
        let msg = relaunch_message(
            Path::new("/wt"),
            None,
            Path::new("/elsewhere"),
            "o",
            "r",
            7,
        );
        // The relaunch runs outside the bound session, so it must name the repo
        // explicitly — a bare `ghwf work-on 7` would resolve against the cwd.
        assert!(msg.contains("ghwf work-on https://github.com/o/r/issues/7"));
        assert!(!msg.contains("ghwf work-on 7"));
    }

    #[test]
    fn cwd_inside_worktree() {
        assert!(is_inside(
            Path::new("/tmp/wt/sub/dir"),
            Path::new("/tmp/wt")
        ));
        assert!(is_inside(Path::new("/tmp/wt"), Path::new("/tmp/wt")));
    }

    #[test]
    fn cwd_outside_worktree() {
        assert!(!is_inside(Path::new("/tmp/other"), Path::new("/tmp/wt")));
        // A sibling sharing a name prefix must not count as inside.
        assert!(!is_inside(Path::new("/tmp/wt-2"), Path::new("/tmp/wt")));
    }
}
