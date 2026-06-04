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

/// A comment on an issue's (or PR's) conversation thread.
#[derive(Deserialize, Serialize)]
pub struct Comment {
    pub user: User,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
    pub author_association: String,
}
