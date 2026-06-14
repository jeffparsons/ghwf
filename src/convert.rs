use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::{clone, git};

/// Convert an existing ordinary (non-bare) clone into ghwf's preferred layout:
/// a container directory holding the bare repo (`<name>.git`), a generated
/// `ghwf.toml`, and a `worktrees/` directory — keeping the original clone
/// intact as a `<name>.pre-ghwf` backup.
///
/// `path` defaults to the current directory, so the conversion may rename the
/// process's own working directory. To make that safe, all the fallible work
/// happens in a temporary sibling directory first; only once it succeeds do two
/// quick same-filesystem renames swap the new layout into place. Everything
/// downstream of [`resolve`] uses absolute paths, so the moving cwd never
/// matters.
pub fn run(path: Option<&Path>) -> Result<()> {
    let start = path.unwrap_or_else(|| Path::new("."));
    let plan = resolve(start)?;
    let default = build_and_swap(&plan)?;
    report(&plan, default.as_deref());
    Ok(())
}

/// The absolute paths a conversion operates on, computed once up front so that
/// nothing downstream depends on the process's (about-to-move) cwd.
struct Plan {
    /// The repo's current top level — the original clone, which becomes the
    /// backup.
    top: PathBuf,
    /// The `origin` URL the new bare repo clones from.
    url: String,
    /// The repo name (the original directory's final component); the bare repo
    /// is `<name>.git`.
    name: String,
    /// Where the original is renamed to.
    backup: PathBuf,
    /// Scratch directory the new layout is built in before being swapped into
    /// `top`.
    temp: PathBuf,
}

/// Validate `start` and compute the [`Plan`], without mutating anything.
fn resolve(start: &Path) -> Result<Plan> {
    if !git::is_inside_work_tree(start) {
        bail!(
            "`{}` is not inside an ordinary (non-bare) git clone — nothing to convert",
            start.display()
        );
    }
    let top = git::toplevel(start)?
        .canonicalize()
        .context("failed to resolve the repository's top-level directory")?;
    // A linked worktree (`git worktree add`) or a submodule has a `.git` *file*
    // rather than a directory; neither is a standalone clone to convert.
    if top.join(".git").is_file() {
        bail!(
            "`{}` is a linked worktree or submodule, not a standalone clone",
            top.display()
        );
    }
    let name = top
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("`{}` has no directory name to reuse", top.display()))?
        .to_string();
    let parent = top
        .parent()
        .with_context(|| format!("`{}` has no parent directory", top.display()))?;
    let backup = parent.join(format!("{name}.pre-ghwf"));
    let temp = parent.join(format!("{name}.ghwf-converting"));
    for (path, what) in [(&backup, "backup"), (&temp, "scratch")] {
        if path.exists() {
            bail!(
                "{what} path `{}` already exists; move it aside first",
                path.display()
            );
        }
    }
    let url = git::remote_url(&top)
        .context("failed to read the `origin` remote URL — does this clone have one?")?;
    Ok(Plan {
        top,
        url,
        name,
        backup,
        temp,
    })
}

/// Build the new layout in `temp`, then swap it into place. The original clone
/// is untouched until the build succeeds, and a swap failure is rolled back, so
/// a failed convert never loses the original. Returns the default branch when
/// its worktree was created (see [`clone::populate`]).
fn build_and_swap(plan: &Plan) -> Result<Option<String>> {
    std::fs::create_dir(&plan.temp).with_context(|| {
        format!(
            "failed to create scratch directory `{}`",
            plan.temp.display()
        )
    })?;
    // Reuse origin's objects from the old clone (dissociating afterwards) so the
    // re-clone of the history is fast, even though the bare repo ends up a
    // pristine single-branch clone.
    let default = match clone::populate(&plan.temp, &plan.name, &plan.url, Some(&plan.top)) {
        Ok(default) => default,
        Err(err) => {
            // Nothing has moved yet, so the original is intact; just clear the
            // half-built scratch directory.
            let _ = std::fs::remove_dir_all(&plan.temp);
            return Err(err);
        }
    };
    swap_into_place(plan)?;
    if let Some(default) = &default {
        // `git worktree add` baked the scratch path into the worktree's admin
        // files; the swap moved those files but not the paths they record, so
        // relink them to the worktree's final location.
        let bare = plan.top.join(format!("{}.git", plan.name));
        let worktree = plan.top.join("worktrees").join(default);
        git::repair_worktree(&bare, &worktree)?;
    }
    Ok(default)
}

/// The two renames that move the new layout into the original path. Both are
/// within the same parent (same filesystem), so they're quick and can't hit
/// `EXDEV`. If the second fails, the first is undone so the original is left
/// where it was.
fn swap_into_place(plan: &Plan) -> Result<()> {
    std::fs::rename(&plan.top, &plan.backup).with_context(|| {
        format!(
            "failed to move the original clone aside to `{}`",
            plan.backup.display()
        )
    })?;
    if let Err(err) = std::fs::rename(&plan.temp, &plan.top) {
        // Put the original back, then drop the scratch build.
        let _ = std::fs::rename(&plan.backup, &plan.top);
        let _ = std::fs::remove_dir_all(&plan.temp);
        return Err(err).with_context(|| {
            format!(
                "failed to move the new layout into `{}`; restored the original clone",
                plan.top.display()
            )
        });
    }
    Ok(())
}

/// Describe the new layout, where the backup is, and what to do next.
/// `default_worktree` is the default branch when its worktree was created.
fn report(plan: &Plan, default_worktree: Option<&str>) {
    println!(
        "Converted `{}` into ghwf's preferred layout:",
        plan.top.display()
    );
    println!(
        "- `{}.git` — the bare repo (a fresh single-branch clone)",
        plan.name
    );
    println!("- `ghwf.toml` — the essentials (`main_repo`, `worktrees_dir`)");
    println!("- `worktrees/` — per-issue worktrees are created here");
    if let Some(default) = default_worktree {
        println!("  - `{default}/` — a checkout of the default branch, ready to use");
    }
    println!();
    println!(
        "Your original clone is preserved at `{}` — any local-only branches,",
        plan.backup.display()
    );
    println!("stashes, or uncommitted changes are still there.");
    println!();
    println!("Next steps:");
    println!("- `cd {}`", plan.top.display());
    println!(
        "- `ghwf config init` to set up the optional extras \
         (priority labels, PR instructions, workflow status labels)"
    );
    println!("- `ghwf work-on <issue>` (or `ghwf next`) to start working");
}

#[cfg(test)]
mod tests {
    use super::{resolve, run};
    use crate::git::tests::{git_stdout, init_repo, run_git, scratch};
    use std::path::{Path, PathBuf};

    /// A fixture origin repo with a second branch besides `main`.
    fn fixture_origin(root: &Path) -> PathBuf {
        let origin = root.join("origin");
        std::fs::create_dir(&origin).unwrap();
        init_repo(&origin);
        run_git(&origin, &["branch", "extra"]);
        origin
    }

    /// An ordinary non-bare clone of `origin` at `<root>/<name>`.
    fn working_clone(root: &Path, origin: &Path, name: &str) -> PathBuf {
        run_git(root, &["clone", origin.to_str().unwrap(), name]);
        root.join(name)
    }

    #[test]
    fn converts_a_working_clone() {
        let root = scratch("convert-happy");
        let origin = fixture_origin(&root);
        let clone = working_clone(&root, &origin, "myrepo");

        run(Some(&clone)).unwrap();

        // The container is now at the original path, holding a bare repo.
        let bare = clone.join("myrepo.git");
        assert_eq!(
            git_stdout(&bare, &["rev-parse", "--is-bare-repository"]),
            "true"
        );
        // Remote-tracking refs are set up exactly as after a working-copy clone.
        for rev in [
            "refs/remotes/origin/main",
            "refs/remotes/origin/extra",
            "refs/remotes/origin/HEAD",
        ] {
            assert!(crate::git::rev_parse_ok(&bare, rev).is_some(), "{rev}");
        }
        // The generated config is in place and loadable.
        let text = std::fs::read_to_string(clone.join("ghwf.toml")).unwrap();
        assert!(toml::from_str::<crate::config::Config>(&text).is_ok());
        // The default branch is checked out under `worktrees/`.
        let default_worktree = clone.join("worktrees").join("main");
        assert!(default_worktree.join("file.txt").is_file());
        // …and its worktree links survived the build-in-scratch-then-rename: git
        // operations work *inside* it (rather than failing with "not a git
        // repository" because the admin files still point at the scratch dir),
        // and the bare repo no longer reports it as prunable.
        assert_eq!(
            git_stdout(&default_worktree, &["rev-parse", "--is-inside-work-tree"]),
            "true"
        );
        let worktrees = git_stdout(&bare, &["worktree", "list", "--porcelain"]);
        assert!(
            !worktrees.lines().any(|line| line == "prunable"),
            "default worktree left prunable: {worktrees}"
        );

        // The original clone is preserved, intact and non-bare, as the backup.
        let backup = root.join("myrepo.pre-ghwf");
        assert_eq!(
            git_stdout(&backup, &["rev-parse", "--is-bare-repository"]),
            "false"
        );
        assert!(backup.join("file.txt").is_file());
        // No scratch directory is left behind.
        assert!(!root.join("myrepo.ghwf-converting").exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn retains_local_only_work_in_the_backup() {
        let root = scratch("convert-retains");
        let origin = fixture_origin(&root);
        let clone = working_clone(&root, &origin, "myrepo");

        // Local-only history and an uncommitted file that exist nowhere on origin.
        run_git(&clone, &["branch", "local-only"]);
        std::fs::write(clone.join("scratch.txt"), "wip\n").unwrap();

        run(Some(&clone)).unwrap();

        // The new bare repo is pristine: only the default branch, no local-only.
        let bare = clone.join("myrepo.git");
        assert_eq!(crate::git::list_local_branches(&bare).unwrap(), ["main"]);

        // The backup still carries the local-only branch and the uncommitted file.
        let backup = root.join("myrepo.pre-ghwf");
        let branches = git_stdout(&backup, &["branch", "--format=%(refname:short)"]);
        assert!(
            branches.lines().any(|b| b == "local-only"),
            "backup branches: {branches}"
        );
        assert!(backup.join("scratch.txt").is_file());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rolls_back_when_the_clone_fails() {
        let root = scratch("convert-rollback");
        let origin = fixture_origin(&root);
        let clone = working_clone(&root, &origin, "myrepo");

        // Remove origin so the re-clone's fetch can't reach it and `populate`
        // fails partway through.
        std::fs::remove_dir_all(&origin).unwrap();

        assert!(run(Some(&clone)).is_err());

        // The original clone is left exactly where it was, untouched…
        assert_eq!(
            git_stdout(&clone, &["rev-parse", "--is-bare-repository"]),
            "false"
        );
        assert!(clone.join("file.txt").is_file());
        // …and neither the backup nor the scratch directory survives.
        assert!(!root.join("myrepo.pre-ghwf").exists());
        assert!(!root.join("myrepo.ghwf-converting").exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_non_convertible_inputs() {
        let root = scratch("convert-rejects");
        let origin = fixture_origin(&root);

        // A plain directory that isn't a repo at all.
        let plain = root.join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert!(resolve(&plain).is_err());

        // A bare repo is not an ordinary clone.
        let bare = root.join("bare.git");
        run_git(
            &root,
            &["clone", "--bare", origin.to_str().unwrap(), "bare.git"],
        );
        assert!(resolve(&bare).is_err());

        // A pre-existing backup blocks the conversion (and leaves the clone alone).
        let clone = working_clone(&root, &origin, "myrepo");
        std::fs::create_dir(root.join("myrepo.pre-ghwf")).unwrap();
        assert!(resolve(&clone).is_err());
        assert!(clone.join("file.txt").is_file());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
