use std::path::Path;

use anyhow::{Context, Result};

use crate::git;
use crate::models::Issue;
use crate::state::{self, IssueState, PrepState};

/// Build the implement-phase banner: where the work lives and what to do next.
///
/// ghwf's role here is light — Claude does the coding. The new-PR-activity digest
/// is rendered by the caller below this banner; the worktree guard is applied by
/// the caller after state is saved.
///
/// `pr_instructions` is the project's PR instructions file, when one exists.
pub fn run(
    issue: &Issue,
    owner: &str,
    repo: &str,
    number: u64,
    state: &IssueState,
    pr_instructions: Option<&Path>,
) -> Result<String> {
    // Only the slug is needed here, which is independent of the branch prefix.
    let (_, slug) = state::branch_and_slug(None, number, &issue.title);
    let plan_rel = format!("plans/{number}-{slug}.md");

    let Some(prep) = state.prep.as_ref() else {
        // Shouldn't happen: implement is only reachable via prep-and-plan.
        return Ok(no_prep_body(number));
    };

    if prep.no_branch {
        return Ok(no_branch_body(&plan_rel));
    }

    let worktree = prep
        .worktree_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not yet created)".to_string());
    let pr_url = prep
        .pr_number
        .map(|pr| format!("https://github.com/{owner}/{repo}/pull/{pr}"));

    Ok(branch_body(
        &worktree,
        &plan_rel,
        pr_url.as_deref(),
        pr_instructions,
    ))
}

/// Build the review-phase banner. The PR is already ready for review — the
/// user marking it so is what advanced the workflow into this phase.
///
/// `pr_instructions` is the project's PR instructions file, when one exists.
pub fn review(
    owner: &str,
    repo: &str,
    number: u64,
    state: &IssueState,
    pr_instructions: Option<&Path>,
) -> String {
    let Some(prep) = state.prep.as_ref() else {
        return no_prep_body(number);
    };

    if prep.no_branch {
        return review_no_branch_body();
    }

    let Some(pr) = prep.pr_number else {
        return "Review — awaiting human review. (No ghwf PR was recorded.)".to_string();
    };

    let pr_url = format!("https://github.com/{owner}/{repo}/pull/{pr}");
    review_body(&pr_url, pr_instructions)
}

/// How the open PR's branch stands against its freshly-fetched base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSync {
    /// HEAD already contains `origin/<base>`; nothing to do.
    UpToDate,
    /// The base advanced beyond the merge-base and a trial merge is clean.
    BehindClean,
    /// The base advanced and a trial merge conflicts.
    Conflict,
}

/// Fetch, then classify the worktree's HEAD against `origin/<base>`. Returns the
/// base branch name (so callers can name it) alongside the verdict. Local: a
/// `git fetch` plus an in-memory `git merge-tree`, no GitHub API calls.
pub fn base_sync(worktree: &Path) -> Result<(String, BaseSync)> {
    git::fetch(worktree)?;
    let base = git::default_remote_branch(worktree)?;
    let base_ref = format!("origin/{base}");
    let sync = if git::is_ancestor(worktree, &base_ref, "HEAD") {
        BaseSync::UpToDate
    } else if git::would_conflict(worktree, &base_ref)? {
        BaseSync::Conflict
    } else {
        BaseSync::BehindClean
    };
    Ok((base, sync))
}

/// Check the open PR's branch against its base, guarding the branch-mode
/// preconditions and swallowing errors. Returns `None` when there's nothing to
/// check (no branch / no PR / no worktree on disk) or the check failed — this
/// must never break a `work-on` run.
///
/// The caller gates this on the workflow having an open PR (so `pr_number` is
/// expected); here we re-check the branch-mode preconditions defensively.
pub fn check_base(prep: &PrepState) -> Option<(String, BaseSync)> {
    if prep.no_branch || prep.pr_number.is_none() {
        return None;
    }
    let worktree = prep.worktree_path.as_ref()?;
    if !worktree.is_dir() {
        return None;
    }
    match base_sync(worktree) {
        Ok(result) => Some(result),
        Err(err) => {
            eprintln!("warning: could not check the branch against its base: {err:#}");
            None
        }
    }
}

/// Detect whether the open PR's branch conflicts with the freshly-fetched base.
/// Returns `Some(base_branch_name)` only when a trial merge conflicts; a clean
/// or up-to-date branch (or any precondition-unmet / error case) is `None`.
pub fn detect_conflict(prep: &PrepState) -> Option<String> {
    match check_base(prep)? {
        (base, BaseSync::Conflict) => Some(base),
        _ => None,
    }
}

/// The leading banner block for the implement/review phase body when the branch
/// is behind its base, plus whether it represents a standing conflict (which
/// keeps the ball with Claude) or just an informational auto-merge note.
pub enum BaseBanner {
    /// A conflict the agent must resolve before the work is done.
    Conflict(String),
    /// ghwf merged the base in for a clean behind-branch; nothing to do.
    Merged(String),
}

impl BaseBanner {
    /// The rendered banner text to lead the phase body with.
    pub fn text(&self) -> &str {
        match self {
            BaseBanner::Conflict(text) | BaseBanner::Merged(text) => text,
        }
    }

    /// Whether this is a standing conflict (vs. a done-and-dusted auto-merge).
    pub fn is_conflict(&self) -> bool {
        matches!(self, BaseBanner::Conflict(_))
    }
}

/// Build the leading banner block for the implement/review phase body when the
/// branch is behind its base. A conflict yields the resolve-it-now notice; a
/// clean-but-behind branch, when `auto_merge` is on, is merged up to base and
/// pushed (the notice confirms it); everything else (up to date, or auto-merge
/// off, or any failure) yields `None`.
///
/// Best-effort throughout: a failed auto-merge warns and falls through to no
/// banner rather than breaking the run — GitHub still squash-merges fine.
pub fn base_banner(prep: &PrepState, number: u64, auto_merge: bool) -> Option<BaseBanner> {
    match check_base(prep)? {
        (_, BaseSync::UpToDate) => None,
        (base, BaseSync::Conflict) => Some(BaseBanner::Conflict(crate::render::conflict_notice(
            &base, number,
        ))),
        (base, BaseSync::BehindClean) => {
            if !auto_merge {
                return None;
            }
            match auto_merge_base(prep, &base) {
                Ok(()) => Some(BaseBanner::Merged(crate::render::base_merged_notice(
                    &base, number,
                ))),
                Err(err) => {
                    eprintln!("warning: auto-merge of `origin/{base}` failed: {err:#}");
                    None
                }
            }
        }
    }
}

/// Merge `origin/<base>` into the PR branch and push. Caller has already proven
/// the merge is clean (a `BehindClean` verdict).
fn auto_merge_base(prep: &PrepState, base: &str) -> Result<()> {
    let worktree = prep
        .worktree_path
        .as_ref()
        .context("no worktree path recorded")?;
    let branch = prep.branch.as_ref().context("no branch recorded")?;
    git::merge(worktree, &format!("origin/{base}"))?;
    git::push(worktree, branch)?;
    Ok(())
}

/// The "keep the PR title/body current" paragraph, pointing at the project's
/// instructions file when one exists and falling back to a generic default.
fn pr_maintenance_instruction(pr_instructions: Option<&Path>) -> String {
    match pr_instructions {
        Some(path) => format!(
            "Read `{}` for this project's instructions on writing the PR title and body. \
             Finish each round of work by checking whether the PR title or body should be \
             updated to reflect what is now on the branch, and update them per those \
             instructions.",
            path.display()
        ),
        None => "Finish each round of work by checking whether the PR title or body should \
                 be updated to reflect what is now on the branch; keep them accurate, \
                 concise, and current."
            .to_string(),
    }
}

fn branch_body(
    worktree: &str,
    plan_rel: &str,
    pr_url: Option<&str>,
    pr_instructions: Option<&Path>,
) -> String {
    let pr_line = pr_url
        .map(|url| format!("- Draft PR: {url}\n"))
        .unwrap_or_default();
    format!(
        "Implement — code the change in the worktree.\n\n\
         - Worktree: `{worktree}`\n\
         - Plan: `{plan_rel}`\n\
         {pr_line}\n\
         Implement per the plan, committing and pushing to the branch as you go (the draft \
         PR updates automatically). Address any PR feedback shown below. {} When the work is \
         complete and ready for human review, hand off with `ghwf hand-off` (body \
         from stdin): a comment summarising the change. ghwf appends the next-step \
         instructions (the user marks the draft PR ready for review) — do not write \
         them yourself.\n\n{}\n\n{}\n\n{}",
        pr_maintenance_instruction(pr_instructions),
        crate::render::reply_where_asked_instruction(),
        crate::render::question_instruction(),
        crate::render::wait_instruction()
    )
}

fn no_branch_body(plan_rel: &str) -> String {
    format!(
        "Implement (--no-branch) — code the change per `{plan_rel}`.\n\n\
         You are managing the branch and commits yourself; there is no ghwf worktree or PR. \
         When the work is complete, hand off with `ghwf hand-off` (body from \
         stdin).\n\n{}\n\n{}",
        crate::render::question_instruction(),
        crate::render::wait_instruction()
    )
}

fn review_body(pr_url: &str, pr_instructions: Option<&Path>) -> String {
    format!(
        "Review — awaiting human review.\n\n\
         The PR is ready for review: {pr_url}\n\n\
         Nothing more is needed from you unless review feedback arrives; it will appear below \
         on future `ghwf work-on` runs. {}\n\n{}\n\n{}\n\n{}",
        pr_maintenance_instruction(pr_instructions),
        crate::render::reply_where_asked_instruction(),
        crate::render::question_instruction(),
        crate::render::wait_instruction()
    )
}

fn review_no_branch_body() -> String {
    format!(
        "Review — the work is complete.\n\n\
         There is no ghwf PR to mark ready (this issue was worked with --no-branch); hand off \
         for human review however you normally would.\n\n{}\n\n{}",
        crate::render::question_instruction(),
        crate::render::wait_instruction()
    )
}

fn no_prep_body(number: u64) -> String {
    format!(
        "No prep state is recorded for issue #{number}. Run `ghwf work-on` through the \
         earlier phases (pre-plan, prep-and-plan) first."
    )
}

#[cfg(test)]
mod tests {
    use super::{branch_body, detect_conflict, no_branch_body, review_body, review_no_branch_body};
    use crate::state::PrepState;
    use std::path::Path;

    #[test]
    fn detect_conflict_skips_when_preconditions_unmet() {
        // --no-branch: nothing to check.
        let no_branch = PrepState {
            no_branch: true,
            pr_number: Some(1),
            worktree_path: Some("/wt".into()),
            ..Default::default()
        };
        assert!(detect_conflict(&no_branch).is_none());

        // No PR recorded yet.
        let no_pr = PrepState {
            worktree_path: Some("/wt".into()),
            ..Default::default()
        };
        assert!(detect_conflict(&no_pr).is_none());

        // PR recorded but no worktree path.
        let no_worktree = PrepState {
            pr_number: Some(1),
            ..Default::default()
        };
        assert!(detect_conflict(&no_worktree).is_none());

        // Worktree path that doesn't exist on disk: skipped, not an error.
        let missing = PrepState {
            pr_number: Some(1),
            worktree_path: Some("/nonexistent/ghwf/worktree".into()),
            ..Default::default()
        };
        assert!(detect_conflict(&missing).is_none());
    }

    #[test]
    fn waiting_bodies_include_wait_instruction() {
        for body in [
            branch_body("/wt", "plans/7-x.md", None, None),
            no_branch_body("plans/7-x.md"),
            review_body("https://github.com/o/r/pull/18", None),
            review_no_branch_body(),
        ] {
            assert!(body.contains("`ghwf wait`"), "missing in: {body}");
        }
    }

    #[test]
    fn waiting_bodies_route_questions_off_interactive_prompts() {
        for body in [
            branch_body("/wt", "plans/7-x.md", None, None),
            no_branch_body("plans/7-x.md"),
            review_body("https://github.com/o/r/pull/18", None),
            review_no_branch_body(),
        ] {
            assert!(
                body.contains("`ghwf hand-off --question`"),
                "missing in: {body}"
            );
        }
    }

    #[test]
    fn pr_bodies_steer_replies_to_where_asked() {
        for body in [
            branch_body("/wt", "plans/7-x.md", None, None),
            review_body("https://github.com/o/r/pull/18", None),
        ] {
            assert!(
                body.contains("Answer each question in the place it was asked"),
                "missing in: {body}"
            );
            assert!(
                body.contains("`ghwf reply-review-comment --id <id>`"),
                "missing in: {body}"
            );
        }
    }

    #[test]
    fn implement_bodies_hand_off_without_retired_command() {
        for body in [
            branch_body("/wt", "plans/7-x.md", None, None),
            no_branch_body("plans/7-x.md"),
        ] {
            assert!(body.contains("`ghwf hand-off`"), "missing in: {body}");
            assert!(
                !body.contains("/approve-implementation"),
                "retired in: {body}"
            );
        }
    }

    #[test]
    fn pr_bodies_name_the_instructions_file_when_present() {
        let path = Path::new("/base/pull-request.md");
        for body in [
            branch_body("/wt", "plans/7-x.md", None, Some(path)),
            review_body("https://github.com/o/r/pull/18", Some(path)),
        ] {
            assert!(
                body.contains("`/base/pull-request.md`"),
                "missing in: {body}"
            );
            assert!(
                body.contains("Finish each round of work"),
                "missing in: {body}"
            );
        }
    }

    #[test]
    fn pr_bodies_fall_back_to_generic_instruction() {
        for body in [
            branch_body("/wt", "plans/7-x.md", None, None),
            review_body("https://github.com/o/r/pull/18", None),
        ] {
            assert!(
                body.contains("keep them accurate, concise, and current"),
                "missing in: {body}"
            );
        }
    }

    #[test]
    fn base_sync_classifies_against_origin() {
        use super::{base_sync, BaseSync};
        use crate::git::tests::{init_repo, run_git, scratch};

        let root = scratch("base-sync");
        let origin = root.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "--bare", "-b", "main"]);

        // The PR-branch worktree, wired to the bare origin with origin/HEAD set.
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        init_repo(&wt);
        run_git(&wt, &["remote", "add", "origin", origin.to_str().unwrap()]);
        run_git(&wt, &["push", "origin", "main"]);
        run_git(&wt, &["fetch", "origin"]);
        run_git(
            &wt,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        // A PR branch with its own commit while the base hasn't moved: HEAD
        // already contains origin/main, so it's up to date.
        run_git(&wt, &["checkout", "-b", "feat"]);
        std::fs::write(wt.join("feat.txt"), "f\n").unwrap();
        run_git(&wt, &["add", "feat.txt"]);
        run_git(&wt, &["commit", "-m", "feat work"]);
        assert_eq!(
            base_sync(&wt).unwrap(),
            ("main".to_string(), BaseSync::UpToDate)
        );

        // Advance origin/main on a different file (via a second clone): the
        // branch is now behind, but a trial merge is clean.
        let up = root.join("up");
        run_git(
            &root,
            &["clone", origin.to_str().unwrap(), up.to_str().unwrap()],
        );
        std::fs::write(up.join("other.txt"), "o\n").unwrap();
        run_git(&up, &["add", "other.txt"]);
        run_git(&up, &["commit", "-m", "upstream other"]);
        run_git(&up, &["push", "origin", "main"]);
        assert_eq!(
            base_sync(&wt).unwrap(),
            ("main".to_string(), BaseSync::BehindClean)
        );

        // Both sides now edit the same line of file.txt: a conflict.
        std::fs::write(wt.join("file.txt"), "feat\n").unwrap();
        run_git(&wt, &["commit", "-am", "feat edits file"]);
        std::fs::write(up.join("file.txt"), "upstream\n").unwrap();
        run_git(&up, &["commit", "-am", "upstream edits file"]);
        run_git(&up, &["push", "origin", "main"]);
        assert_eq!(
            base_sync(&wt).unwrap(),
            ("main".to_string(), BaseSync::Conflict)
        );

        std::fs::remove_dir_all(&root).unwrap();
    }
}
