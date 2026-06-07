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

/// A label on an issue, trimmed to the fields we care about.
#[derive(Deserialize, Serialize)]
pub struct Label {
    pub name: String,
}

/// An entry from the REST issues *listing*, which carries fields the
/// single-issue [`Issue`] fetch doesn't need (assignees, labels) and may
/// describe a PR rather than an issue.
#[derive(Deserialize, Serialize)]
pub struct IssueListing {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub assignees: Vec<User>,
    #[serde(default)]
    pub labels: Vec<Label>,
    // Present (with any value) exactly when the entry is a PR.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
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
    use super::{ReviewComment, User};

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
