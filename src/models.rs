use serde::{Deserialize, Serialize};

/// A GitHub user, trimmed to the fields we care about.
#[derive(Deserialize, Serialize)]
pub struct User {
    pub login: String,
}

/// A GitHub issue (or PR, for the fields shared with issues).
#[derive(Deserialize, Serialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub user: User,
    // An empty issue body comes back as `null`, hence `Option`.
    pub body: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
    pub author_association: String,
}

/// A pull request, trimmed to the fields conclusion and draft-flip detection
/// need, plus the title/body/head the proxy commands read. Only the single-PR
/// fetch carries `merged`.
#[derive(Deserialize, Serialize)]
pub struct PullRequest {
    pub number: u64,
    // `title`, `body`, and `head` always come back from the single-PR fetch the
    // proxy commands use; they're `#[serde(default)]` so the conclusion/draft
    // detection in `wait` can parse a PR object carrying only the fields it
    // needs.
    #[serde(default)]
    pub title: String,
    pub state: String,
    pub merged: bool,
    // Whether the PR is still a draft. The user marking it ready for review
    // is what advances the implement phase.
    #[serde(default)]
    pub draft: bool,
    // An empty PR body comes back as `null`, hence `Option`.
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub head: Head,
}

/// The head ref of a PR: the branch its commits live on, and its tip SHA.
#[derive(Deserialize, Serialize, Default)]
pub struct Head {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

/// A label on an issue, trimmed to the fields we care about.
#[derive(Deserialize, Serialize)]
pub struct Label {
    pub name: String,
}

/// The issue-dependencies rollup GitHub embeds in an issue object, trimmed to
/// the one count we act on.
#[derive(Deserialize, Serialize, Default)]
pub struct IssueDependenciesSummary {
    // Count of *open* issues currently blocking this one. Closed blockers are
    // excluded here (they live in `total_blocked_by`), so this is exactly
    // "currently blocked".
    #[serde(default)]
    pub blocked_by: u64,
}

/// The sub-issues rollup GitHub embeds in an issue object, trimmed to the one
/// count we act on.
#[derive(Deserialize, Serialize, Default)]
pub struct SubIssuesSummary {
    // Number of sub-issues; > 0 marks a tracking issue.
    #[serde(default)]
    pub total: u64,
}

/// An entry from the REST issues *listing*, which carries fields the
/// single-issue [`Issue`] fetch doesn't need (assignees, labels) and may
/// describe a PR rather than an issue. The sub-issues endpoint returns the same
/// shape, so this also models a tracking issue's children.
#[derive(Deserialize, Serialize)]
pub struct IssueListing {
    pub number: u64,
    pub title: String,
    // The repo-wide listing is fetched with `?state=open`, so this is elided in
    // most tests and defaults to empty (treated as open). It is load-bearing
    // only for sub-issue children, which the endpoint returns in any state.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub assignees: Vec<User>,
    #[serde(default)]
    pub labels: Vec<Label>,
    // Present (with any value) exactly when the entry is a PR.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
    // Both summaries are `#[serde(default)]` so repos (or GitHub versions)
    // without the dependencies/sub-issues features parse cleanly and read as
    // not-blocked / not-tracking.
    #[serde(default)]
    pub issue_dependencies_summary: IssueDependenciesSummary,
    #[serde(default)]
    pub sub_issues_summary: SubIssuesSummary,
}

impl IssueListing {
    /// Blocked by at least one still-open issue.
    pub fn is_blocked(&self) -> bool {
        self.issue_dependencies_summary.blocked_by > 0
    }

    /// Has sub-issues — a tracking issue we shouldn't work directly.
    pub fn is_tracking(&self) -> bool {
        self.sub_issues_summary.total > 0
    }

    /// Open, per the listing's `state`. The repo-wide `?state=open` listing
    /// elides it in tests, so an empty state counts as open; only closed
    /// sub-issue children carry a non-open value here.
    pub fn is_open(&self) -> bool {
        self.state.is_empty() || self.state == "open"
    }
}

/// A PR associated with a branch, from `gh pr list --json`, trimmed to the
/// fields garbage collection needs.
#[derive(Deserialize)]
pub struct BranchPr {
    pub number: u64,
    // "OPEN", "CLOSED", or "MERGED".
    pub state: String,
    // The PR's head commit. Frozen at merge time for merged PRs: pushes to the
    // branch after the merge don't update it.
    #[serde(rename = "headRefOid")]
    pub head_ref_oid: String,
    // The commit the merge created on the base branch (the merge commit, or
    // the squash/rebase result); `null` until merged.
    #[serde(rename = "mergeCommit")]
    pub merge_commit: Option<Oid>,
}

/// A bare commit reference, as `gh` renders `mergeCommit`.
#[derive(Deserialize)]
pub struct Oid {
    pub oid: String,
}

/// A comment on an issue's (or PR's) conversation thread.
#[derive(Deserialize, Serialize)]
pub struct Comment {
    pub id: u64,
    pub user: User,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
    pub author_association: String,
    // The reaction summary GitHub embeds in every comment object. It gates
    // the per-comment reactions detail fetch: no 👍s, no extra API call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<ReactionRollup>,
}

/// The reaction-count summary embedded in a comment object, trimmed to the
/// counts we care about.
#[derive(Deserialize, Serialize)]
pub struct ReactionRollup {
    pub total_count: u64,
    #[serde(rename = "+1")]
    pub plus_one: u64,
}

/// One reaction on a comment, from the comment-reactions detail endpoint.
#[derive(Deserialize, Serialize)]
pub struct Reaction {
    pub id: u64,
    pub user: User,
    // The reaction kind: `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`,
    // `rocket`, or `eyes`. Only `+1` carries workflow meaning.
    pub content: String,
    pub created_at: String,
}

impl Reaction {
    /// Whether this reaction is a thumbs-up.
    pub fn is_thumbs_up(&self) -> bool {
        self.content == "+1"
    }
}

/// An inline review comment on a PR, anchored to a file (and usually a line)
/// of its diff.
#[derive(Deserialize, Serialize)]
pub struct ReviewComment {
    pub id: u64,
    pub user: User,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
    pub author_association: String,
    pub path: String,
    // `null` when the comment is outdated against the latest diff, or file-level.
    pub line: Option<u64>,
    // The anchor in the diff the comment was left on; the fallback when `line`
    // is `null`.
    pub original_line: Option<u64>,
}

impl ReviewComment {
    /// Human-readable anchor for the comment: `path:line`, falling back to the
    /// original line for outdated comments, or the bare path for file-level ones.
    pub fn location(&self) -> String {
        match self.line.or(self.original_line) {
            Some(line) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{IssueListing, ReviewComment, User};

    #[test]
    fn listing_without_summaries_reads_as_eligible() {
        // A repo (or GitHub version) without the dependencies/sub-issues
        // features omits both summaries (and `state`); they default such that
        // the issue is open, not blocked, and not a tracking issue.
        let listing: IssueListing = serde_json::from_str(r#"{"number":1,"title":"t"}"#).unwrap();
        assert!(listing.is_open());
        assert!(!listing.is_blocked());
        assert!(!listing.is_tracking());
    }

    #[test]
    fn blocked_by_open_counts_as_blocked() {
        let listing: IssueListing = serde_json::from_str(
            r#"{"number":1,"title":"t","issue_dependencies_summary":{"blocked_by":1,"total_blocked_by":1}}"#,
        )
        .unwrap();
        assert!(listing.is_blocked());
    }

    #[test]
    fn closed_only_blockers_are_not_blocked() {
        // Closed blockers live in `total_blocked_by`; `blocked_by` stays 0.
        let listing: IssueListing = serde_json::from_str(
            r#"{"number":1,"title":"t","issue_dependencies_summary":{"blocked_by":0,"total_blocked_by":2}}"#,
        )
        .unwrap();
        assert!(!listing.is_blocked());
    }

    #[test]
    fn sub_issues_total_marks_a_tracking_issue() {
        let listing: IssueListing = serde_json::from_str(
            r#"{"number":1,"title":"t","sub_issues_summary":{"total":3,"completed":1}}"#,
        )
        .unwrap();
        assert!(listing.is_tracking());
    }

    #[test]
    fn explicit_closed_state_is_not_open() {
        let listing: IssueListing =
            serde_json::from_str(r#"{"number":1,"title":"t","state":"closed"}"#).unwrap();
        assert!(!listing.is_open());
    }

    fn review_comment(line: Option<u64>, original_line: Option<u64>) -> ReviewComment {
        ReviewComment {
            id: 1,
            user: User {
                login: "reviewer".to_string(),
            },
            body: "body".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            html_url: "https://github.com/o/r/pull/1#discussion_r1".to_string(),
            author_association: "OWNER".to_string(),
            path: "src/main.rs".to_string(),
            line,
            original_line,
        }
    }

    #[test]
    fn location_prefers_current_line() {
        assert_eq!(
            review_comment(Some(42), Some(7)).location(),
            "src/main.rs:42"
        );
    }

    #[test]
    fn location_falls_back_to_original_line() {
        assert_eq!(review_comment(None, Some(7)).location(), "src/main.rs:7");
    }

    #[test]
    fn location_falls_back_to_bare_path() {
        assert_eq!(review_comment(None, None).location(), "src/main.rs");
    }
}
