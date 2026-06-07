use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;
use serde::Deserialize;

use crate::state::{self, IssueState, Phase};
use crate::store;

/// Consecutive nudges after which the hook gives up and lets the stop through:
/// the session was nudged this many times without anything new arriving, so
/// either Claude is stuck or the user wants out.
const NUDGE_CAP: u32 = 3;

/// The fields we read from the JSON Claude Code feeds a Stop hook on stdin.
///
/// `stop_hook_active` is deliberately ignored: it would cap us at a single
/// nudge per natural stop, and the `stop_nudges` counter subsumes its
/// infinite-loop protection.
#[derive(Deserialize)]
struct HookInput {
    #[serde(default)]
    session_id: String,
}

/// The Stop-hook entry point (`ghwf claude-stop-hook`).
///
/// Claude Code runs this on every Stop event in every session, so two rules
/// hold throughout: consult only local state (no network), and fail open —
/// any parse or IO problem means allowing the stop with no output, never
/// wedging a session. Exit 0 either way; a block is signalled by the JSON
/// `{"decision": "block", …}` on stdout.
pub fn run() -> Result<()> {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return Ok(());
    }
    let Ok(input) = serde_json::from_str::<HookInput>(&raw) else {
        return Ok(());
    };
    if input.session_id.is_empty() {
        return Ok(());
    }
    let Ok(Some(mut bound)) = find_bound_issue(&input.session_id) else {
        return Ok(());
    };

    if !should_block(&bound.state) {
        return Ok(());
    }

    // Record the nudge before issuing it; a failure to persist means the cap
    // can't count, so fail open rather than nudge unaccountably.
    bound.state.stop_nudges += 1;
    if state::save(&bound.owner, &bound.repo, bound.number, &bound.state).is_err() {
        return Ok(());
    }

    let reason = block_reason(bound.number, bound.state.phase);
    println!(
        "{}",
        serde_json::json!({"decision": "block", "reason": reason})
    );
    Ok(())
}

/// Whether to block the stop for the bound issue.
fn should_block(state: &IssueState) -> bool {
    // The workflow is finished: nothing left to wait for.
    if state.issue_closed {
        return false;
    }
    // The PR was merged (workflow complete) or closed without merging
    // (workflow halted): either way the loop is over.
    if state.pr_outcome.is_some() {
        return false;
    }
    // Nudged repeatedly with nothing new arriving: stop fighting.
    state.stop_nudges < NUDGE_CAP
}

/// The instruction Claude receives in place of stopping: the same
/// `wait`/`work-on` loop contract the phase banners carry, plus an explicit
/// out when the user has asked for one.
fn block_reason(number: u64, phase: Phase) -> String {
    format!(
        "Issue #{number}'s ghwf workflow is still in the {} phase, so this session \
         should keep watching for activity. Run `ghwf wait {number}` with a 10-minute \
         command timeout: exit 0 means new activity arrived — run `ghwf work-on {number}` \
         to process it; exit 2 means nothing yet — run `ghwf wait {number}` again. If the \
         user has explicitly told you to stop working on this issue, stop instead.",
        phase.label()
    )
}

/// An issue bound to a session, located by its recorded worktree session id.
struct BoundIssue {
    owner: String,
    repo: String,
    number: u64,
    state: IssueState,
}

/// Scan the per-issue state files for the issue whose worktree most recently
/// ran `work-on` under `session_id`. Multiple matches (one session having
/// driven several issues) resolve to the most recently modified state file.
/// No-branch issues never record a session id, so they never match.
fn find_bound_issue(session_id: &str) -> Result<Option<BoundIssue>> {
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

#[cfg(test)]
mod tests {
    use super::{block_reason, should_block, NUDGE_CAP};
    use crate::state::{IssueState, Phase, PrOutcome};

    fn active_state() -> IssueState {
        IssueState {
            phase: Phase::Implement,
            ..Default::default()
        }
    }

    #[test]
    fn active_issue_blocks() {
        assert!(should_block(&active_state()));
    }

    #[test]
    fn closed_issue_allows() {
        let state = IssueState {
            issue_closed: true,
            ..active_state()
        };
        assert!(!should_block(&state));
    }

    #[test]
    fn concluded_pr_allows() {
        for outcome in [PrOutcome::Merged, PrOutcome::Closed] {
            let state = IssueState {
                pr_outcome: Some(outcome),
                ..active_state()
            };
            assert!(!should_block(&state));
        }
    }

    #[test]
    fn nudge_cap_allows() {
        let mut state = active_state();
        state.stop_nudges = NUDGE_CAP - 1;
        assert!(should_block(&state));
        state.stop_nudges = NUDGE_CAP;
        assert!(!should_block(&state));
    }

    #[test]
    fn reason_names_issue_phase_and_loop() {
        let reason = block_reason(42, Phase::PrepAndPlan);
        assert!(reason.contains("#42"));
        assert!(reason.contains("prep-and-plan"));
        assert!(reason.contains("ghwf wait 42"));
        assert!(reason.contains("ghwf work-on 42"));
        assert!(reason.contains("stop instead"));
    }
}
