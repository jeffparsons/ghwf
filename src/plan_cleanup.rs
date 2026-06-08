use std::path::Path;

use anyhow::Result;

use crate::git;

/// The outcome of an attempted plan-commit removal.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// The plan commit was rebased out and the branch force-pushed.
    Removed,
    /// The removal was skipped; the plan is left in place. The string is a
    /// human-readable reason, for a warning.
    Skipped(String),
}

/// Rewrite the commit that added `plan_rel` out of `branch`'s history in
/// `worktree`, then force-push. Every precondition that doesn't hold returns
/// `Removal::Skipped(reason)` rather than an error — this is a best-effort
/// cleanup that must never break the workflow. An `Err` is reserved for an
/// unexpected git failure while probing.
///
/// The rewrite is only attempted when the history above the plan commit is
/// linear and no later commit touched the plan file, so a plain rebase replays
/// cleanly without flattening merges or hitting conflicts.
pub fn remove_plan_commit(worktree: &Path, branch: &str, plan_rel: &str) -> Result<Removal> {
    // A rebase rewrites the working tree; refuse if it isn't clean.
    if !git::is_tree_clean(worktree)? {
        return Ok(Removal::Skipped(
            "the worktree has uncommitted changes".to_string(),
        ));
    }

    let Some(plan_commit) = git::commit_that_added(worktree, plan_rel)? else {
        return Ok(Removal::Skipped(format!(
            "no commit adds `{plan_rel}` on this branch"
        )));
    };

    // `--onto` needs the plan commit's parent as the new base.
    let Some(base) = git::rev_parse_ok(worktree, &format!("{plan_commit}^")) else {
        return Ok(Removal::Skipped(
            "the plan commit has no parent to rebase onto".to_string(),
        ));
    };

    // Merges in the replayed range would be flattened by a plain rebase; leave
    // such branches untouched.
    if git::range_has_merges(worktree, &format!("{base}..HEAD"))? {
        return Ok(Removal::Skipped(
            "the branch contains merge commits".to_string(),
        ));
    }

    // A later commit that also modified the plan file would conflict once the
    // adding commit is dropped.
    if git::path_touched_in_range(worktree, &format!("{plan_commit}..HEAD"), plan_rel)? {
        return Ok(Removal::Skipped(format!(
            "`{plan_rel}` is modified by a later commit"
        )));
    }

    // Drop the plan commit. On failure, clean up the half-done rebase and skip.
    if let Err(err) = git::rebase_onto(worktree, &base, &plan_commit) {
        let _ = git::rebase_abort(worktree);
        return Ok(Removal::Skipped(format!("the rebase failed: {err:#}")));
    }

    // Publish the rewritten branch. A rejected push leaves the local branch
    // rewritten but the remote (and PR) still carrying the plan.
    if let Err(err) = git::force_push_with_lease(worktree, branch) {
        return Ok(Removal::Skipped(format!(
            "the plan commit was dropped locally but the force-push was rejected \
             ({err:#}); the remote branch still has the plan"
        )));
    }

    Ok(Removal::Removed)
}

#[cfg(test)]
mod tests {
    use super::{remove_plan_commit, Removal};
    use crate::git::tests::{init_repo, rev_parse, run_git, scratch};
    use std::path::Path;

    /// Commit `relpath` with `content` and `message` in `dir`.
    fn commit_path(dir: &Path, relpath: &str, content: &str, message: &str) {
        let path = dir.join(relpath);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        run_git(dir, &["add", "--", relpath]);
        run_git(dir, &["commit", "-m", message]);
    }

    /// A repo with `origin` set up and `main` pushed: base → plan → impl. Returns
    /// the worktree path.
    fn repo_with_origin(tag: &str) -> std::path::PathBuf {
        let root = scratch(tag);
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
        commit_path(&repo, "src/a.rs", "a\n", "impl a");
        run_git(&repo, &["push", "-u", "origin", "main"]);
        repo
    }

    #[test]
    fn removes_the_plan_commit_and_pushes() {
        let repo = repo_with_origin("cleanup-ok");
        assert_eq!(
            remove_plan_commit(&repo, "main", "plans/1-x.md").unwrap(),
            Removal::Removed
        );
        assert!(!repo.join("plans/1-x.md").exists());
        assert!(repo.join("src/a.rs").exists());
        // Origin followed the rewrite.
        run_git(&repo, &["fetch", "origin"]);
        assert_eq!(rev_parse(&repo, "main"), rev_parse(&repo, "origin/main"));
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn skips_a_dirty_worktree() {
        let repo = repo_with_origin("cleanup-dirty");
        std::fs::write(repo.join("src/a.rs"), "dirty\n").unwrap();
        assert!(matches!(
            remove_plan_commit(&repo, "main", "plans/1-x.md").unwrap(),
            Removal::Skipped(reason) if reason.contains("uncommitted")
        ));
        assert!(repo.join("plans/1-x.md").exists());
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn skips_when_no_plan_commit() {
        let repo = repo_with_origin("cleanup-noplan");
        assert!(matches!(
            remove_plan_commit(&repo, "main", "plans/9-absent.md").unwrap(),
            Removal::Skipped(reason) if reason.contains("no commit adds")
        ));
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn skips_when_branch_has_a_merge() {
        let repo = repo_with_origin("cleanup-merge");
        run_git(&repo, &["checkout", "-b", "side"]);
        commit_path(&repo, "src/s.rs", "s\n", "side work");
        run_git(&repo, &["checkout", "main"]);
        run_git(&repo, &["merge", "--no-ff", "-m", "merge side", "side"]);
        assert!(matches!(
            remove_plan_commit(&repo, "main", "plans/1-x.md").unwrap(),
            Removal::Skipped(reason) if reason.contains("merge commits")
        ));
        assert!(repo.join("plans/1-x.md").exists());
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    #[test]
    fn skips_when_plan_modified_later() {
        let repo = repo_with_origin("cleanup-modified");
        commit_path(&repo, "plans/1-x.md", "plan v2\n", "tweak plan");
        assert!(matches!(
            remove_plan_commit(&repo, "main", "plans/1-x.md").unwrap(),
            Removal::Skipped(reason) if reason.contains("modified by a later commit")
        ));
        assert!(repo.join("plans/1-x.md").exists());
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }
}
