use std::io::Read;

use anyhow::Result;
use serde::Deserialize;

use crate::session_binding::find_bound_issue;
use crate::state::{self, IssueState, Phase};

/// Consecutive nudges after which the hook gives up and lets the stop through:
/// the session was nudged this many times without anything new arriving, so
/// either Claude is stuck or the user wants out.
pub(crate) const NUDGE_CAP: u32 = 3;

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
    let Ok(Some(bound)) = find_bound_issue(&input.session_id) else {
        return Ok(());
    };

    if !should_block(&bound.state) {
        return Ok(());
    }

    // Record the nudge before issuing it; a failure to persist means the cap
    // can't count, so fail open rather than nudge unaccountably. The bump goes
    // through `mutate` so it re-reads the latest state under the per-issue lock
    // and touches only `stop_nudges` — a concurrent `work-on` save (phase
    // advance, consumed-sets) is never clobbered out from under it.
    let bumped = state::mutate(&bound.owner, &bound.repo, bound.number, |s| s.stop_nudges += 1);
    if bumped.is_err() {
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
    // The workflow is finished (issue closed, or PR merged/closed): nothing
    // left to wait for.
    if state.is_concluded() {
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
         should keep watching for activity. Run `ghwf wait` with a 10-minute \
         command timeout: exit 0 means new activity arrived — run `ghwf work-on` \
         to process it; exit 2 means nothing yet — run `ghwf wait` again. If the \
         user has explicitly told you to stop working on this issue, stop instead.",
        phase.label()
    )
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
        assert!(reason.contains("ghwf wait"));
        assert!(reason.contains("ghwf work-on"));
        // The bare number is omitted so the loop resolves against the bound issue.
        assert!(!reason.contains("ghwf wait 42"));
        assert!(reason.contains("stop instead"));
    }
}
