use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::models::Issue;
use crate::state::{self, IssueState, PrepState};
use crate::{config, git, github};

/// Ensure the issue's branch + worktree exist, creating `state.prep` (in branch
/// mode) if needed. Returns `(worktree_path, branch)`.
///
/// Shared by the prep-and-plan phase and the outside-Claude launcher (which
/// creates the worktree as early as pre-plan, so the session it starts is
/// anchored there from the first launch). Records the branch and worktree path
/// in prep state; callers persist the state.
pub fn ensure_worktree(
    issue: &Issue,
    owner: &str,
    repo: &str,
    state: &mut IssueState,
) -> Result<(PathBuf, String)> {
    // Branch mode requires a config that tells us where to put the worktree.
    let located = config::require()?;
    let main_repo = located.main_repo_path();

    if state.prep.is_none() {
        state.prep = Some(PrepState::default());
    }
    let prep = state.prep.as_mut().expect("prep state was just set");

    if prep.branch.is_none() {
        let (branch, _) = state::branch_and_slug(issue.number, &issue.title);
        let worktree_path = located.worktrees_dir_path().join(&branch);
        git::fetch(&main_repo)?;
        let default = github::default_branch(owner, repo)?;
        git::add_worktree(
            &main_repo,
            &worktree_path,
            &branch,
            &format!("origin/{default}"),
        )?;
        prep.branch = Some(branch);
        prep.worktree_path = Some(worktree_path);
    }

    let worktree = prep.worktree_path.clone().expect("worktree path set above");
    let branch = prep.branch.clone().expect("branch set above");
    Ok((worktree, branch))
}

/// Drive the prep-and-plan phase for an issue, idempotently doing whatever step
/// remains: create the worktree/branch, wait for Claude to write the plan, then
/// commit, push, and open a draft PR. Returns the banner body describing the
/// current state and what Claude should do next.
pub fn run(
    issue: &Issue,
    owner: &str,
    repo: &str,
    number: u64,
    no_branch_flag: bool,
    state: &mut IssueState,
) -> Result<String> {
    // Record the mode on first entry; later runs reuse it. (The outside-Claude
    // launcher may already have created branch-mode prep state during pre-plan.)
    if state.prep.is_none() {
        state.prep = Some(PrepState {
            no_branch: no_branch_flag,
            ..Default::default()
        });
    }
    let prep = state.prep.as_ref().expect("prep state was just set");

    if no_branch_flag && !prep.no_branch {
        eprintln!(
            "warning: this issue is already being worked in branch mode; ignoring --no-branch."
        );
    }

    let (_, slug) = state::branch_and_slug(number, &issue.title);
    let plan_rel = format!("plans/{number}-{slug}.md");

    if prep.no_branch {
        return Ok(no_branch_body(&plan_rel));
    }

    // 1. Ensure the worktree/branch exists.
    let (worktree, branch) = ensure_worktree(issue, owner, repo, state)?;

    // 2. Wait for Claude to write the plan file.
    let plan_abs = worktree.join(&plan_rel);
    if !plan_abs.exists() {
        return Ok(plan_needed_body(&worktree, &branch, &plan_abs, number));
    }

    // 3. Commit the plan if it isn't already.
    if !(git::is_tracked(&worktree, &plan_rel) && git::is_clean(&worktree, &plan_rel)?) {
        git::commit_file(
            &worktree,
            &plan_rel,
            &format!("Add plan for #{number}: {}", issue.title),
        )?;
    }

    // 4. Push the branch if it isn't up to date on origin.
    if !git::remote_branch_matches(&worktree, &branch)? {
        git::push(&worktree, &branch)?;
    }

    // 5. Open the draft PR if there isn't one yet.
    let prep = state
        .prep
        .as_mut()
        .expect("prep state exists in branch mode");
    if prep.pr_number.is_none() {
        let pr = match github::find_pr(owner, repo, &branch)? {
            Some(n) => n,
            None => {
                let default = github::default_branch(owner, repo)?;
                let body = format!("Plan for #{number}.\n\n## Issue\n\n{}", issue.html_url);
                github::create_draft_pr(owner, repo, &default, &branch, &issue.title, &body)?
            }
        };
        prep.pr_number = Some(pr);
    }
    let pr = prep.pr_number.expect("pr number set above");
    let pr_url = format!("https://github.com/{owner}/{repo}/pull/{pr}");

    Ok(complete_body(&worktree, &branch, &pr_url, number))
}

fn no_branch_body(plan_rel: &str) -> String {
    format!(
        "Prep-and-plan (--no-branch).\n\n\
         Write the plan to `{plan_rel}` as a file (do not use Claude Code plan mode). \
         No branch, worktree, or PR will be created — you are managing the branch and commits yourself."
    )
}

fn plan_needed_body(worktree: &Path, branch: &str, plan_abs: &Path, number: u64) -> String {
    format!(
        "Prep-and-plan: worktree ready at `{}` on branch `{branch}`.\n\n\
         Write the plan to `{}` as a file (do not use Claude Code plan mode), then re-run \
         `ghwf work-on {number}`. ghwf will commit it, push the branch, and open a draft PR.",
        worktree.display(),
        plan_abs.display(),
    )
}

fn complete_body(worktree: &Path, branch: &str, pr_url: &str, number: u64) -> String {
    format!(
        "Prep-and-plan complete.\n\n\
         - Worktree: `{}`\n\
         - Branch: `{branch}`\n\
         - Draft PR: {pr_url}\n\n\
         If you haven't already, post a hand-off comment on issue #{number} and on the PR \
         saying the plan is ready for review, and that commenting `/approve-plan` on either \
         thread advances to the implement phase.\n\n{}",
        worktree.display(),
        crate::render::wait_instruction(number),
    )
}

#[cfg(test)]
mod tests {
    use super::complete_body;
    use std::path::Path;

    #[test]
    fn complete_body_includes_wait_instruction() {
        let body = complete_body(Path::new("/wt"), "b", "https://github.com/o/r/pull/18", 7);
        assert!(body.contains("`ghwf wait 7`"));
    }
}
