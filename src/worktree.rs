use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Ensure this Claude session is running *inside* `worktree`, hard-erroring
/// otherwise.
///
/// ghwf cannot change a Claude Code session's working directory (it's fixed when
/// `claude` launches), so rather than limp along via absolute paths we refuse and
/// tell the user how to relaunch in the worktree.
///
/// `config_dir` (the `ghwf.toml` directory, when known) anchors the relaunch
/// command at the project root so the printed `cd` target stays short and
/// relative.
pub fn ensure_inside(worktree: &Path, config_dir: Option<&Path>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to determine the current directory")?;
    if is_inside(&cwd, worktree) {
        return Ok(());
    }
    bail!("{}", relaunch_message(worktree, config_dir, &cwd));
}

/// True if `cwd` is `worktree` or a descendant of it.
///
/// Both paths are canonicalized first so symlinks (e.g. macOS `/var` →
/// `/private/var`) don't produce false negatives.
fn is_inside(cwd: &Path, worktree: &Path) -> bool {
    canonical(cwd).starts_with(canonical(worktree))
}

/// Canonicalize `path`, falling back to the path as-is when it can't be resolved
/// (e.g. it doesn't exist yet).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The hard-error message telling the user to relaunch Claude in `worktree`.
///
/// The raw `cd … && claude` is a placeholder for an eventual `ghwf resume
/// <issue>` (tracked in ghwf issue #2); the message is shaped so swapping it in
/// later is trivial.
fn relaunch_message(worktree: &Path, config_dir: Option<&Path>, cwd: &Path) -> String {
    // Prefer a short relative `cd` target anchored at the project root; fall back
    // to the absolute worktree path when the worktree isn't under the config dir.
    let rel = config_dir.and_then(|dir| worktree.strip_prefix(dir).ok());
    let (where_from, target) = match (config_dir, rel) {
        (Some(dir), Some(rel)) => (
            format!(" from the project root (where `ghwf.toml` lives), `{}`", dir.display()),
            rel.display().to_string(),
        ),
        _ => (String::new(), worktree.display().to_string()),
    };

    let wt = worktree.display();
    let here = cwd.display();
    format!(
        "This issue's work happens in the worktree `{wt}`.\n\
         This Claude session is running in `{here}`, and ghwf can't move it.\n\
         Exit Claude and relaunch in the worktree{where_from}:\n\n    cd {target} && claude\n\n\
         then re-run your `ghwf` command. A future `ghwf resume <issue>` will replace this manual step."
    )
}

#[cfg(test)]
mod tests {
    use super::is_inside;
    use std::path::Path;

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
