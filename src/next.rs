use anyhow::{bail, Result};

use crate::models::IssueListing;
use crate::{config, github, state};

/// Pick the next issue to work on, print the pick and its rationale, and
/// return its number for the caller to hand to `work-on`.
///
/// Eligible issues are the repo's open issues, excluding PRs, issues assigned
/// to someone other than the user, and issues that already have recorded ghwf
/// state (some session has already started them — `next` can't tell whether
/// that session is still live, so it never picks them; `ghwf work-on <n>`
/// resumes them explicitly).
pub fn pick() -> Result<u64> {
    let priority_labels = match config::find()? {
        Some(located) => located.config.priority_labels,
        None => Vec::new(),
    };
    let (owner, repo) = github::repo_or_cwd()?;
    let me = github::authenticated_user()?;
    let issues = github::list_open_issues(&owner, &repo)?;

    // A state file that exists but fails to parse still marks the issue as
    // started — only its details are unreadable.
    let started = |number: u64| {
        state::load_if_exists(&owner, &repo, number)
            .map(|s| s.is_some())
            .unwrap_or(true)
    };
    let selection = select(&issues, &me, &priority_labels, started);

    for number in &selection.skipped_started {
        println!("Skipping #{number} — already started; resume it with `ghwf work-on {number}`.");
    }
    match selection.picked {
        Some(issue) => {
            println!(
                "Picked #{} \"{}\" — {}.",
                issue.number,
                issue.title,
                rationale(issue, &me, &priority_labels)
            );
            Ok(issue.number)
        }
        None => bail!(
            "no eligible open issues in {owner}/{repo}: every open issue is a PR, is assigned \
             to someone else, or is already started (resume those with `ghwf work-on <n>`)."
        ),
    }
}

/// The outcome of [`select`]: the winning issue, if any, and the
/// already-started issues passed over (in listing order).
struct Selection<'a> {
    picked: Option<&'a IssueListing>,
    skipped_started: Vec<u64>,
}

/// Choose the most important eligible issue.
///
/// Sort key, ascending — first wins: issues assigned to `me` before
/// unassigned ones, then the best (earliest-in-list) priority label, then the
/// lowest issue number. PRs and issues assigned to someone else are excluded
/// outright; otherwise-eligible issues for which `already_started` returns
/// true are skipped and reported.
fn select<'a>(
    issues: &'a [IssueListing],
    me: &str,
    priority_labels: &[String],
    already_started: impl Fn(u64) -> bool,
) -> Selection<'a> {
    let mut skipped_started = Vec::new();
    let mut candidates: Vec<&IssueListing> = Vec::new();
    for issue in issues {
        if issue.pull_request.is_some() {
            continue;
        }
        if !issue.assignees.is_empty() && !assigned_to(issue, me) {
            continue;
        }
        if already_started(issue.number) {
            skipped_started.push(issue.number);
            continue;
        }
        candidates.push(issue);
    }
    candidates.sort_by_key(|issue| {
        (
            !assigned_to(issue, me),
            label_rank(issue, priority_labels),
            issue.number,
        )
    });
    Selection {
        picked: candidates.first().copied(),
        skipped_started,
    }
}

/// Whether `me` is among the issue's assignees. GitHub logins are
/// case-insensitive.
fn assigned_to(issue: &IssueListing, me: &str) -> bool {
    issue
        .assignees
        .iter()
        .any(|a| a.login.eq_ignore_ascii_case(me))
}

/// The index into `priority_labels` of the issue's best (earliest-in-list)
/// priority label; `usize::MAX` when it carries none, so unlabelled issues
/// sort after all labelled ones. Label names compare case-insensitively, as
/// GitHub treats them.
fn label_rank(issue: &IssueListing, priority_labels: &[String]) -> usize {
    issue
        .labels
        .iter()
        .filter_map(|label| {
            priority_labels
                .iter()
                .position(|p| p.eq_ignore_ascii_case(&label.name))
        })
        .min()
        .unwrap_or(usize::MAX)
}

/// A one-line explanation of why the issue won.
fn rationale(issue: &IssueListing, me: &str, priority_labels: &[String]) -> String {
    let mut parts = Vec::new();
    if assigned_to(issue, me) {
        parts.push(format!("assigned to {me}"));
    }
    let rank = label_rank(issue, priority_labels);
    if rank != usize::MAX {
        parts.push(format!("priority label `{}`", priority_labels[rank]));
    }
    if parts.is_empty() {
        "the earliest eligible open issue".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::{select, Selection};
    use crate::models::{IssueListing, Label, User};

    /// An open issue with the given assignees and labels.
    fn issue(number: u64, assignees: &[&str], labels: &[&str]) -> IssueListing {
        IssueListing {
            number,
            title: format!("issue {number}"),
            assignees: assignees
                .iter()
                .map(|login| User {
                    login: login.to_string(),
                })
                .collect(),
            labels: labels
                .iter()
                .map(|name| Label {
                    name: name.to_string(),
                })
                .collect(),
            pull_request: None,
        }
    }

    /// A PR entry, as the issues listing reports one.
    fn pr(number: u64) -> IssueListing {
        IssueListing {
            pull_request: Some(serde_json::json!({})),
            ..issue(number, &[], &[])
        }
    }

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// Run `select` with nothing already started.
    fn pick<'a>(issues: &'a [IssueListing], me: &str, priority_labels: &[String]) -> Selection<'a> {
        select(issues, me, priority_labels, |_| false)
    }

    #[test]
    fn assignment_beats_labels() {
        let issues = [issue(1, &[], &["urgent"]), issue(2, &["me"], &[])];
        let picked = pick(&issues, "me", &labels(&["urgent"])).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn earlier_label_beats_later() {
        let issues = [issue(1, &[], &["soon"]), issue(2, &[], &["urgent"])];
        let picked = pick(&issues, "me", &labels(&["urgent", "soon"]))
            .picked
            .unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn any_priority_label_beats_none() {
        let issues = [issue(1, &[], &[]), issue(2, &[], &["soon"])];
        let picked = pick(&issues, "me", &labels(&["urgent", "soon"]))
            .picked
            .unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn best_label_counts() {
        // An issue's smallest-index label is what ranks it.
        let issues = [issue(1, &[], &["soon"]), issue(2, &[], &["soon", "urgent"])];
        let picked = pick(&issues, "me", &labels(&["urgent", "soon"]))
            .picked
            .unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn labels_rank_within_assigned_group_too() {
        let issues = [issue(1, &["me"], &[]), issue(2, &["me"], &["urgent"])];
        let picked = pick(&issues, "me", &labels(&["urgent"])).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn number_breaks_ties() {
        let issues = [issue(7, &[], &[]), issue(3, &[], &[]), issue(5, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 3);
    }

    #[test]
    fn assigned_to_someone_else_is_excluded() {
        // Even a top-priority label doesn't make someone else's issue mine.
        let issues = [issue(1, &["other"], &["urgent"]), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &labels(&["urgent"])).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn co_assignment_counts_as_mine() {
        let issues = [issue(1, &["other", "me"], &[]), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 1);
    }

    #[test]
    fn login_compares_case_insensitively() {
        let issues = [issue(1, &["Me"], &[]), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 1);
    }

    #[test]
    fn prs_are_excluded() {
        let issues = [pr(1), issue(2, &[], &[])];
        let picked = pick(&issues, "me", &[]).picked.unwrap();
        assert_eq!(picked.number, 2);
    }

    #[test]
    fn started_issues_are_skipped_and_reported() {
        let issues = [
            issue(1, &["me"], &[]),
            // Excluded outright, so not reported as skipped.
            issue(2, &["other"], &[]),
            issue(3, &[], &[]),
        ];
        let selection = select(&issues, "me", &[], |n| n == 1);
        assert_eq!(selection.picked.unwrap().number, 3);
        assert_eq!(selection.skipped_started, [1]);
    }

    #[test]
    fn no_priority_labels_degrades_to_assigned_then_number() {
        let issues = [
            issue(1, &[], &["urgent"]),
            issue(2, &[], &[]),
            issue(3, &["me"], &[]),
        ];
        let selection = pick(&issues, "me", &[]);
        assert_eq!(selection.picked.unwrap().number, 3);
    }

    #[test]
    fn empty_pool_picks_nothing() {
        let issues = [pr(1), issue(2, &["other"], &[])];
        let selection = pick(&issues, "me", &[]);
        assert!(selection.picked.is_none());
        assert!(selection.skipped_started.is_empty());
    }
}
