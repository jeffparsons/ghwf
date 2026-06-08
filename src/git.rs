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

#[cfg(test)]
pub mod tests {
    use super::{
        delete_local_branch, delete_remote_branch, fetch, has_untracked_files, is_ancestor,
        is_tree_clean, list_local_branches, list_remote_branches, merge_ff_only,
        parse_worktree_list, remove_worktree, rev_parse_ok,
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
}
