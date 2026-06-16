use std::collections::BTreeSet;

use anyhow::{Context, Result};

use crate::config::{self, LabelsConfig};
use crate::github::{self, RepoRef};
use crate::state::{Attention, IssueState, LabelSyncRecord, Phase, PrOutcome};

/// Mirror the workflow state onto GitHub labels on the issue and (when one
/// exists) its PR. Entirely best-effort decoration: every failure is a stderr
/// warning, never an error — the state file remains the source of truth.
///
/// The issue and PR may live in different repos (an `issue_repos` foreign
/// issue): `issue_repo` is where the issue lives, `code_repo` is where the PR
/// lives. They coincide for the common single-repo case. The configured labels
/// must exist in whichever repo a thread lives in; a missing label just warns
/// (best-effort), so a foreign issue repo without the labels degrades to no
/// issue labels rather than failing.
///
/// No-op when no `[labels]` section is configured, and when the last sync
/// already applied the same inputs (recorded in `state.labels_synced`), so
/// the steady-state loop adds zero API calls.
pub fn sync(
    issue_repo: &RepoRef,
    code_repo: &RepoRef,
    number: u64,
    pr_number: Option<u64>,
    state: &mut IssueState,
) {
    // A concluded workflow waits on nobody: the phase label stays as a
    // record of how far the work got, the attention label comes off.
    let attention = state.pr_outcome.is_none().then_some(state.attention);
    sync_with(
        issue_repo,
        code_repo,
        number,
        pr_number,
        state.phase,
        attention,
        state,
    );
}

/// Normalize labels for a freshly detected PR conclusion, without consulting or
/// mutating `state.phase` / `state.pr_outcome`. This lets `wait` clean up the
/// labels the instant it sees a merge (or a close) even when no `work-on`
/// follows, while leaving the conclusion invisible in the state file so the next
/// `work-on` still treats it as new and runs its once-per-merge side effects.
///
/// The `LabelSyncRecord` it writes is exactly the one `work-on`'s own end-of-run
/// `sync` would write for the same conclusion — a merge collapses to the
/// terminal `Finished` phase, a close keeps the phase it concluded from, and
/// either way the attention label comes off — so a follow-up `work-on` sync
/// no-ops rather than making duplicate API calls.
pub fn sync_concluded(
    issue_repo: &RepoRef,
    code_repo: &RepoRef,
    number: u64,
    pr_number: Option<u64>,
    conclusion: PrOutcome,
    state: &mut IssueState,
) {
    let phase = concluded_phase(conclusion, state.phase);
    sync_with(issue_repo, code_repo, number, pr_number, phase, None, state);
}

/// The phase a concluded workflow labels as: a merge collapses to the terminal
/// `Finished`; a close keeps the phase it concluded from, as a record of how far
/// the work got.
fn concluded_phase(conclusion: PrOutcome, phase: Phase) -> Phase {
    match conclusion {
        PrOutcome::Merged => Phase::Finished,
        PrOutcome::Closed => phase,
    }
}

/// Apply the given phase/attention labels to the issue and (when one exists) its
/// PR, recording the result in `state.labels_synced`. No-op when no `[labels]`
/// section is configured, and when the last sync already applied the same inputs.
fn sync_with(
    issue_repo: &RepoRef,
    code_repo: &RepoRef,
    number: u64,
    pr_number: Option<u64>,
    phase: Phase,
    attention: Option<Attention>,
    state: &mut IssueState,
) {
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

    let record = LabelSyncRecord {
        phase,
        attention,
        pr_number,
    };
    if state.labels_synced == Some(record) {
        return;
    }

    // Each thread is labelled in its own repo: the issue in `issue_repo`, the PR
    // (when one exists) in `code_repo`.
    let mut threads = vec![(issue_repo, number)];
    if let Some(pr) = pr_number {
        threads.push((code_repo, pr));
    }

    let mut all_ok = true;
    for (repo, thread) in threads {
        if let Err(err) = sync_thread(&cfg, &repo.0, &repo.1, thread, phase, attention) {
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

/// The default labels `ghwf config state-labels` sets up: config key, label name,
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
    // The terminal phase: a merged PR. Coloured GitHub's merged-purple to read
    // as a concluded workflow.
    (
        "finished",
        "ghwf:finished",
        "8957e5",
        "ghwf: workflow complete",
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
const PHASE_DEFAULTS: usize = 5;

/// `ghwf config state-labels`: on first run, create the default workflow labels in
/// the GitHub repo and append the `[labels]` section to `ghwf.toml` (rename
/// afterwards by editing the file and the repo's labels together). On a re-run
/// — once the section exists — it leaves the file alone and just creates any
/// configured label still missing from the repo.
pub fn configure() -> Result<()> {
    let located = config::require()?;
    match &located.config.labels {
        // First-time setup: create the default labels and append the section.
        None => configure_at(&located),
        // The section already exists, so leave the file alone — but still
        // reconcile the repo(s), creating any configured label that doesn't
        // exist yet. This is what lets a label added to the defaults after a
        // repo was first set up (e.g. `ghwf:finished`) be created on a re-run,
        // rather than the user having to add it by hand.
        Some(labels) => reconcile(&located, labels),
    }
}

/// Create any configured label that is missing from the repo(s), without
/// touching `ghwf.toml`. Run by [`configure`] when a `[labels]` section is
/// already present.
fn reconcile(located: &config::Located, labels: &LabelsConfig) -> Result<()> {
    // The configured (code) repo, plus every `issue_repos` repo — same set
    // `configure_at` labels on first-time setup.
    let mut repos = vec![github::repo_or_cwd()?];
    repos.extend(located.config.issue_repo_refs()?);
    let mut created = 0;
    for (owner, repo) in &repos {
        created += reconcile_repo(owner, repo, labels)?;
    }
    if created == 0 {
        println!("All configured labels already exist; nothing to create.");
    }
    Ok(())
}

/// Create the configured labels missing from `owner/repo`, returning how many
/// were created.
fn reconcile_repo(owner: &str, repo: &str, labels: &LabelsConfig) -> Result<usize> {
    let existing: BTreeSet<String> = github::list_repo_labels(owner, repo)?.into_iter().collect();
    let to_create = labels_to_create(labels, &existing);
    for &(name, color, description) in &to_create {
        github::create_label(owner, repo, name, color, description)
            .with_context(|| format!("failed to create label `{name}` in {owner}/{repo}"))?;
        println!("Created label `{name}` in {owner}/{repo}.");
    }
    Ok(to_create.len())
}

/// The configured labels absent from `existing`, each paired with the colour
/// and description from its [`DEFAULTS`] slot. The pairing is positional:
/// [`LabelsConfig::all`] yields names in `DEFAULTS` order (a correspondence the
/// `generated_section_parses_and_covers_every_state` test pins). A renamed
/// label still gets created, carrying its slot's default colour — the closest
/// we can do without the original metadata.
fn labels_to_create<'a>(
    labels: &'a LabelsConfig,
    existing: &BTreeSet<String>,
) -> Vec<(&'a str, &'static str, &'static str)> {
    labels
        .all()
        .into_iter()
        .zip(DEFAULTS)
        .filter(|(name, _)| !existing.contains(*name))
        .map(|(name, &(_, _, color, description))| (name, color, description))
        .collect()
}

/// The body of [`configure`], for callers (the `config init` wizard) that
/// have already located the config and ruled out an existing `[labels]`
/// section.
pub fn configure_at(located: &config::Located) -> Result<()> {
    // The configured (code) repo, plus every `issue_repos` repo — a foreign
    // issue is labelled in its own repo, so the labels must exist there too.
    let mut repos = vec![github::repo_or_cwd()?];
    repos.extend(located.config.issue_repo_refs()?);
    for (owner, repo) in &repos {
        create_default_labels(owner, repo)?;
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

/// Create the [`DEFAULTS`] labels in `owner/repo`, skipping any that already
/// exist. Used for the configured repo and every `issue_repos` repo.
fn create_default_labels(owner: &str, repo: &str) -> Result<()> {
    let existing: BTreeSet<String> = github::list_repo_labels(owner, repo)?.into_iter().collect();
    for &(_, name, color, description) in DEFAULTS {
        if existing.contains(name) {
            println!("Label `{name}` already exists in {owner}/{repo}; leaving it as is.");
            continue;
        }
        github::create_label(owner, repo, name, color, description)
            .with_context(|| format!("failed to create label `{name}` in {owner}/{repo}"))?;
        println!("Created label `{name}` in {owner}/{repo}.");
    }
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
    use std::collections::BTreeSet;

    use super::{
        concluded_phase, desired_labels, labels_section, labels_to_create, DEFAULTS, PHASE_DEFAULTS,
    };
    use crate::config::Config;
    use crate::state::{Attention, Phase, PrOutcome};

    fn config_with_labels() -> Config {
        let toml = format!("worktrees_dir = \"worktrees\"\n{}", labels_section());
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn generated_section_parses_and_covers_every_state() {
        // The section `ghwf config state-labels` writes must round-trip through the
        // config parser, with every phase and attention state mapped.
        let labels = config_with_labels().labels.unwrap();
        assert_eq!(labels.for_phase(Phase::PrePlan), "ghwf:pre-plan");
        assert_eq!(labels.for_phase(Phase::Finished), "ghwf:finished");
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

    #[test]
    fn finished_phase_carries_only_the_finished_label() {
        // A merged PR collapses to the single terminal label: no phase-of-origin
        // record, and the attention label is already gone for a concluded run.
        let labels = config_with_labels().labels.unwrap();
        let desired = desired_labels(&labels, Phase::Finished, None);
        assert_eq!(desired.into_iter().collect::<Vec<_>>(), ["ghwf:finished"]);
    }

    #[test]
    fn concluded_workflow_labels_collapse_correctly() {
        // A merge collapses to the terminal label regardless of where it
        // concluded from; a close keeps its phase but drops the attention label.
        // These are exactly the label sets `sync_concluded` applies (it pairs
        // `concluded_phase` with `desired_labels(.., None)`), and they match what
        // `work-on`'s own end-of-run sync produces, so a follow-up sync no-ops.
        let labels = config_with_labels().labels.unwrap();
        let merged = desired_labels(
            &labels,
            concluded_phase(PrOutcome::Merged, Phase::Review),
            None,
        );
        assert_eq!(merged.into_iter().collect::<Vec<_>>(), ["ghwf:finished"]);
        let closed = desired_labels(
            &labels,
            concluded_phase(PrOutcome::Closed, Phase::Review),
            None,
        );
        assert_eq!(closed.into_iter().collect::<Vec<_>>(), ["ghwf:review"]);
    }

    #[test]
    fn reconcile_creates_only_missing_labels_with_default_metadata() {
        // A re-run of `config state-labels` over a repo that already has every label
        // but `ghwf:finished` creates exactly that one, with its slot's colour
        // and description.
        let labels = config_with_labels().labels.unwrap();
        let mut existing: BTreeSet<String> =
            labels.all().iter().map(|name| name.to_string()).collect();
        existing.remove("ghwf:finished");
        assert_eq!(
            labels_to_create(&labels, &existing),
            vec![("ghwf:finished", "8957e5", "ghwf: workflow complete")]
        );
        // Nothing to do once the repo already has them all.
        let all: BTreeSet<String> = labels.all().iter().map(|name| name.to_string()).collect();
        assert!(labels_to_create(&labels, &all).is_empty());
    }
}
