use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store;

/// What a given Claude session has already been shown for one issue.
///
/// Content hashes (not timestamps) drive change detection, so an edited issue
/// body or comment is re-surfaced.
#[derive(Default, Serialize, Deserialize)]
pub struct SeenRecord {
    pub issue_body_hash: Option<String>,
    // Conversation comment id -> content hash of its body. Covers the issue
    // thread and, once a PR exists, the PR conversation thread too —
    // conversation comment ids share one global namespace, so one map is
    // unambiguous.
    pub comments: BTreeMap<u64, String>,
    // Inline review comment id -> content hash of its body. These ids come
    // from a different namespace than conversation comment ids, hence the
    // separate map; `default` keeps pre-existing records parsing.
    #[serde(default)]
    pub review_comments: BTreeMap<u64, String>,
}

/// Path to the seen-record for a session + issue, under the ghwf data dir.
fn record_path(session_id: &str, owner: &str, repo: &str, number: u64) -> Result<PathBuf> {
    Ok(store::data_dir()?
        .join("seen")
        .join(session_id)
        .join(owner)
        .join(repo)
        .join(format!("{number}.json")))
}

/// Load the seen-record for a session + issue, or a fresh default if none exists.
pub fn load(session_id: &str, owner: &str, repo: &str, number: u64) -> Result<SeenRecord> {
    let path = record_path(session_id, owner, repo, number)?;
    match fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json)
            .with_context(|| format!("failed to parse seen-record {}", path.display())),
        Err(_) => Ok(SeenRecord::default()),
    }
}

/// Persist the seen-record for a session + issue.
pub fn save(
    session_id: &str,
    owner: &str,
    repo: &str,
    number: u64,
    record: &SeenRecord,
) -> Result<()> {
    let path = record_path(session_id, owner, repo, number)?;
    let dir = path
        .parent()
        .expect("record path always has a parent directory");
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(record).context("failed to serialize seen-record")?;
    fs::write(&path, json)
        .with_context(|| format!("failed to write seen-record {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::SeenRecord;

    #[test]
    fn record_without_review_comments_parses() {
        // Records written before inline review comments existed lack the field.
        let json = r#"{"issue_body_hash": "abc", "comments": {"1": "h1"}}"#;
        let record: SeenRecord = serde_json::from_str(json).unwrap();
        assert!(record.review_comments.is_empty());
        assert_eq!(record.comments.get(&1).map(String::as_str), Some("h1"));
    }
}
