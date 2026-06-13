use std::io::Read;

use anyhow::Result;
use serde::Deserialize;

use crate::session_binding::find_bound_issue;
use crate::state::{self, AlertKind, SessionAlert};

/// The fields we read from the JSON Claude Code feeds a Notification hook on
/// stdin. We take the notification kind from our own `--kind` argument (set per
/// matcher entry in the installed settings) rather than from stdin, so we don't
/// depend on the exact stdin field name for the notification type.
#[derive(Deserialize)]
struct HookInput {
    #[serde(default)]
    session_id: String,
}

/// The Notification-hook entry point (`ghwf claude-notification-hook --kind …`).
///
/// Claude Code runs this when a session goes idle (`idle_prompt`) or parks on a
/// permission dialog (`permission_prompt`). Like the Stop hook, it consults only
/// local state (no network) and fails open — any parse or IO problem just exits
/// 0 with nothing recorded. A Notification hook's output is ignored by Claude
/// Code anyway; its whole job is the side effect of recording the alert that the
/// supervisor polls.
pub fn run(kind: AlertKind) -> Result<()> {
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
    // A concluded workflow has nothing left to recover.
    if bound.state.is_concluded() {
        return Ok(());
    }

    let at = episode_start(
        bound.state.session_alert.as_ref(),
        kind,
        &input.session_id,
        state::now_epoch(),
    );
    bound.state.session_alert = Some(SessionAlert {
        kind,
        session_id: input.session_id,
        at,
    });
    // Best-effort: a failed save just means no alert is recorded this time.
    let _ = state::save(&bound.owner, &bound.repo, bound.number, &bound.state);
    Ok(())
}

/// When the current stuck episode began. The same kind of alert for the same
/// session keeps its original start time (so the supervisor's grace window
/// measures from when the session first got stuck, not the latest notification);
/// a different kind, a different session, or no prior alert starts a fresh
/// episode at `now`.
fn episode_start(prev: Option<&SessionAlert>, kind: AlertKind, session_id: &str, now: u64) -> u64 {
    match prev {
        Some(prev) if prev.kind == kind && prev.session_id == session_id => prev.at,
        _ => now,
    }
}

#[cfg(test)]
mod tests {
    use super::episode_start;
    use crate::state::{AlertKind, SessionAlert};

    fn alert(kind: AlertKind, session_id: &str, at: u64) -> SessionAlert {
        SessionAlert {
            kind,
            session_id: session_id.to_string(),
            at,
        }
    }

    #[test]
    fn fresh_episode_when_no_prior_alert() {
        assert_eq!(episode_start(None, AlertKind::Idle, "s1", 100), 100);
    }

    #[test]
    fn same_kind_and_session_keeps_start_time() {
        let prev = alert(AlertKind::Idle, "s1", 100);
        assert_eq!(episode_start(Some(&prev), AlertKind::Idle, "s1", 250), 100);
    }

    #[test]
    fn changed_kind_or_session_restarts_episode() {
        let prev = alert(AlertKind::Idle, "s1", 100);
        // Different kind.
        assert_eq!(
            episode_start(Some(&prev), AlertKind::Permission, "s1", 250),
            250
        );
        // Different session.
        assert_eq!(episode_start(Some(&prev), AlertKind::Idle, "s2", 250), 250);
    }
}
