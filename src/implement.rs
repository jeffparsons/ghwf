use std::path::Path;

use anyhow::Result;

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
    let (_, slug) = state::branch_and_slug(number, &issue.title);
    let plan_rel = format!("plans/{number}-{slug}.md");

    let Some(prep) = state.prep.as_ref() else {
        // Shouldn't happen: implement is only reachable via prep-and-plan.
        return Ok(no_prep_body(number));
    };

    if prep.no_branch {
        return Ok(no_branch_body(number, &plan_rel));
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
        number,
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
        return review_no_branch_body(number);
    }

    let Some(pr) = prep.pr_number else {
        return "Review — awaiting human review. (No ghwf PR was recorded.)".to_string();
    };

    let pr_url = format!("https://github.com/{owner}/{repo}/pull/{pr}");
    review_body(&pr_url, number, pr_instructions)
}

/// Detect whether the open PR's branch conflicts with the freshly-fetched
/// base. Returns `Some(base_branch_name)` when it does.
///
/// Best-effort: any failure (no worktree on disk, offline, a git error) logs a
/// warning and returns `None` — conflict detection must never break a
/// `work-on` run. Detection is local: a `git fetch` of the base ref plus an
/// in-memory `git merge-tree`, with no GitHub API calls.
///
/// The caller gates this on the workflow having an open PR (so `pr_number` is
/// expected); here we re-check the branch-mode preconditions defensively.
pub fn detect_conflict(prep: &PrepState) -> Option<String> {
    if prep.no_branch || prep.pr_number.is_none() {
        return None;
    }
    let worktree = prep.worktree_path.as_ref()?;
    if !worktree.is_dir() {
        return None;
    }
    match try_detect_conflict(worktree) {
        Ok(result) => result,
        Err(err) => {
            eprintln!("warning: could not check for merge conflicts: {err:#}");
            None
        }
    }
}

/// The fallible mechanics of [`detect_conflict`].
fn try_detect_conflict(worktree: &Path) -> Result<Option<String>> {
    git::fetch(worktree)?;
    let base = git::default_remote_branch(worktree)?;
    if git::would_conflict(worktree, &format!("origin/{base}"))? {
        Ok(Some(base))
    } else {
        Ok(None)
    }
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
    number: u64,
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
         complete and ready for human review, hand off with `ghwf hand-off {number}` (body \
         from stdin): a comment summarising the change. ghwf appends the next-step \
         instructions (the user marks the draft PR ready for review) — do not write \
         them yourself.\n\n{}",
        pr_maintenance_instruction(pr_instructions),
        crate::render::wait_instruction(number)
    )
}

fn no_branch_body(number: u64, plan_rel: &str) -> String {
    format!(
        "Implement (--no-branch) — code the change per `{plan_rel}`.\n\n\
         You are managing the branch and commits yourself; there is no ghwf worktree or PR. \
         When the work is complete, hand off with `ghwf hand-off {number}` (body from \
         stdin).\n\n{}",
        crate::render::wait_instruction(number)
    )
}

fn review_body(pr_url: &str, number: u64, pr_instructions: Option<&Path>) -> String {
    format!(
        "Review — awaiting human review.\n\n\
         The PR is ready for review: {pr_url}\n\n\
         Nothing more is needed from you unless review feedback arrives; it will appear below \
         on future `ghwf work-on {number}` runs. {}\n\n{}",
        pr_maintenance_instruction(pr_instructions),
        crate::render::wait_instruction(number)
    )
}

fn review_no_branch_body(number: u64) -> String {
    format!(
        "Review — the work is complete.\n\n\
         There is no ghwf PR to mark ready (this issue was worked with --no-branch); hand off \
         for human review however you normally would.\n\n{}",
        crate::render::wait_instruction(number)
    )
}

fn no_prep_body(number: u64) -> String {
    format!(
        "No prep state is recorded for issue #{number}. Run `ghwf work-on {number}` through the \
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
            branch_body("/wt", "plans/7-x.md", None, 7, None),
            no_branch_body(7, "plans/7-x.md"),
            review_body("https://github.com/o/r/pull/18", 7, None),
            review_no_branch_body(7),
        ] {
            assert!(body.contains("`ghwf wait 7`"), "missing in: {body}");
        }
    }

    #[test]
    fn implement_bodies_hand_off_without_retired_command() {
        for body in [
            branch_body("/wt", "plans/7-x.md", None, 7, None),
            no_branch_body(7, "plans/7-x.md"),
        ] {
            assert!(body.contains("`ghwf hand-off 7`"), "missing in: {body}");
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
            branch_body("/wt", "plans/7-x.md", None, 7, Some(path)),
            review_body("https://github.com/o/r/pull/18", 7, Some(path)),
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
            branch_body("/wt", "plans/7-x.md", None, 7, None),
            review_body("https://github.com/o/r/pull/18", 7, None),
        ] {
            assert!(
                body.contains("keep them accurate, concise, and current"),
                "missing in: {body}"
            );
        }
    }
}
