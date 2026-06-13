use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::models::BranchPr;
use crate::{config, git, github, state, store, worktree};

/// Delete branches (local and remote) whose PRs have been merged, as long as
/// the branch tip is exactly what got merged into the default branch, plus
/// their worktrees when the working tree is clean. Anything suspicious is
/// warned about and left alone; nothing is ever force-deleted.
pub fn run(dry_run: bool) -> Result<()> {
    let (owner, repo_name) = github::repo_or_cwd()?;
    // Git commands run from the configured main repo; without a config, the
    // current directory's repo.
    let repo = match config::find()? {
        Some(located) => located.main_repo_path(),
        None => PathBuf::from("."),
    };
    let default = github::default_branch(&owner, &repo_name)?;

    // Judge against current facts: fresh remote tips, current merge state,
    // and (via --prune) no remote-tracking refs for already-deleted branches.
    println!("Fetching from origin…");
    git::fetch(&repo)?;

    // Candidates: every local and origin branch except the default branch.
    let mut candidates = git::list_local_branches(&repo)?;
    candidates.extend(git::list_remote_branches(&repo)?);
    candidates.sort();
    candidates.dedup();
    candidates.retain(|branch| *branch != default);

    let default_ref = format!("refs/remotes/origin/{default}");
    let mut quiet = true;
    for branch in &candidates {
        let prs = github::branch_prs(&owner, &repo_name, branch)?;
        // No merged PR (or still-active work): not garbage, nothing to say.
        let Some(pr) = pick_merged_pr(&prs) else {
            continue;
        };
        let facts = gather(&repo, branch)?;
        let merge_landed = pr
            .merge_commit
            .as_ref()
            .is_some_and(|commit| git::is_ancestor(&repo, &commit.oid, &default_ref));
        let verdict = classify(branch, &facts, &pr.head_ref_oid, merge_landed);
        quiet &= execute(&repo, &owner, &repo_name, branch, &facts, &verdict, dry_run)?;
    }

    if quiet {
        println!("Nothing to collect.");
    }
    Ok(())
}

/// Run garbage collection automatically when this repo has opted in
/// (`auto_collect_garbage`) and at least the configured interval has elapsed
/// since the last automatic run. Best-effort: failures are warned about, never
/// propagated. `owner`/`repo` key the per-repo throttle and should be the code
/// repo GC acts on (which [`run`] resolves to independently).
pub fn run_periodic(config: &config::Config, owner: &str, repo: &str) {
    if !config.auto_collect_garbage {
        return;
    }
    let interval_secs = config.auto_collect_garbage_interval_hours.saturating_mul(3600);
    let now = state::now_epoch();
    if !is_due(read_last_run(owner, repo), now, interval_secs) {
        return;
    }
    println!("Running periodic garbage collection…");
    if let Err(err) = run(false) {
        eprintln!("warning: periodic garbage collection failed: {err:#}");
    }
    // Stamp regardless of GC's outcome, so a persistently-failing GC can't fire
    // on every merge.
    if let Err(err) = stamp_last_run(owner, repo, now) {
        eprintln!("warning: failed to record periodic GC timestamp: {err:#}");
    }
}

/// Whether an automatic GC is due, given the last run time, the current time,
/// and the minimum gap. Never run → due; clock skew (`now` < `last`) → not due.
fn is_due(last_run: Option<u64>, now: u64, interval_secs: u64) -> bool {
    match last_run {
        None => true,
        Some(last) => now.saturating_sub(last) >= interval_secs,
    }
}

/// Path to the per-repo timestamp recording the last automatic GC run.
fn last_run_path(owner: &str, repo: &str) -> Result<PathBuf> {
    Ok(store::data_dir()?
        .join("gc")
        .join(owner)
        .join(repo)
        .join("last-run"))
}

/// The epoch-seconds timestamp of the last automatic GC for this repo, or `None`
/// when there is no record (or it is unreadable or garbled — treated as
/// "never run", so GC simply runs).
fn read_last_run(owner: &str, repo: &str) -> Option<u64> {
    let path = last_run_path(owner, repo).ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Record `now` as the last automatic-GC time for this repo.
fn stamp_last_run(owner: &str, repo: &str, now: u64) -> Result<()> {
    let path = last_run_path(owner, repo)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, now.to_string())?;
    Ok(())
}

/// The merged PR to judge a branch against: the latest-numbered merged one.
/// `None` when any PR is still open (the branch is active work, not garbage)
/// or when none was ever merged.
fn pick_merged_pr(prs: &[BranchPr]) -> Option<&BranchPr> {
    if prs.iter().any(|pr| pr.state == "OPEN") {
        return None;
    }
    prs.iter()
        .filter(|pr| pr.state == "MERGED")
        .max_by_key(|pr| pr.number)
}

/// The observed state of one candidate branch, gathered up front so the
/// decision in [`classify`] is pure data-in, data-out.
struct BranchFacts {
    local_tip: Option<String>,
    remote_tip: Option<String>,
    worktree: Option<WorktreeFacts>,
}

/// The observed state of a candidate branch's worktree.
struct WorktreeFacts {
    path: PathBuf,
    status: TreeStatus,
    // The main worktree (the main repo's own checkout) is never removed.
    is_main: bool,
    // Neither is the worktree this command is running inside.
    contains_cwd: bool,
}

/// A working tree's cleanliness, as far as removal safety cares.
enum TreeStatus {
    Clean,
    // No changes to tracked files, but untracked files present — git would
    // refuse the removal, and they may be unsaved work.
    UntrackedOnly,
    TrackedChanges,
}

/// Collect a branch's local tip, remote tip, and worktree state.
fn gather(repo: &Path, branch: &str) -> Result<BranchFacts> {
    let local_tip = git::rev_parse_ok(repo, &format!("refs/heads/{branch}"));
    let remote_tip = git::rev_parse_ok(repo, &format!("refs/remotes/origin/{branch}"));
    // Only a local branch can be checked out somewhere.
    let worktree = match local_tip {
        Some(_) => match git::branch_worktree(repo, branch)? {
            Some(path) => {
                let status = if !git::is_tree_clean(&path)? {
                    TreeStatus::TrackedChanges
                } else if git::has_untracked_files(&path)? {
                    TreeStatus::UntrackedOnly
                } else {
                    TreeStatus::Clean
                };
                Some(WorktreeFacts {
                    is_main: same_path(&path, repo),
                    contains_cwd: worktree::cwd_is_inside(&path),
                    status,
                    path,
                })
            }
            None => None,
        },
        None => None,
    };
    Ok(BranchFacts {
        local_tip,
        remote_tip,
        worktree,
    })
}

/// True if two paths name the same directory, resolving symlinks.
fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// What to do about one branch with a merged PR: the deletions that are safe,
/// and the warnings explaining everything kept.
#[derive(Default)]
struct Verdict {
    remove_worktree: bool,
    delete_local: bool,
    delete_remote: bool,
    // Set when nothing of the branch will remain, so the issue state file
    // that recorded it can go too.
    delete_state: bool,
    warnings: Vec<String>,
}

/// Decide a branch's fate. `merged_tip` is the head commit GitHub merged
/// (the PR's frozen `headRefOid`); `merge_landed` is whether the PR's merge
/// commit is an ancestor of the default branch.
///
/// Local and remote are judged independently: whichever side still matches
/// the merged tip is deleted, and whichever diverged is kept (it holds the
/// extra content) with a warning.
fn classify(branch: &str, facts: &BranchFacts, merged_tip: &str, merge_landed: bool) -> Verdict {
    let mut verdict = Verdict::default();

    if !merge_landed {
        verdict.warnings.push(format!(
            "branch `{branch}` has a merged PR, but the merge is not on the default branch; \
             leaving it alone"
        ));
        return verdict;
    }

    if let Some(remote) = &facts.remote_tip {
        if remote == merged_tip {
            verdict.delete_remote = true;
        } else {
            verdict.warnings.push(format!(
                "`origin/{branch}` has different/extra content than what was merged; keeping it"
            ));
        }
    }

    if let Some(local) = &facts.local_tip {
        if local != merged_tip {
            verdict.warnings.push(format!(
                "local branch `{branch}` has different/extra content than what was merged; \
                 keeping it"
            ));
        } else {
            match &facts.worktree {
                None => verdict.delete_local = true,
                Some(wt) if wt.is_main => {
                    verdict.warnings.push(format!(
                        "branch `{branch}` is checked out in the main worktree; keeping it"
                    ));
                }
                Some(wt) if wt.contains_cwd => {
                    verdict.warnings.push(format!(
                        "branch `{branch}`'s worktree contains the current directory; \
                         keeping the worktree and branch"
                    ));
                }
                Some(wt) => match wt.status {
                    TreeStatus::Clean => {
                        verdict.remove_worktree = true;
                        verdict.delete_local = true;
                    }
                    TreeStatus::UntrackedOnly => {
                        verdict.warnings.push(format!(
                            "worktree `{}` has untracked files; keeping the worktree and \
                             branch `{branch}`",
                            wt.path.display()
                        ));
                    }
                    TreeStatus::TrackedChanges => {
                        verdict.warnings.push(format!(
                            "worktree `{}` has working tree changes; keeping the worktree and \
                             branch `{branch}`",
                            wt.path.display()
                        ));
                    }
                },
            }
        }
    }

    // The state file goes only when this run leaves nothing of the branch.
    verdict.delete_state = (facts.local_tip.is_none() || verdict.delete_local)
        && (facts.remote_tip.is_none() || verdict.delete_remote)
        && (facts.worktree.is_none() || verdict.remove_worktree);
    verdict
}

/// Print the verdict's warnings and carry out (or, under `--dry-run`,
/// describe) its deletions. Returns `false` when anything was said, so the
/// caller knows the run wasn't quiet.
fn execute(
    repo: &Path,
    owner: &str,
    repo_name: &str,
    branch: &str,
    facts: &BranchFacts,
    verdict: &Verdict,
    dry_run: bool,
) -> Result<bool> {
    for warning in &verdict.warnings {
        eprintln!("warning: {warning}");
    }

    // A failed step downgrades to a warning, keeps everything that depends on
    // it, and blocks the state-file cleanup.
    let mut all_succeeded = true;

    let mut local_deletable = verdict.delete_local;
    if verdict.remove_worktree {
        // The worktree facts exist whenever its removal was ordered.
        let path = &facts
            .worktree
            .as_ref()
            .expect("verdict requires facts")
            .path;
        if dry_run {
            println!("would remove worktree `{}`", path.display());
        } else {
            match git::remove_worktree(repo, path) {
                Ok(()) => println!("removed worktree `{}`", path.display()),
                Err(err) => {
                    eprintln!(
                        "warning: failed to remove worktree `{}`: {err:#}",
                        path.display()
                    );
                    // The branch is still checked out there.
                    local_deletable = false;
                    all_succeeded = false;
                }
            }
        }
    }

    if local_deletable {
        if dry_run {
            println!("would delete local branch `{branch}`");
        } else {
            match git::delete_local_branch(repo, branch) {
                Ok(()) => println!("deleted local branch `{branch}`"),
                Err(err) => {
                    eprintln!("warning: failed to delete local branch `{branch}`: {err:#}");
                    all_succeeded = false;
                }
            }
        }
    }

    if verdict.delete_remote {
        if dry_run {
            println!("would delete remote branch `origin/{branch}`");
        } else {
            match git::delete_remote_branch(repo, branch) {
                Ok(()) => println!("deleted remote branch `origin/{branch}`"),
                Err(err) => {
                    eprintln!("warning: failed to delete remote branch `origin/{branch}`: {err:#}");
                    all_succeeded = false;
                }
            }
        }
    }

    if verdict.delete_state && all_succeeded {
        if let Some(number) = state::find_issue_for_branch(owner, repo_name, branch)? {
            if dry_run {
                println!("would remove ghwf state for issue #{number}");
            } else {
                state::delete(owner, repo_name, number)?;
                println!("removed ghwf state for issue #{number}");
            }
        }
    }

    let acted = verdict.remove_worktree || verdict.delete_local || verdict.delete_remote;
    Ok(!acted && verdict.warnings.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{classify, is_due, pick_merged_pr, BranchFacts, TreeStatus, Verdict, WorktreeFacts};
    use crate::models::{BranchPr, Oid};
    use std::path::PathBuf;

    const MERGED_TIP: &str = "aaa111";
    const OTHER_TIP: &str = "bbb222";

    fn pr(number: u64, state: &str, merged: bool) -> BranchPr {
        BranchPr {
            number,
            state: state.to_string(),
            head_ref_oid: MERGED_TIP.to_string(),
            merge_commit: merged.then(|| Oid {
                oid: "ccc333".to_string(),
            }),
        }
    }

    fn worktree(status: TreeStatus) -> WorktreeFacts {
        WorktreeFacts {
            path: PathBuf::from("/repos/worktrees/branch"),
            status,
            is_main: false,
            contains_cwd: false,
        }
    }

    fn facts(
        local_tip: Option<&str>,
        remote_tip: Option<&str>,
        worktree: Option<WorktreeFacts>,
    ) -> BranchFacts {
        BranchFacts {
            local_tip: local_tip.map(str::to_string),
            remote_tip: remote_tip.map(str::to_string),
            worktree,
        }
    }

    fn run_classify(facts: &BranchFacts, merge_landed: bool) -> Verdict {
        classify("branch", facts, MERGED_TIP, merge_landed)
    }

    #[test]
    fn is_due_honours_the_interval() {
        // Never run before: always due.
        assert!(is_due(None, 1_000, 3600));
        // Less than the interval has passed: not due.
        assert!(!is_due(Some(1_000), 1_000 + 3599, 3600));
        // Exactly at the interval: due.
        assert!(is_due(Some(1_000), 1_000 + 3600, 3600));
        // A zero interval disables the throttle.
        assert!(is_due(Some(1_000), 1_000, 0));
        // Clock skew (now before last): not due, never panics.
        assert!(!is_due(Some(2_000), 1_000, 3600));
    }

    #[test]
    fn pick_merged_pr_skips_active_and_unmerged_branches() {
        // An open PR blocks collection even alongside an older merged one.
        let open = [pr(1, "MERGED", true), pr(2, "OPEN", false)];
        assert!(pick_merged_pr(&open).is_none());
        // Closed-without-merge is not garbage either.
        let closed = [pr(1, "CLOSED", false)];
        assert!(pick_merged_pr(&closed).is_none());
        assert!(pick_merged_pr(&[]).is_none());
        // The latest merged PR wins.
        let merged = [
            pr(1, "MERGED", true),
            pr(3, "MERGED", true),
            pr(2, "CLOSED", false),
        ];
        assert_eq!(pick_merged_pr(&merged).unwrap().number, 3);
    }

    #[test]
    fn fully_matching_branch_collects_everything() {
        let facts = facts(
            Some(MERGED_TIP),
            Some(MERGED_TIP),
            Some(worktree(TreeStatus::Clean)),
        );
        let verdict = run_classify(&facts, true);
        assert!(verdict.remove_worktree);
        assert!(verdict.delete_local);
        assert!(verdict.delete_remote);
        assert!(verdict.delete_state);
        assert!(verdict.warnings.is_empty());
    }

    #[test]
    fn squash_merge_shape_still_collects() {
        // For squash/rebase merges the tip never lands on main itself, but
        // tip == headRefOid and a landed merge commit suffice; classify takes
        // exactly those facts, so this is the same as the plain case.
        let facts = facts(Some(MERGED_TIP), Some(MERGED_TIP), None);
        let verdict = run_classify(&facts, true);
        assert!(verdict.delete_local);
        assert!(verdict.delete_remote);
        assert!(verdict.delete_state);
    }

    #[test]
    fn diverged_local_is_kept_with_a_warning() {
        let facts = facts(Some(OTHER_TIP), Some(MERGED_TIP), None);
        let verdict = run_classify(&facts, true);
        assert!(!verdict.delete_local);
        // The matching remote side still goes.
        assert!(verdict.delete_remote);
        assert!(!verdict.delete_state);
        assert_eq!(verdict.warnings.len(), 1);
        assert!(verdict.warnings[0].contains("local branch `branch`"));
    }

    #[test]
    fn diverged_remote_is_kept_with_a_warning() {
        let facts = facts(Some(MERGED_TIP), Some(OTHER_TIP), None);
        let verdict = run_classify(&facts, true);
        assert!(verdict.delete_local);
        assert!(!verdict.delete_remote);
        assert!(!verdict.delete_state);
        assert_eq!(verdict.warnings.len(), 1);
        assert!(verdict.warnings[0].contains("origin/branch"));
    }

    #[test]
    fn unlanded_merge_keeps_everything() {
        // A merged PR whose merge commit isn't on the default branch (e.g.
        // merged into some other base) is suspicious: warn, touch nothing.
        let facts = facts(
            Some(MERGED_TIP),
            Some(MERGED_TIP),
            Some(worktree(TreeStatus::Clean)),
        );
        let verdict = run_classify(&facts, false);
        assert!(!verdict.remove_worktree);
        assert!(!verdict.delete_local);
        assert!(!verdict.delete_remote);
        assert!(!verdict.delete_state);
        assert_eq!(verdict.warnings.len(), 1);
        assert!(verdict.warnings[0].contains("not on the default branch"));
    }

    #[test]
    fn dirty_worktree_keeps_worktree_and_local_branch() {
        let facts = facts(
            Some(MERGED_TIP),
            Some(MERGED_TIP),
            Some(worktree(TreeStatus::TrackedChanges)),
        );
        let verdict = run_classify(&facts, true);
        assert!(!verdict.remove_worktree);
        assert!(!verdict.delete_local);
        // The merged remote branch is independent of local unsaved work.
        assert!(verdict.delete_remote);
        assert!(!verdict.delete_state);
        assert!(verdict.warnings[0].contains("working tree changes"));
    }

    #[test]
    fn untracked_files_keep_the_worktree_too() {
        let facts = facts(
            Some(MERGED_TIP),
            None,
            Some(worktree(TreeStatus::UntrackedOnly)),
        );
        let verdict = run_classify(&facts, true);
        assert!(!verdict.remove_worktree);
        assert!(!verdict.delete_local);
        assert!(!verdict.delete_state);
        assert!(verdict.warnings[0].contains("untracked files"));
    }

    #[test]
    fn main_worktree_is_never_removed() {
        let mut wt = worktree(TreeStatus::Clean);
        wt.is_main = true;
        let facts = facts(Some(MERGED_TIP), None, Some(wt));
        let verdict = run_classify(&facts, true);
        assert!(!verdict.remove_worktree);
        assert!(!verdict.delete_local);
        assert!(verdict.warnings[0].contains("main worktree"));
    }

    #[test]
    fn cwd_worktree_is_never_removed() {
        let mut wt = worktree(TreeStatus::Clean);
        wt.contains_cwd = true;
        let facts = facts(Some(MERGED_TIP), None, Some(wt));
        let verdict = run_classify(&facts, true);
        assert!(!verdict.remove_worktree);
        assert!(!verdict.delete_local);
        assert!(verdict.warnings[0].contains("current directory"));
    }

    #[test]
    fn remote_only_branch_collects_remote_and_state() {
        let facts = facts(None, Some(MERGED_TIP), None);
        let verdict = run_classify(&facts, true);
        assert!(!verdict.delete_local);
        assert!(verdict.delete_remote);
        assert!(verdict.delete_state);
        assert!(verdict.warnings.is_empty());
    }

    #[test]
    fn local_only_branch_collects_local_and_state() {
        let facts = facts(Some(MERGED_TIP), None, None);
        let verdict = run_classify(&facts, true);
        assert!(verdict.delete_local);
        assert!(!verdict.delete_remote);
        assert!(verdict.delete_state);
    }
}
