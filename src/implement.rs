use anyhow::Result;

use crate::models::Issue;
use crate::state::{self, IssueState};
use crate::github;

/// Build the implement-phase banner: where the work lives and what to do next.
///
/// ghwf's role here is light — Claude does the coding. The new-PR-activity digest
/// is rendered by the caller below this banner; the worktree guard is applied by
/// the caller after state is saved.
pub fn run(
    issue: &Issue,
    owner: &str,
    repo: &str,
    number: u64,
    state: &IssueState,
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

    Ok(branch_body(&worktree, &plan_rel, pr_url.as_deref(), number))
}

/// Build the review-phase banner, flipping the draft PR to ready-for-review once.
pub fn review(
    owner: &str,
    repo: &str,
    number: u64,
    state: &mut IssueState,
) -> Result<String> {
    let Some(prep) = state.prep.as_mut() else {
        return Ok(no_prep_body(number));
    };

    if prep.no_branch {
        return Ok(review_no_branch_body());
    }

    let Some(pr) = prep.pr_number else {
        return Ok("Review — awaiting human review. (No ghwf PR was recorded to mark ready.)"
            .to_string());
    };

    // Flip draft → ready exactly once, then remember we did.
    if !prep.pr_ready {
        github::mark_pr_ready(owner, repo, pr)?;
        prep.pr_ready = true;
    }

    let pr_url = format!("https://github.com/{owner}/{repo}/pull/{pr}");
    Ok(review_body(&pr_url, number))
}

fn branch_body(worktree: &str, plan_rel: &str, pr_url: Option<&str>, number: u64) -> String {
    let pr_line = pr_url
        .map(|url| format!("- Draft PR: {url}\n"))
        .unwrap_or_default();
    format!(
        "Implement — code the change in the worktree.\n\n\
         - Worktree: `{worktree}`\n\
         - Plan: `{plan_rel}`\n\
         {pr_line}\n\
         Implement per the plan, committing and pushing to the branch as you go (the draft \
         PR updates automatically). Address any PR feedback shown below. When the work is \
         complete and ready for human review, post a hand-off comment on issue #{number} \
         and on the PR prompting the user to comment `/approve-implementation` on either \
         thread."
    )
}

fn no_branch_body(number: u64, plan_rel: &str) -> String {
    format!(
        "Implement (--no-branch) — code the change per `{plan_rel}`.\n\n\
         You are managing the branch and commits yourself; there is no ghwf worktree or PR. \
         When the work is complete, post a comment on issue #{number} prompting the user to \
         comment `/approve-implementation`."
    )
}

fn review_body(pr_url: &str, number: u64) -> String {
    format!(
        "Review — awaiting human review.\n\n\
         The PR has been marked ready for review: {pr_url}\n\n\
         Nothing more is needed from you unless review feedback arrives; it will appear below \
         on future `ghwf work-on {number}` runs."
    )
}

fn review_no_branch_body() -> String {
    "Review — the work is complete.\n\n\
     There is no ghwf PR to mark ready (this issue was worked with --no-branch); hand off for \
     human review however you normally would."
        .to_string()
}

fn no_prep_body(number: u64) -> String {
    format!(
        "No prep state is recorded for issue #{number}. Run `ghwf work-on {number}` through the \
         earlier phases (pre-plan, prep-and-plan) first."
    )
}
