use std::path::{Path, PathBuf};
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

/// Clone `url` as a bare repo at `dest`, optionally borrowing objects from a
/// local `reference` repo instead of fetching them over the network.
///
/// `--single-branch` keeps the bare repo's `refs/heads/*` down to just the
/// default branch — a plain `--bare` clone mirrors every remote branch into
/// `refs/heads/*`, where they would sit frozen forever (fetch `--prune` only
/// tends `refs/remotes/*`) and pollute [`list_local_branches`].
///
/// A reference clone is always dissociated: the reference repo is exactly
/// what a migrating user is likely to delete, so the new repo must never
/// keep depending on it.
pub fn clone_bare(url: &str, dest: &Path, reference: Option<&Path>) -> Result<()> {
    let dest = dest
        .to_str()
        .context("destination path is not valid UTF-8")?;
    let mut args = vec!["clone", "--bare", "--single-branch"];
    // Canonicalize: git resolves a relative reference path against its own
    // cwd, and the canonical form keeps error messages unambiguous.
    let reference = match reference {
        Some(path) => Some(
            path.canonicalize()
                .with_context(|| format!("--reference repo `{}` was not found", path.display()))?,
        ),
        None => None,
    };
    let reference_arg;
    if let Some(path) = &reference {
        reference_arg = path.to_str().context("reference path is not valid UTF-8")?;
        args.extend(["--reference", reference_arg, "--dissociate"]);
    }
    args.extend([url, dest]);
    git(Path::new("."), &args).map(|_| ())
}

/// Configure `repo`'s origin to behave like a normal (working-copy) clone's:
/// the conventional fetch refspec, populated remote-tracking refs, and
/// `origin/HEAD` pointing at the default branch. A bare clone sets none of
/// these up, but ghwf relies on them (`fetch --prune origin` and
/// `origin/<default>` worktree starts).
pub fn setup_conventional_remote(repo: &Path) -> Result<()> {
    git(
        repo,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    )?;
    git(repo, &["fetch", "--prune", "origin"])?;
    git(repo, &["remote", "set-head", "origin", "--auto"]).map(|_| ())
}

/// Whether `dir` is inside a git work tree (false in a bare repo or outside
/// git entirely).
pub fn is_inside_work_tree(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|out| out.trim() == "true")
        .unwrap_or(false)
}

/// Root of the work tree containing `dir`.
pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git(dir, &["rev-parse", "--show-toplevel"])?.trim(),
    ))
}

/// Whether `dir` is (or is inside) a git repository, bare or not.
pub fn is_repo(dir: &Path) -> bool {
    dir.is_dir() && git_ok(dir, &["rev-parse", "--git-dir"])
}

/// Whether `relpath` is already ignored by the repo's gitignore rules.
pub fn is_ignored(repo: &Path, relpath: &str) -> bool {
    git_ok(repo, &["check-ignore", "-q", relpath])
}

/// Resolve a path within the git metadata directory for `dir`, e.g.
/// `info/exclude`. Handles linked worktrees correctly (where `.git` is a file),
/// returning an absolute path.
pub fn git_path(dir: &Path, relative: &str) -> Result<PathBuf> {
    let raw = git(dir, &["rev-parse", "--git-path", relative])?;
    let path = PathBuf::from(raw.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        dir.join(path)
    })
}

/// The URL of the repo's `origin` remote.
pub fn remote_url(repo: &Path) -> Result<String> {
    Ok(git(repo, &["remote", "get-url", "origin"])?
        .trim()
        .to_string())
}

/// Fetch the latest refs from origin, pruning remote-tracking refs whose
/// remote branch is gone.
pub fn fetch(repo: &Path) -> Result<()> {
    git(repo, &["fetch", "--prune", "origin"]).map(|_| ())
}

/// Create a new worktree at `path` on a new `branch` starting from `start`
/// (e.g. `origin/main`).
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, start: &str) -> Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    git(repo, &["worktree", "add", "-b", branch, path, start]).map(|_| ())
}

/// Create a worktree at `path` checked out on the existing local `branch`
/// (unlike [`add_worktree`], which creates a new branch with `-b`).
pub fn add_worktree_for_branch(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    git(repo, &["worktree", "add", path, branch]).map(|_| ())
}

/// Repair the administrative files linking the worktree at `path` to `repo`
/// (and back), after the worktree or the repo has moved on disk. `git worktree
/// add` records absolute paths, so a layout that is built in one directory and
/// then renamed needs this to relink — passing the worktree's new path repairs
/// both directions of the pointer.
pub fn repair_worktree(repo: &Path, path: &Path) -> Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    git(repo, &["worktree", "repair", path]).map(|_| ())
}

/// Path of the worktree (main or linked) that has `branch` checked out, if
/// any. Git allows at most one worktree per branch.
pub fn branch_worktree(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let output = git(repo, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_list(&output, branch))
}

/// Find `branch`'s worktree in `git worktree list --porcelain` output. Blocks
/// are blank-line-separated; the match pairs a `worktree <path>` line with
/// `branch refs/heads/<branch>`. Bare and detached blocks carry no `branch`
/// line and never match.
fn parse_worktree_list(output: &str, branch: &str) -> Option<PathBuf> {
    let want = format!("branch refs/heads/{branch}");
    output.split("\n\n").find_map(|block| {
        let path = block
            .lines()
            .find_map(|line| line.strip_prefix("worktree "))?;
        block
            .lines()
            .any(|line| line == want)
            .then(|| PathBuf::from(path))
    })
}

/// True if `dir`'s working tree has no staged or unstaged changes to tracked
/// files. Untracked files are ignored, unlike [`is_clean`].
pub fn is_tree_clean(dir: &Path) -> Result<bool> {
    Ok(
        git(dir, &["status", "--porcelain", "--untracked-files=no"])?
            .trim()
            .is_empty(),
    )
}

/// Fast-forward `dir`'s checked-out branch to `target` (e.g. `origin/main`),
/// failing if a real merge would be needed.
pub fn merge_ff_only(dir: &Path, target: &str) -> Result<()> {
    git(dir, &["merge", "--ff-only", target]).map(|_| ())
}

/// Merge `target` (e.g. `origin/main`) into `dir`'s checked-out branch, creating
/// a merge commit. On failure (including a conflicted merge) the merge is aborted
/// so the working tree is left clean before the error propagates. Callers only
/// invoke this after proving the merge is clean (see [`would_conflict`]); the
/// abort is a safety net.
pub fn merge(dir: &Path, target: &str) -> Result<()> {
    match git(dir, &["merge", "--no-edit", target]) {
        Ok(_) => Ok(()),
        Err(err) => {
            // Best-effort: undo a half-applied or conflicted merge.
            let _ = git_ok(dir, &["merge", "--abort"]);
            Err(err)
        }
    }
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

/// All local branch names.
pub fn list_local_branches(repo: &Path) -> Result<Vec<String>> {
    Ok(
        git(repo, &["for-each-ref", "refs/heads", "--format=%(refname)"])?
            .lines()
            .filter_map(|line| line.strip_prefix("refs/heads/"))
            .map(str::to_string)
            .collect(),
    )
}

/// All branch names on origin, with the `origin/` prefix stripped and the
/// symbolic `origin/HEAD` skipped.
pub fn list_remote_branches(repo: &Path) -> Result<Vec<String>> {
    Ok(git(
        repo,
        &["for-each-ref", "refs/remotes/origin", "--format=%(refname)"],
    )?
    .lines()
    .filter_map(|line| line.strip_prefix("refs/remotes/origin/"))
    .filter(|name| *name != "HEAD")
    .map(str::to_string)
    .collect())
}

/// The default branch as `dir`'s remote knows it, derived from the
/// `origin/HEAD` symref (e.g. `main`). Local-only: no network, no GitHub API.
/// Every ghwf-managed clone has `origin/HEAD` set (`setup_conventional_remote`
/// runs `remote set-head origin --auto`).
pub fn default_remote_branch(dir: &Path) -> Result<String> {
    let head = git(dir, &["rev-parse", "--abbrev-ref", "origin/HEAD"])?;
    let head = head.trim();
    head.strip_prefix("origin/")
        .map(str::to_string)
        .with_context(|| format!("unexpected origin/HEAD form: `{head}`"))
}

/// Whether merging `base` into the commit at `dir`'s HEAD would conflict. A
/// read-only trial merge: `git merge-tree --write-tree` writes a tree object
/// but never touches the index or working tree.
pub fn would_conflict(dir: &Path, base: &str) -> Result<bool> {
    // merge-tree exits 1 for conflicts *and* for a rev that doesn't resolve
    // (printing nothing to stdout), so pre-validate both revs to disambiguate.
    if rev_parse_ok(dir, "HEAD").is_none() {
        bail!("HEAD does not resolve in {}", dir.display());
    }
    if rev_parse_ok(dir, base).is_none() {
        bail!("`{base}` does not resolve in {}", dir.display());
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["merge-tree", "--write-tree", "HEAD", base])
        .output()
        .context("failed to run `git merge-tree` — is git installed and on PATH?")?;
    match output.status.code() {
        // A clean merge.
        Some(0) => Ok(false),
        // Conflicts: merge-tree prints the conflicted tree OID and entries to
        // stdout. Empty stdout on exit 1 means an error, not a conflict.
        Some(1) if !output.stdout.is_empty() => Ok(true),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("`git merge-tree` failed:\n{}", stderr.trim());
        }
    }
}

/// The commit a rev points at, or `None` when it doesn't resolve.
pub fn rev_parse_ok(repo: &Path, rev: &str) -> Option<String> {
    git(repo, &["rev-parse", "--verify", "--quiet", rev])
        .ok()
        .map(|sha| sha.trim().to_string())
}

/// True if `ancestor` is an ancestor of (or equal to) `descendant`. Any
/// failure to prove ancestry (e.g. an unknown commit) reads as "no".
pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    git_ok(repo, &["merge-base", "--is-ancestor", ancestor, descendant])
}

/// True if `dir`'s working tree contains untracked (and unignored) files.
pub fn has_untracked_files(dir: &Path) -> Result<bool> {
    Ok(git(dir, &["status", "--porcelain"])?
        .lines()
        .any(|line| line.starts_with("??")))
}

/// Delete a local branch. The branch must not be checked out anywhere.
///
/// Uses `-D`: callers have already proven the content is merged, and `-d`'s
/// own merged-into-HEAD check doesn't understand squash merges.
pub fn delete_local_branch(repo: &Path, branch: &str) -> Result<()> {
    git(repo, &["branch", "-D", branch]).map(|_| ())
}

/// Delete `branch` on origin.
pub fn delete_remote_branch(repo: &Path, branch: &str) -> Result<()> {
    git(repo, &["push", "origin", "--delete", branch]).map(|_| ())
}

/// Remove a worktree. Git refuses (errors) when the worktree has tracked
/// changes or untracked files; this never forces.
pub fn remove_worktree(repo: &Path, path: &Path) -> Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    git(repo, &["worktree", "remove", path]).map(|_| ())
}

/// The commit that *added* `relpath` on the current history, or `None` when the
/// path was never added. When the path was added more than once (added, removed,
/// re-added), the oldest such commit is returned.
pub fn commit_that_added(dir: &Path, relpath: &str) -> Result<Option<String>> {
    let out = git(
        dir,
        &["log", "--diff-filter=A", "--format=%H", "--", relpath],
    )?;
    Ok(out.lines().last().map(|line| line.trim().to_string()))
}

/// True if `range` (e.g. `A..HEAD`) contains any merge commit.
pub fn range_has_merges(dir: &Path, range: &str) -> Result<bool> {
    Ok(!git(dir, &["rev-list", "--merges", range])?
        .trim()
        .is_empty())
}

/// True if any commit in `range` touched `relpath`.
pub fn path_touched_in_range(dir: &Path, range: &str, relpath: &str) -> Result<bool> {
    Ok(!git(dir, &["rev-list", range, "--", relpath])?
        .trim()
        .is_empty())
}

/// Replay the commits after `upstream` onto `onto`, dropping `upstream` itself
/// (`git rebase --onto <onto> <upstream>`). The caller is responsible for
/// aborting on failure.
pub fn rebase_onto(dir: &Path, onto: &str, upstream: &str) -> Result<()> {
    git(dir, &["rebase", "--onto", onto, upstream]).map(|_| ())
}

/// Abort an in-progress rebase, best-effort (used to clean up after a failed
/// [`rebase_onto`]).
pub fn rebase_abort(dir: &Path) -> Result<()> {
    git(dir, &["rebase", "--abort"]).map(|_| ())
}

/// Force-push `branch` to origin, refusing to clobber unseen remote work
/// (`--force-with-lease`).
pub fn force_push_with_lease(dir: &Path, branch: &str) -> Result<()> {
    git(dir, &["push", "--force-with-lease", "origin", branch]).map(|_| ())
}

#[cfg(test)]
pub mod tests {
    use super::{
        add_worktree_for_branch, branch_worktree, commit_that_added, default_remote_branch,
        delete_local_branch, delete_remote_branch, fetch, force_push_with_lease,
        has_untracked_files, is_ancestor, is_tree_clean, list_local_branches, list_remote_branches,
        merge, merge_ff_only, parse_worktree_list, path_touched_in_range, range_has_merges,
        rebase_onto, remove_worktree, rev_parse_ok, would_conflict,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn parse_worktree_list_finds_branch_checkout() {
        // Real porcelain shape: a bare block, a branch checkout, a detached
        // checkout.
        let output = "worktree /repos/repo.git\n\
                      bare\n\
                      \n\
                      worktree /repos/worktrees/main\n\
                      HEAD 1111111111111111111111111111111111111111\n\
                      branch refs/heads/main\n\
                      \n\
                      worktree /repos/worktrees/detached\n\
                      HEAD 2222222222222222222222222222222222222222\n\
                      detached\n";
        assert_eq!(
            parse_worktree_list(output, "main"),
            Some(PathBuf::from("/repos/worktrees/main"))
        );
        // Neither the bare block nor the detached block matches anything.
        assert_eq!(parse_worktree_list(output, "master"), None);
    }

    #[test]
    fn parse_worktree_list_requires_exact_branch() {
        let output = "worktree /repos/worktrees/main-2\n\
                      HEAD 1111111111111111111111111111111111111111\n\
                      branch refs/heads/main-2\n";
        assert_eq!(parse_worktree_list(output, "main"), None);
        assert_eq!(
            parse_worktree_list(output, "main-2"),
            Some(PathBuf::from("/repos/worktrees/main-2"))
        );
    }

    /// A unique scratch directory for tests that drive real git.
    pub fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ghwf-git-test-{tag}-{}", std::process::id()));
        // A leftover from a previous run would make git commands misbehave.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run git in `dir` with a fixed identity, panicking on failure.
    pub fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "`git {}` failed in {dir:?}",
            args.join(" ")
        );
    }

    /// Resolve a rev in `dir`, panicking on failure.
    pub fn rev_parse(dir: &Path, rev: &str) -> String {
        super::git(dir, &["rev-parse", rev])
            .unwrap()
            .trim()
            .to_string()
    }

    /// Run git in `dir` and return its trimmed stdout, panicking on failure.
    pub fn git_stdout(dir: &Path, args: &[&str]) -> String {
        super::git(dir, args).unwrap().trim().to_string()
    }

    /// Init a repo at `dir` with one committed file on branch `main`.
    pub fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("file.txt"), "one\n").unwrap();
        run_git(dir, &["add", "file.txt"]);
        run_git(dir, &["commit", "-m", "one"]);
    }

    #[test]
    fn is_tree_clean_ignores_untracked() {
        let root = scratch("clean");
        init_repo(&root);
        assert!(is_tree_clean(&root).unwrap());

        // Untracked files don't count as changes.
        std::fs::write(root.join("untracked.txt"), "x\n").unwrap();
        assert!(is_tree_clean(&root).unwrap());

        // A modified tracked file does.
        std::fs::write(root.join("file.txt"), "two\n").unwrap();
        assert!(!is_tree_clean(&root).unwrap());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn local_branches_list_and_delete() {
        let root = scratch("branches");
        init_repo(&root);
        run_git(&root, &["branch", "extra"]);
        assert_eq!(list_local_branches(&root).unwrap(), ["extra", "main"]);

        delete_local_branch(&root, "extra").unwrap();
        assert_eq!(list_local_branches(&root).unwrap(), ["main"]);
        // The checked-out branch refuses to die.
        assert!(delete_local_branch(&root, "main").is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn remote_branches_roundtrip_through_a_local_origin() {
        let root = scratch("remote");
        let origin = root.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "--bare", "-b", "main"]);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        run_git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        run_git(&repo, &["push", "origin", "main", "main:extra"]);
        run_git(&repo, &["fetch", "origin"]);
        // The symbolic origin/HEAD must be skipped, not listed.
        run_git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        assert_eq!(list_remote_branches(&repo).unwrap(), ["extra", "main"]);

        // Deleting on origin and pruning drops the remote-tracking ref.
        delete_remote_branch(&repo, "extra").unwrap();
        fetch(&repo).unwrap();
        assert_eq!(list_remote_branches(&repo).unwrap(), ["main"]);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rev_parse_ok_and_is_ancestor() {
        let root = scratch("ancestor");
        init_repo(&root);
        let first = rev_parse_ok(&root, "refs/heads/main").unwrap();
        std::fs::write(root.join("file.txt"), "two\n").unwrap();
        run_git(&root, &["commit", "-am", "two"]);
        let second = rev_parse_ok(&root, "refs/heads/main").unwrap();

        assert!(rev_parse_ok(&root, "refs/heads/nonexistent").is_none());
        assert!(is_ancestor(&root, &first, &second));
        // Equality counts as ancestry.
        assert!(is_ancestor(&root, &second, &second));
        assert!(!is_ancestor(&root, &second, &first));
        // An unknown commit fails safe: not an ancestor.
        assert!(!is_ancestor(
            &root,
            "0000000000000000000000000000000000000000",
            &second
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn has_untracked_files_sees_only_untracked() {
        let root = scratch("untracked");
        init_repo(&root);
        assert!(!has_untracked_files(&root).unwrap());

        // A modified tracked file is not "untracked".
        std::fs::write(root.join("file.txt"), "two\n").unwrap();
        assert!(!has_untracked_files(&root).unwrap());

        std::fs::write(root.join("new.txt"), "x\n").unwrap();
        assert!(has_untracked_files(&root).unwrap());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn add_worktree_for_branch_checks_out_existing_branch() {
        let root = scratch("add-existing");
        let origin = root.join("origin");
        std::fs::create_dir(&origin).unwrap();
        init_repo(&origin);
        // A bare repo with a local `main` branch but no checkout of it — the
        // shape `ghwf clone` produces.
        run_git(
            &root,
            &[
                "clone",
                "--bare",
                "--single-branch",
                origin.to_str().unwrap(),
                "bare",
            ],
        );
        let bare = root.join("bare");

        // Check out the existing `main` branch into a worktree — no new branch.
        let wt = root.join("wt");
        add_worktree_for_branch(&bare, &wt, "main").unwrap();
        assert!(wt.join("file.txt").is_file());
        assert_eq!(list_local_branches(&bare).unwrap(), ["main"]);
        // git reports the worktree's absolute (possibly canonicalized) path.
        let reported = branch_worktree(&bare, "main").unwrap().unwrap();
        assert_eq!(reported.canonicalize().unwrap(), wt.canonicalize().unwrap());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn remove_worktree_requires_a_clean_tree() {
        let root = scratch("rm-worktree");
        init_repo(&root);
        let wt = root.join("wt");
        run_git(
            &root,
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );

        // A dirty worktree refuses removal (gc downgrades this to a warning).
        std::fs::write(wt.join("file.txt"), "changed\n").unwrap();
        assert!(remove_worktree(&root, &wt).is_err());

        std::fs::write(wt.join("file.txt"), "one\n").unwrap();
        remove_worktree(&root, &wt).unwrap();
        assert!(!wt.exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Commit `relpath` with `content` and `message` in `dir`.
    fn commit_path(dir: &Path, relpath: &str, content: &str, message: &str) {
        let path = dir.join(relpath);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        run_git(dir, &["add", "--", relpath]);
        run_git(dir, &["commit", "-m", message]);
    }

    #[test]
    fn rebase_drops_the_plan_commit_keeping_later_work() {
        let root = scratch("rebase-drop");
        init_repo(&root);
        // base → plan → two implementation commits, none of which touch the plan.
        commit_path(&root, "plans/1-x.md", "plan\n", "Add plan for #1");
        let plan = rev_parse(&root, "HEAD");
        commit_path(&root, "src/a.rs", "a\n", "impl a");
        commit_path(&root, "src/b.rs", "b\n", "impl b");

        // The plan commit is correctly identified, and its history is linear.
        assert_eq!(
            commit_that_added(&root, "plans/1-x.md").unwrap().as_deref(),
            Some(plan.as_str())
        );
        assert!(!range_has_merges(&root, &format!("{plan}^..HEAD")).unwrap());
        assert!(!path_touched_in_range(&root, &format!("{plan}..HEAD"), "plans/1-x.md").unwrap());

        // Dropping it removes the plan file but keeps the implementation work.
        rebase_onto(&root, &format!("{plan}^"), &plan).unwrap();
        assert!(!root.join("plans/1-x.md").exists());
        assert!(root.join("src/a.rs").exists());
        assert!(root.join("src/b.rs").exists());
        assert!(commit_that_added(&root, "plans/1-x.md").unwrap().is_none());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merge_and_later_modification_are_detected() {
        let root = scratch("rebase-detect");
        init_repo(&root);
        commit_path(&root, "plans/1-x.md", "plan\n", "Add plan for #1");
        let plan = rev_parse(&root, "HEAD");

        // A side branch merged back in introduces a merge commit in the range.
        run_git(&root, &["checkout", "-b", "side"]);
        commit_path(&root, "src/s.rs", "s\n", "side work");
        run_git(&root, &["checkout", "main"]);
        commit_path(&root, "src/m.rs", "m\n", "main work");
        run_git(&root, &["merge", "--no-ff", "-m", "merge side", "side"]);
        assert!(range_has_merges(&root, &format!("{plan}^..HEAD")).unwrap());

        // A later commit that edits the plan file is detected.
        commit_path(&root, "plans/1-x.md", "plan v2\n", "tweak plan");
        assert!(path_touched_in_range(&root, &format!("{plan}..HEAD"), "plans/1-x.md").unwrap());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn force_push_with_lease_updates_origin() {
        let root = scratch("force-push");
        let origin = root.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "--bare", "-b", "main"]);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        run_git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        commit_path(&repo, "plans/1-x.md", "plan\n", "Add plan for #1");
        let plan = rev_parse(&repo, "HEAD");
        commit_path(&repo, "src/a.rs", "a\n", "impl a");
        run_git(&repo, &["push", "-u", "origin", "main"]);

        // Rewrite history, then force-push: origin follows the rewritten tip.
        rebase_onto(&repo, &format!("{plan}^"), &plan).unwrap();
        force_push_with_lease(&repo, "main").unwrap();
        run_git(&repo, &["fetch", "origin"]);
        assert_eq!(rev_parse(&repo, "main"), rev_parse(&repo, "origin/main"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merge_ff_only_fast_forwards_but_rejects_divergence() {
        let root = scratch("ff");
        init_repo(&root);

        // A branch left behind at the first commit, then main advances.
        run_git(&root, &["branch", "behind"]);
        std::fs::write(root.join("file.txt"), "two\n").unwrap();
        run_git(&root, &["commit", "-am", "two"]);

        // Fast-forwarding `behind` to main succeeds.
        run_git(&root, &["checkout", "behind"]);
        merge_ff_only(&root, "main").unwrap();
        assert_eq!(rev_parse(&root, "behind"), rev_parse(&root, "main"));

        // A diverged branch refuses to fast-forward.
        run_git(&root, &["checkout", "-b", "diverged", "HEAD~1"]);
        std::fs::write(root.join("other.txt"), "x\n").unwrap();
        run_git(&root, &["add", "other.txt"]);
        run_git(&root, &["commit", "-m", "diverge"]);
        assert!(merge_ff_only(&root, "main").is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn would_conflict_detects_conflicts_and_clean_merges() {
        let root = scratch("conflict");
        init_repo(&root);

        // A branch that edits the same line main also edits: a conflict.
        run_git(&root, &["checkout", "-b", "feat"]);
        std::fs::write(root.join("file.txt"), "feat\n").unwrap();
        run_git(&root, &["commit", "-am", "feat edit"]);
        run_git(&root, &["checkout", "main"]);
        std::fs::write(root.join("file.txt"), "main\n").unwrap();
        run_git(&root, &["commit", "-am", "main edit"]);

        // HEAD is `main`; merging `feat` in conflicts.
        assert!(would_conflict(&root, "feat").unwrap());

        // A branch touching only a different file merges cleanly.
        run_git(&root, &["checkout", "-b", "clean", "main"]);
        std::fs::write(root.join("other.txt"), "x\n").unwrap();
        run_git(&root, &["add", "other.txt"]);
        run_git(&root, &["commit", "-m", "add other"]);
        run_git(&root, &["checkout", "main"]);
        assert!(!would_conflict(&root, "clean").unwrap());

        // A non-resolving base is an error, not a silent "no conflict".
        assert!(would_conflict(&root, "nonexistent").is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merge_creates_commit_or_aborts_on_conflict() {
        let root = scratch("merge");
        init_repo(&root);

        // `clean` touches a different file; merging it into main is clean and
        // produces a merge commit that contains both sides.
        run_git(&root, &["checkout", "-b", "clean"]);
        std::fs::write(root.join("other.txt"), "x\n").unwrap();
        run_git(&root, &["add", "other.txt"]);
        run_git(&root, &["commit", "-m", "add other"]);
        run_git(&root, &["checkout", "main"]);
        std::fs::write(root.join("file.txt"), "two\n").unwrap();
        run_git(&root, &["commit", "-am", "main edit"]);

        let before = rev_parse(&root, "HEAD");
        merge(&root, "clean").unwrap();
        // HEAD advanced and now contains the merged branch.
        assert_ne!(rev_parse(&root, "HEAD"), before);
        assert!(is_ancestor(&root, "clean", "HEAD"));
        assert!(is_tree_clean(&root).unwrap());

        // A conflicting merge errors and aborts, leaving the tree clean.
        run_git(&root, &["checkout", "-b", "feat", "main"]);
        std::fs::write(root.join("file.txt"), "feat\n").unwrap();
        run_git(&root, &["commit", "-am", "feat edit"]);
        run_git(&root, &["checkout", "main"]);
        std::fs::write(root.join("file.txt"), "main-again\n").unwrap();
        run_git(&root, &["commit", "-am", "main edit again"]);

        let before_conflict = rev_parse(&root, "HEAD");
        assert!(merge(&root, "feat").is_err());
        assert_eq!(rev_parse(&root, "HEAD"), before_conflict);
        assert!(is_tree_clean(&root).unwrap());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn default_remote_branch_reads_origin_head() {
        let root = scratch("origin-head");
        let origin = root.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "--bare", "-b", "main"]);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        run_git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        run_git(&repo, &["push", "origin", "main"]);
        run_git(&repo, &["fetch", "origin"]);
        run_git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        assert_eq!(default_remote_branch(&repo).unwrap(), "main");

        std::fs::remove_dir_all(&root).unwrap();
    }
}
