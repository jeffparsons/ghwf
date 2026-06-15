use anyhow::Result;

use crate::config::Located;
use crate::github::{self, RepoRef};
use crate::labels;
use crate::state::{self, Attention};

/// Re-sync the attention/labels of any crash-abandoned issue across the repos
/// this config covers, flipping it to `waiting-on-user` so a session that
/// crashed or was killed mid-workflow doesn't leave the issue showing a frozen
/// "machine is working" badge until a human re-enters it (#110).
///
/// Best-effort: every failure is a stderr warning, never propagated. A no-op
/// when no `[labels]` section is configured — label hygiene is the whole point,
/// so without labels there is nothing to do, exactly as [`labels::sync`] itself
/// no-ops.
pub fn sweep(located: &Located) -> Result<()> {
    if located.config.labels.is_none() {
        return Ok(());
    }
    // The repos whose issues this config covers: the code repo plus every
    // foreign `issue_repos` repo. A foreign issue's state lives under its own
    // issue-repo directory, so all are walked; the common single-repo case
    // collapses to one entry after the dedup.
    let mut repos = vec![github::repo_or_cwd()?];
    repos.extend(located.config.issue_repo_refs()?);
    repos.sort();
    repos.dedup();
    for repo in &repos {
        for number in state::issue_numbers(&repo.0, &repo.1) {
            if let Err(err) = reset_if_abandoned(repo, number) {
                eprintln!("warning: failed to resync labels for #{number}: {err:#}");
            }
        }
    }
    Ok(())
}

/// Reset one issue to `waiting-on-user` (and re-sync its labels) when it bears
/// the crash signature, returning whether it acted.
///
/// The signature is: unconcluded work, still showing a "machine is working"
/// attention (`waiting-on-ghwf` / `waiting-on-claude`), with a stale lease left
/// behind by a dead session. An issue already resting on `waiting-on-user` is
/// left untouched (the gate fails on its attention), which also makes the sweep
/// idempotent: once reset, later sweeps skip it after just the state read.
///
/// The reset is persisted only when the label sync actually lands. If the
/// GitHub calls fail, the state is left as-is so a later sweep retries from the
/// original attention, rather than stranding the issue on a `waiting-on-user`
/// state whose label was never updated.
fn reset_if_abandoned(issue_repo: &RepoRef, number: u64) -> Result<bool> {
    let (owner, repo) = issue_repo;
    let Some(mut state) = state::load_if_exists(owner, repo, number)? else {
        return Ok(false);
    };
    if !should_reset_attention(state.is_concluded(), state.attention) {
        return Ok(false);
    }
    if !state::lease_is_stale(owner, repo, number) {
        return Ok(false);
    }

    let before = state.labels_synced;
    state.attention = Attention::WaitingOnUser;
    let code_repo = github::code_repo(issue_repo)?;
    let pr_number = state.prep.as_ref().and_then(|p| p.pr_number);
    labels::sync(issue_repo, &code_repo, number, pr_number, &mut state);

    // `labels::sync` advances `labels_synced` only on a fully applied sync, so an
    // unchanged record means the GitHub calls didn't land — leave the state
    // untouched and let a later sweep retry rather than persisting a needs-you
    // attention the label never caught up to.
    if state.labels_synced == before {
        return Ok(false);
    }
    state::save(owner, repo, number, &state)?;
    println!("Reset #{number} to needs-you — its session looks to have crashed.");
    Ok(true)
}

/// Whether an issue's attention should be reset to `waiting-on-user`: it has
/// unconcluded work whose attention still claims the machine is on it. A
/// concluded workflow waits on nobody, and an issue already waiting on the user
/// is already correct.
fn should_reset_attention(concluded: bool, attention: Attention) -> bool {
    !concluded
        && matches!(
            attention,
            Attention::WaitingOnGhwf | Attention::WaitingOnClaude
        )
}

#[cfg(test)]
mod tests {
    use super::should_reset_attention;
    use crate::state::Attention;

    #[test]
    fn resets_only_unconcluded_machine_states() {
        // The "machine is working" states on unconcluded work are the crash
        // signature: reset them.
        assert!(should_reset_attention(false, Attention::WaitingOnGhwf));
        assert!(should_reset_attention(false, Attention::WaitingOnClaude));

        // An issue already resting on the user is already correct — never reset.
        assert!(!should_reset_attention(false, Attention::WaitingOnUser));

        // A concluded workflow waits on nobody, whatever the attention.
        assert!(!should_reset_attention(true, Attention::WaitingOnGhwf));
        assert!(!should_reset_attention(true, Attention::WaitingOnClaude));
        assert!(!should_reset_attention(true, Attention::WaitingOnUser));
    }
}
