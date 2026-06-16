use std::collections::BTreeSet;

use anyhow::{Context, Result};

use crate::config;
use crate::github;

/// Priority-label names we recognise, each with the colour (GitHub's 6-hex
/// form) and description we create it with. Matching is case-insensitive.
/// Deliberately easy to grow: a larger pre-canned list can be added later so
/// many "custom" labels magically get an appropriate colour.
const RECOGNISED: &[(&str, &str, &str)] = &[
    // GitHub's bright amber.
    ("high-priority", "fbca04", "High priority"),
    // GitHub's pale amber, a step down from `high-priority`.
    ("medium-priority", "fef2c0", "Medium priority"),
];

/// `ghwf config priority-labels`: create the configured priority labels in the
/// code repo and every `issue_repos` repo, adopting (leaving untouched) any
/// that already exist. Idempotent — safe to re-run, and the way to upsert the
/// labels onto an existing repo or a fresh clone.
pub fn configure() -> Result<()> {
    let located = config::require()?;
    create_for(&located)
}

/// The body of [`configure`], also called by the `config init` wizard once it
/// has located the config and written the chosen `priority_labels`.
pub fn create_for(located: &config::Located) -> Result<()> {
    let labels = &located.config.priority_labels;
    if labels.is_empty() {
        println!(
            "No `priority_labels` configured; nothing to create. \
             Set some (e.g. via `ghwf config init`) first."
        );
        return Ok(());
    }
    // The configured (code) repo, plus every `issue_repos` repo — a foreign
    // issue carries its priority label in its own repo, so the label must exist
    // there too. Same repo set as `ghwf config state-labels`.
    let mut repos = vec![github::repo_or_cwd()?];
    repos.extend(located.config.issue_repo_refs()?);
    let mut created = 0;
    for (owner, repo) in &repos {
        created += create_in_repo(owner, repo, labels)?;
    }
    if created == 0 {
        println!("All configured priority labels already exist; nothing to create.");
    }
    Ok(())
}

/// Create the configured priority labels missing from `owner/repo`, returning
/// how many were created. An existing same-named label is adopted (left
/// untouched), never recoloured.
fn create_in_repo(owner: &str, repo: &str, labels: &[String]) -> Result<usize> {
    let existing: BTreeSet<String> = github::list_repo_labels(owner, repo)?
        .into_iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let to_create = labels_to_create(labels, &existing);
    for (name, color, description) in &to_create {
        github::create_label(owner, repo, name, color, description)
            .with_context(|| format!("failed to create label `{name}` in {owner}/{repo}"))?;
        println!("Created label `{name}` in {owner}/{repo}.");
    }
    Ok(to_create.len())
}

/// The configured labels absent from `existing` (a set of lower-cased names),
/// each paired with the colour and description to create it with. GitHub label
/// names are unique case-insensitively, so a case-only difference counts as
/// already-present — adopting it rather than provoking a 409 on create.
fn labels_to_create<'a>(
    labels: &'a [String],
    existing: &BTreeSet<String>,
) -> Vec<(&'a str, String, &'static str)> {
    labels
        .iter()
        .filter(|name| !existing.contains(&name.to_ascii_lowercase()))
        .map(|name| (name.as_str(), colour_for(name), description_for(name)))
        .collect()
}

/// The recognised colour and description for `name` (case-insensitive), if any.
fn recognised(name: &str) -> Option<(&'static str, &'static str)> {
    RECOGNISED
        .iter()
        .find(|(known, _, _)| known.eq_ignore_ascii_case(name))
        .map(|&(_, color, description)| (color, description))
}

/// The colour to create `name` with: its recognised colour if we know the name,
/// otherwise a colour derived deterministically from the name.
fn colour_for(name: &str) -> String {
    match recognised(name) {
        Some((color, _)) => color.to_string(),
        None => digest_colour(name),
    }
}

/// The description to create `name` with: the recognised one, else empty.
fn description_for(name: &str) -> &'static str {
    recognised(name).map_or("", |(_, description)| description)
}

/// Deterministic 6-hex colour from a label name, so a given name always lands
/// on the same colour across runs. Uses FNV-1a rather than
/// `std::collections::hash_map::DefaultHasher`, whose seed is randomised per
/// process and so would give a different colour each run.
fn digest_colour(name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.to_ascii_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:06x}", hash & 0xff_ffff)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{colour_for, digest_colour, labels_to_create};

    fn lower_set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_ascii_lowercase()).collect()
    }

    #[test]
    fn recognised_colours_are_case_insensitive() {
        assert_eq!(colour_for("high-priority"), "fbca04");
        assert_eq!(colour_for("High-Priority"), "fbca04");
        assert_eq!(colour_for("medium-priority"), "fef2c0");
    }

    #[test]
    fn digest_colour_is_deterministic_and_valid() {
        let first = digest_colour("soon");
        assert_eq!(first, digest_colour("soon"));
        assert_eq!(first.len(), 6);
        assert!(first
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Case-insensitive, like the recognised lookup.
        assert_eq!(digest_colour("Soon"), first);
        // Different names generally land on different colours.
        assert_ne!(digest_colour("soon"), digest_colour("later"));
    }

    #[test]
    fn labels_to_create_skips_existing_case_insensitively() {
        let labels = vec!["high-priority".to_string(), "soon".to_string()];
        let existing = lower_set(&["High-Priority"]);
        let to_create = labels_to_create(&labels, &existing);
        // Only `soon` is missing; it gets its digest colour and empty description.
        assert_eq!(to_create.len(), 1);
        let (name, color, description) = &to_create[0];
        assert_eq!(*name, "soon");
        assert_eq!(*color, digest_colour("soon"));
        assert_eq!(*description, "");
    }

    #[test]
    fn empty_priority_labels_creates_nothing() {
        let labels: Vec<String> = Vec::new();
        let to_create = labels_to_create(&labels, &BTreeSet::new());
        assert!(to_create.is_empty());
    }
}
