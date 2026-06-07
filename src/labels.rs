use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};

use crate::config::{self, LabelsConfig};
use crate::github;
use crate::state::{Attention, IssueState, LabelSyncRecord, Phase};

/// Mirror the workflow state onto GitHub labels on the issue and (when one
/// exists) its PR. Entirely best-effort decoration: every failure is a stderr
/// warning, never an error — the state file remains the source of truth.
///
/// No-op when no `[labels]` section is configured, and when the last sync
/// already applied the same inputs (recorded in `state.labels_synced`), so
/// the steady-state loop adds zero API calls.
pub fn sync(owner: &str, repo: &str, number: u64, pr_number: Option<u64>, state: &mut IssueState) {
    let cfg = match config::find() {
        Ok(Some(located)) => match located.config.labels {
            Some(cfg) => cfg,
            None => return,
        },
        Ok(None) => return,
        Err(err) => {
            eprintln!("warning: skipping label sync — failed to load ghwf.toml: {err:#}");
            return;
        }
    };

    // A concluded workflow waits on nobody: the phase label stays as a
    // record of how far the work got, the attention label comes off.
    let attention = state.pr_outcome.is_none().then_some(state.attention);
    let record = LabelSyncRecord {
        phase: state.phase,
        attention,
        pr_number,
    };
    if state.labels_synced == Some(record) {
        return;
    }

    let mut all_ok = true;
    for thread in std::iter::once(number).chain(pr_number) {
        if let Err(err) = sync_thread(&cfg, owner, repo, thread, state.phase, attention) {
            eprintln!("warning: failed to sync workflow labels on #{thread}: {err:#}");
            all_ok = false;
        }
    }
    // Only a fully applied sync is recorded; a partial one retries next run.
    if all_ok {
        state.labels_synced = Some(record);
    }
}

/// Compute and apply one thread's label changes: remove configured labels
/// that no longer apply, add the ones that do. Labels outside the configured
/// set are never touched.
fn sync_thread(
    cfg: &LabelsConfig,
    owner: &str,
    repo: &str,
    number: u64,
    phase: Phase,
    attention: Option<Attention>,
) -> Result<()> {
    let desired = desired_labels(cfg, phase, attention);
    let configured: BTreeSet<&str> = cfg.all().into();
    let current = github::fetch_issue_labels(owner, repo, number)?;

    for label in &current {
        if configured.contains(label.as_str()) && !desired.contains(label.as_str()) {
            github::remove_issue_label(owner, repo, number, label)?;
        }
    }
    let missing: Vec<&str> = desired
        .iter()
        .filter(|label| !current.iter().any(|current| current == *label))
        .copied()
        .collect();
    if !missing.is_empty() {
        github::add_issue_labels(owner, repo, number, &missing)?;
    }
    Ok(())
}

/// The labels a thread should carry: the phase's, plus the attention state's
/// while the workflow is live.
fn desired_labels(
    cfg: &LabelsConfig,
    phase: Phase,
    attention: Option<Attention>,
) -> BTreeSet<&str> {
    let mut desired = BTreeSet::new();
    desired.insert(cfg.for_phase(phase));
    if let Some(attention) = attention {
        desired.insert(cfg.for_attention(attention));
    }
    desired
}

/// The default labels `ghwf config labels` sets up: config key, label name,
/// colour (GitHub's 6-hex form), and description. The phase ramp runs light
/// to dark blue; attention colours signal whose move it is.
const DEFAULTS: &[(&str, &str, &str, &str)] = &[
    // [labels.phase]
    (
        "pre-plan",
        "ghwf:pre-plan",
        "c5def5",
        "ghwf: gathering context before planning",
    ),
    (
        "prep-and-plan",
        "ghwf:planning",
        "8ab8ef",
        "ghwf: writing the implementation plan",
    ),
    (
        "implement",
        "ghwf:implementing",
        "4a90d9",
        "ghwf: coding the change",
    ),
    (
        "review",
        "ghwf:review",
        "1d76db",
        "ghwf: awaiting human review",
    ),
    // [labels.attention]
    (
        "waiting-on-user",
        "ghwf:needs-you",
        "d93f0b",
        "ghwf: waiting on the user",
    ),
    (
        "waiting-on-claude",
        "ghwf:claude-working",
        "0e8a16",
        "ghwf: waiting on Claude",
    ),
    (
        "waiting-on-ghwf",
        "ghwf:preparing",
        "bfbfbf",
        "ghwf: machinery at work",
    ),
];

/// How many of the [`DEFAULTS`] belong to the `[labels.phase]` table; the
/// rest are `[labels.attention]`.
const PHASE_DEFAULTS: usize = 4;

/// `ghwf config labels`: create the default workflow labels in the GitHub
/// repo and append the `[labels]` section to `ghwf.toml`. Rename afterwards
/// by editing the file and the repo's labels together.
pub fn configure() -> Result<()> {
    let located = config::require()?;
    if located.config.labels.is_some() {
        bail!(
            "{} already has a [labels] section; edit it by hand instead.",
            located.file_path().display()
        );
    }

    let (owner, repo) = github::repo_or_cwd()?;
    let existing: BTreeSet<String> = github::list_repo_labels(&owner, &repo)?
        .into_iter()
        .collect();
    for &(_, name, color, description) in DEFAULTS {
        if existing.contains(name) {
            println!("Label `{name}` already exists in {owner}/{repo}; leaving it as is.");
            continue;
        }
        github::create_label(&owner, &repo, name, color, description)
            .with_context(|| format!("failed to create label `{name}` in {owner}/{repo}"))?;
        println!("Created label `{name}` in {owner}/{repo}.");
    }

    let path = located.file_path();
    let mut text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&labels_section());
    std::fs::write(&path, &text).with_context(|| format!("failed to write {}", path.display()))?;
    println!("Added the [labels] section to {}.", path.display());
    Ok(())
}

/// Render the `[labels]` TOML section for the default label names.
fn labels_section() -> String {
    let mut out = String::from("\n[labels.phase]\n");
    for &(key, name, _, _) in &DEFAULTS[..PHASE_DEFAULTS] {
        out.push_str(&format!("{key} = \"{name}\"\n"));
    }
    out.push_str("\n[labels.attention]\n");
    for &(key, name, _, _) in &DEFAULTS[PHASE_DEFAULTS..] {
        out.push_str(&format!("{key} = \"{name}\"\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{desired_labels, labels_section, DEFAULTS, PHASE_DEFAULTS};
    use crate::config::Config;
    use crate::state::{Attention, Phase};

    fn config_with_labels() -> Config {
        let toml = format!("worktrees_dir = \"worktrees\"\n{}", labels_section());
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn generated_section_parses_and_covers_every_state() {
        // The section `ghwf config labels` writes must round-trip through the
        // config parser, with every phase and attention state mapped.
        let labels = config_with_labels().labels.unwrap();
        assert_eq!(labels.for_phase(Phase::PrePlan), "ghwf:pre-plan");
        assert_eq!(
            labels.for_attention(Attention::WaitingOnGhwf),
            "ghwf:preparing"
        );
        let names: Vec<&str> = DEFAULTS.iter().map(|&(_, name, _, _)| name).collect();
        assert_eq!(labels.all().to_vec(), names);
        assert_eq!(DEFAULTS.len() - PHASE_DEFAULTS, 3);
    }

    #[test]
    fn desired_set_pairs_phase_with_attention() {
        let labels = config_with_labels().labels.unwrap();
        let desired = desired_labels(&labels, Phase::Implement, Some(Attention::WaitingOnUser));
        assert_eq!(
            desired.into_iter().collect::<Vec<_>>(),
            ["ghwf:implementing", "ghwf:needs-you"]
        );
    }

    #[test]
    fn concluded_workflow_drops_the_attention_label() {
        let labels = config_with_labels().labels.unwrap();
        let desired = desired_labels(&labels, Phase::Review, None);
        assert_eq!(desired.into_iter().collect::<Vec<_>>(), ["ghwf:review"]);
    }
}
