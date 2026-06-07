use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::state::{Attention, Phase};

/// Name of the config file ghwf walks up the directory tree to find.
const CONFIG_FILE: &str = "ghwf.toml";

/// Contents of a `ghwf.toml`. Paths are relative to the file's own directory.
#[derive(Deserialize)]
pub struct Config {
    /// Path to the main git repo. Defaults to the config's directory.
    pub main_repo: Option<PathBuf>,
    /// Directory under which worktrees are created.
    pub worktrees_dir: PathBuf,
    /// Labels that mark an issue as urgent, most urgent first. `ghwf next`
    /// prefers issues carrying a label earlier in this list.
    #[serde(default)]
    pub priority_labels: Vec<String>,
    /// Workflow status labels, mirrored onto the issue and PR as the workflow
    /// advances. Absent means the feature is off; `ghwf config labels`
    /// bootstraps the section.
    pub labels: Option<LabelsConfig>,
}

/// The `[labels]` section: one GitHub label name per phase and per attention
/// state. All names are required once the section is present — partial
/// configs would make the sync's remove-undesired step ambiguous.
#[derive(Deserialize)]
pub struct LabelsConfig {
    pub phase: PhaseLabels,
    pub attention: AttentionLabels,
}

/// Label names for the `[labels.phase]` table.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PhaseLabels {
    pub pre_plan: String,
    pub prep_and_plan: String,
    pub implement: String,
    pub review: String,
}

/// Label names for the `[labels.attention]` table.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AttentionLabels {
    pub waiting_on_user: String,
    pub waiting_on_claude: String,
    pub waiting_on_ghwf: String,
}

impl LabelsConfig {
    /// The configured label for a phase.
    pub fn for_phase(&self, phase: Phase) -> &str {
        match phase {
            Phase::PrePlan => &self.phase.pre_plan,
            Phase::PrepAndPlan => &self.phase.prep_and_plan,
            Phase::Implement => &self.phase.implement,
            Phase::Review => &self.phase.review,
        }
    }

    /// The configured label for an attention state.
    pub fn for_attention(&self, attention: Attention) -> &str {
        match attention {
            Attention::WaitingOnUser => &self.attention.waiting_on_user,
            Attention::WaitingOnClaude => &self.attention.waiting_on_claude,
            Attention::WaitingOnGhwf => &self.attention.waiting_on_ghwf,
        }
    }

    /// Every configured label name. Only these are ever added or removed by
    /// the sync; the user's other labels are invisible to it.
    pub fn all(&self) -> [&str; 7] {
        [
            &self.phase.pre_plan,
            &self.phase.prep_and_plan,
            &self.phase.implement,
            &self.phase.review,
            &self.attention.waiting_on_user,
            &self.attention.waiting_on_claude,
            &self.attention.waiting_on_ghwf,
        ]
    }
}

/// A parsed config together with the directory it was found in.
pub struct Located {
    pub dir: PathBuf,
    pub config: Config,
}

impl Located {
    /// Absolute path to the config file itself.
    pub fn file_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    /// Absolute path to the main repo.
    pub fn main_repo_path(&self) -> PathBuf {
        match &self.config.main_repo {
            Some(p) => self.dir.join(p),
            None => self.dir.clone(),
        }
    }

    /// Absolute path to the worktrees directory.
    pub fn worktrees_dir_path(&self) -> PathBuf {
        self.dir.join(&self.config.worktrees_dir)
    }
}

/// Walk up from the current directory looking for a `ghwf.toml`, returning its
/// path if found.
fn locate() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    cwd.ancestors()
        .map(|dir| dir.join(CONFIG_FILE))
        .find(|path| path.is_file())
}

/// Search for `ghwf.toml`, starting at the current directory and walking up.
pub fn find() -> Result<Option<Located>> {
    let Some(path) = locate() else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
    let dir = path
        .parent()
        .expect("config path always has a parent directory")
        .to_path_buf();
    Ok(Some(Located { dir, config }))
}

/// Warn to stderr if no config is present. Use this in contexts that don't
/// strictly need one, so the user is nudged to add it — but skip it where a
/// missing config is about to be a hard error (avoids warning and erroring about
/// the same thing).
pub fn warn_if_absent() {
    if locate().is_none() {
        eprintln!(
            "warning: no {CONFIG_FILE} found in this or any parent directory; \
             commands that create worktrees will require one."
        );
    }
}

/// Like [`find`], but error when no config is found.
pub fn require() -> Result<Located> {
    match find()? {
        Some(located) => Ok(located),
        None => bail!(
            "this step requires a {CONFIG_FILE} (with `worktrees_dir`) in this or a parent \
             directory; none found. Use --no-branch to work without one."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn priority_labels_parse() {
        let config: Config = toml::from_str(
            r#"
            worktrees_dir = "worktrees"
            priority_labels = ["urgent", "soon"]
            "#,
        )
        .unwrap();
        assert_eq!(config.priority_labels, ["urgent", "soon"]);
    }

    #[test]
    fn priority_labels_default_to_empty() {
        // Pre-existing configs without the key keep loading.
        let config: Config = toml::from_str(r#"worktrees_dir = "worktrees""#).unwrap();
        assert!(config.priority_labels.is_empty());
        assert!(config.labels.is_none());
    }

    #[test]
    fn labels_section_parses_and_maps() {
        use crate::state::{Attention, Phase};
        let config: Config = toml::from_str(
            r#"
            worktrees_dir = "worktrees"

            [labels.phase]
            pre-plan = "ghwf:pre-plan"
            prep-and-plan = "ghwf:planning"
            implement = "ghwf:implementing"
            review = "ghwf:review"

            [labels.attention]
            waiting-on-user = "ghwf:needs-you"
            waiting-on-claude = "ghwf:claude-working"
            waiting-on-ghwf = "ghwf:preparing"
            "#,
        )
        .unwrap();
        let labels = config.labels.unwrap();
        assert_eq!(labels.for_phase(Phase::PrePlan), "ghwf:pre-plan");
        assert_eq!(labels.for_phase(Phase::Review), "ghwf:review");
        assert_eq!(
            labels.for_attention(Attention::WaitingOnUser),
            "ghwf:needs-you"
        );
        assert_eq!(
            labels.for_attention(Attention::WaitingOnGhwf),
            "ghwf:preparing"
        );
        assert_eq!(labels.all().len(), 7);
    }

    #[test]
    fn labels_section_missing_key_errors() {
        // All-or-nothing: a partial table is a config error, not a default.
        let result: Result<Config, _> = toml::from_str(
            r#"
            worktrees_dir = "worktrees"

            [labels.phase]
            pre-plan = "ghwf:pre-plan"

            [labels.attention]
            waiting-on-user = "ghwf:needs-you"
            waiting-on-claude = "ghwf:claude-working"
            waiting-on-ghwf = "ghwf:preparing"
            "#,
        );
        assert!(result.is_err());
    }
}
