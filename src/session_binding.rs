use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;

use crate::state::IssueState;
use crate::store;

/// An issue bound to a session, located by its recorded worktree session id.
/// Shared by the Stop and Notification hooks, which both key off the session id
/// Claude Code feeds them on stdin.
pub struct BoundIssue {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub state: IssueState,
}

/// Scan the per-issue state files for the issue whose worktree most recently ran
/// `work-on` under `session_id`. Multiple matches (one session having driven
/// several issues) resolve to the most recently modified state file. No-branch
/// issues never record a session id, so they never match.
///
/// Used from hooks, so it consults only local state and never touches the
/// network; callers fail open on `None`/`Err`.
pub fn find_bound_issue(session_id: &str) -> Result<Option<BoundIssue>> {
    let root = store::data_dir()?.join("issues");
    let mut best: Option<(SystemTime, BoundIssue)> = None;
    for path in state_files(&root) {
        let Ok(json) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<IssueState>(&json) else {
            continue;
        };
        if state
            .prep
            .as_ref()
            .and_then(|p| p.worktree_session_id.as_deref())
            != Some(session_id)
        {
            continue;
        }
        // The path is `<root>/<owner>/<repo>/<number>.json`.
        let Some(bound) = bound_from_path(&root, &path, state) else {
            continue;
        };
        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(at, _)| modified > *at) {
            best = Some((modified, bound));
        }
    }
    Ok(best.map(|(_, bound)| bound))
}

/// Every `*.json` file under `<root>/<owner>/<repo>/`.
fn state_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(owners) = fs::read_dir(root) else {
        return files;
    };
    for owner in owners.flatten() {
        let Ok(repos) = fs::read_dir(owner.path()) else {
            continue;
        };
        for repo in repos.flatten() {
            let Ok(entries) = fs::read_dir(repo.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    files.push(path);
                }
            }
        }
    }
    files
}

/// Recover `(owner, repo, number)` from a state file's path relative to the
/// issues root.
fn bound_from_path(
    root: &std::path::Path,
    path: &std::path::Path,
    state: IssueState,
) -> Option<BoundIssue> {
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = relative.iter();
    let owner = parts.next()?.to_str()?.to_string();
    let repo = parts.next()?.to_str()?.to_string();
    let number = path.file_stem()?.to_str()?.parse().ok()?;
    Some(BoundIssue {
        owner,
        repo,
        number,
        state,
    })
}
