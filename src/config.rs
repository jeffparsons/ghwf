use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use facet::Facet;
use serde::Deserialize;

use crate::state::{Attention, Phase};

/// Name of the config file ghwf walks up the directory tree to find.
pub const CONFIG_FILE: &str = "ghwf.toml";

/// Default name of the PR instructions file, next to the config.
pub const PR_INSTRUCTIONS_FILE: &str = "pull-request.md";

/// Contents of a `ghwf.toml`. Paths are relative to the file's own directory.
///
/// `Facet` is derived alongside serde purely for reflection: `ghwf config ls`,
/// `config info`, and `config example` read this shape (and its doc comments)
/// so the options stay self-documenting. serde + `toml` remain the real parser.
#[derive(Deserialize, Facet)]
pub struct Config {
    /// Path to the main git repo. Defaults to the config's directory.
    pub main_repo: Option<PathBuf>,
    /// Directory under which worktrees are created.
    pub worktrees_dir: PathBuf,
    /// Labels that mark an issue as urgent, most urgent first. `ghwf next`
    /// prefers issues carrying a label earlier in this list.
    #[serde(default)]
    pub priority_labels: Vec<String>,
    /// Path to a markdown file of instructions for writing PR titles and
    /// bodies. Defaults to `pull-request.md` next to the config.
    pub pr_instructions: Option<PathBuf>,
    /// Workflow status labels, mirrored onto the issue and PR as the workflow
    /// advances. Absent means the feature is off; `ghwf config labels`
    /// bootstraps the section.
    pub labels: Option<LabelsConfig>,
    /// Permission mode passed to launched Claude sessions as
    /// `--permission-mode <value>` (e.g. "auto"). Absent means Claude's
    /// default prompting behaviour.
    pub permission_mode: Option<String>,
    /// When true, ghwf rewrites the plan commit out of the branch's history once
    /// the implementation is approved (the draft PR is marked ready for review),
    /// then force-pushes the branch. For repos that don't want Claude's plans
    /// committed. A no-op in `--no-branch` mode, and skipped (with a warning)
    /// when the rewrite can't be done safely.
    #[serde(default)]
    pub delete_plan_on_approval: bool,
    /// When true, `ghwf next` only considers issues already assigned to the
    /// current user, ignoring unassigned ones. Suits teams that allocate work by
    /// discussion or a manager rather than picking off the list. Default off.
    #[serde(default)]
    pub only_assigned_to_me: bool,
    /// Label `ghwf create-issue` applies to a follow-up to mark it blocked by
    /// the issue it was filed from. It's a transient creation-race guard:
    /// included in the create payload so the guard is on the issue from the
    /// moment it exists (no window for a worker to grab it unblocked), then
    /// removed again once the native `blocked_by` dependency — the durable,
    /// GitHub-UI-visible truth — is set right after. It's kept only if that
    /// dependency call fails. Defaults to `blocked`.
    #[serde(default = "default_blocked_label")]
    pub blocked_label: String,
    /// Repos whose issues may be worked on even though the code, worktree, and
    /// PR live in `main_repo`. The configured repo is always allowed; this lists
    /// *additional* issue-only repos. Empty by default. Each entry is either a
    /// plain `"owner/repo"` string or a table with an optional `branch_prefix`.
    #[serde(default)]
    pub issue_repos: Vec<IssueRepo>,
    /// GitHub logins whose comments and 👍 reactions ghwf acts on, in addition
    /// to the always-accepted authenticated user and the repo's collaborators
    /// (anyone with an OWNER / MEMBER / COLLABORATOR association). Everyone
    /// else's comments and reactions are ignored, so a public repo's workflow
    /// can't be driven by strangers. Matched case-insensitively; empty by
    /// default. Note: a 👍 reaction carries no association, so collaborator
    /// auto-accept for reactions is resolved via the repo's collaborator list —
    /// an org member with no repo access is accepted on a typed comment but not
    /// on a bare 👍, and should be listed here (or use the `/approve-*` comment).
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

/// An entry in [`Config::issue_repos`]: a foreign repo whose issues may be
/// worked on. Either a plain `"owner/repo"` string or a table that also carries
/// a `branch_prefix` controlling how that repo's branches are named.
#[derive(Deserialize, Clone, Debug, Facet)]
#[serde(untagged)]
// `Facet` requires enums to carry an explicit representation; serde ignores it.
#[repr(u8)]
pub enum IssueRepo {
    /// `"owner/repo"` — the branch prefix defaults to the repo name.
    Plain(String),
    /// `{ repo = "owner/repo", branch_prefix = "docs" }`. `branch_prefix`
    /// omitted → repo name; `""` → no prefix (collision risk accepted).
    Detailed {
        repo: String,
        branch_prefix: Option<String>,
    },
}

impl IssueRepo {
    /// The `"owner/repo"` spec, whichever form was used.
    fn spec(&self) -> &str {
        match self {
            IssueRepo::Plain(spec) => spec,
            IssueRepo::Detailed { repo, .. } => repo,
        }
    }

    /// Parse and validate the spec into `(owner, repo)`.
    pub fn repo_ref(&self) -> Result<(String, String)> {
        parse_owner_repo_spec(self.spec())
    }

    /// The explicit `branch_prefix` override, if any. `None` means "unset" (the
    /// caller defaults to the repo name); `Some("")` means "no prefix".
    fn branch_prefix(&self) -> Option<&str> {
        match self {
            IssueRepo::Plain(_) => None,
            IssueRepo::Detailed { branch_prefix, .. } => branch_prefix.as_deref(),
        }
    }
}

/// Parse `"owner/repo"` into its halves, rejecting anything that isn't exactly
/// one non-empty owner and one non-empty repo.
fn parse_owner_repo_spec(spec: &str) -> Result<(String, String)> {
    let trimmed = spec.trim();
    match trimmed.split_once('/') {
        Some((owner, repo)) if !owner.is_empty() && !repo.is_empty() && !repo.contains('/') => {
            Ok((owner.to_string(), repo.to_string()))
        }
        _ => bail!("invalid issue_repos entry `{spec}`: expected the form \"owner/repo\""),
    }
}

/// The default name for [`Config::blocked_label`], also used by callers that
/// run without a `ghwf.toml` (where there is no `Config` to read it from).
pub fn default_blocked_label() -> String {
    "blocked".to_string()
}

impl Config {
    /// The validated `(owner, repo)` of every `issue_repos` entry. A malformed
    /// entry is a hard error — better to fail loudly than silently drop an
    /// allowlist entry the user is relying on.
    pub fn issue_repo_refs(&self) -> Result<Vec<(String, String)>> {
        self.issue_repos.iter().map(IssueRepo::repo_ref).collect()
    }

    /// The branch-name prefix for an issue living in `(owner, repo)`:
    /// - `None` when the repo isn't a configured issue repo (e.g. it's the main
    ///   repo) or its entry sets `branch_prefix = ""` (opt out);
    /// - `Some(name)` to prefix with `name` — the entry's `branch_prefix`, or
    ///   the repo name by default.
    pub fn issue_branch_prefix(&self, owner: &str, repo: &str) -> Result<Option<String>> {
        for entry in &self.issue_repos {
            let (o, r) = entry.repo_ref()?;
            if o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo) {
                return Ok(match entry.branch_prefix() {
                    Some("") => None,
                    Some(prefix) => Some(prefix.to_string()),
                    None => Some(r),
                });
            }
        }
        Ok(None)
    }
}

/// The `[labels]` section: one GitHub label name per phase and per attention
/// state. All names are required once the section is present — partial
/// configs would make the sync's remove-undesired step ambiguous.
#[derive(Deserialize, Facet)]
pub struct LabelsConfig {
    /// Label names for each workflow phase.
    pub phase: PhaseLabels,
    /// Label names for each attention state.
    pub attention: AttentionLabels,
}

/// Label names for the `[labels.phase]` table.
#[derive(Deserialize, Facet)]
#[serde(rename_all = "kebab-case")]
// Mirror serde's rename so facet's reflection carries the kebab-case wire keys
// (facet doesn't read serde attributes).
#[facet(rename_all = "kebab-case")]
pub struct PhaseLabels {
    /// Label for the pre-plan phase (gathering information before planning).
    pub pre_plan: String,
    /// Label for the prep-and-plan phase (worktree created, plan being written).
    pub prep_and_plan: String,
    /// Label for the implement phase (coding the change).
    pub implement: String,
    /// Label for the review phase (change ready for human review).
    pub review: String,
    /// Label for the terminal finished phase.
    // Defaulted so a `[labels.phase]` table written before the `finished` phase
    // existed keeps parsing; new setups write it explicitly.
    #[serde(default = "default_finished_label")]
    pub finished: String,
}

/// The conventional name for the terminal `finished` phase label, used when a
/// pre-existing `[labels.phase]` table omits it.
fn default_finished_label() -> String {
    "ghwf:finished".to_string()
}

/// Label names for the `[labels.attention]` table.
#[derive(Deserialize, Facet)]
#[serde(rename_all = "kebab-case")]
// Mirror serde's rename so facet's reflection carries the kebab-case wire keys.
#[facet(rename_all = "kebab-case")]
pub struct AttentionLabels {
    /// Label for when the workflow is waiting on the user (needs a reply or
    /// approval).
    pub waiting_on_user: String,
    /// Label for when Claude is actively working the issue.
    pub waiting_on_claude: String,
    /// Label for when ghwf is preparing (e.g. creating a worktree).
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
            Phase::Finished => &self.phase.finished,
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
    pub fn all(&self) -> [&str; 8] {
        [
            &self.phase.pre_plan,
            &self.phase.prep_and_plan,
            &self.phase.implement,
            &self.phase.review,
            &self.phase.finished,
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

    /// Absolute path to the PR instructions file (which may not exist).
    pub fn pr_instructions_path(&self) -> PathBuf {
        match &self.config.pr_instructions {
            Some(p) => self.dir.join(p),
            None => self.dir.join(PR_INSTRUCTIONS_FILE),
        }
    }
}

/// Walk up from the current directory looking for a `ghwf.toml`, returning its
/// path if found.
pub fn locate() -> Option<PathBuf> {
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
             commands that create worktrees will require one. \
             Run `ghwf config init` to set one up."
        );
    }
}

/// Like [`find`], but error when no config is found.
pub fn require() -> Result<Located> {
    match find()? {
        Some(located) => Ok(located),
        None => bail!(
            "this step requires a {CONFIG_FILE} (with `worktrees_dir`) in this or a parent \
             directory; none found. Run `ghwf config init` to set one up, or use --no-branch \
             to work without one."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Located};
    use std::path::{Path, PathBuf};

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
    fn permission_mode_parses() {
        let config: Config = toml::from_str(
            r#"
            worktrees_dir = "worktrees"
            permission_mode = "auto"
            "#,
        )
        .unwrap();
        assert_eq!(config.permission_mode.as_deref(), Some("auto"));
    }

    #[test]
    fn permission_mode_defaults_to_none() {
        // Pre-existing configs without the key keep loading.
        let config: Config = toml::from_str(r#"worktrees_dir = "worktrees""#).unwrap();
        assert!(config.permission_mode.is_none());
    }

    #[test]
    fn delete_plan_on_approval_parses() {
        let config: Config = toml::from_str(
            r#"
            worktrees_dir = "worktrees"
            delete_plan_on_approval = true
            "#,
        )
        .unwrap();
        assert!(config.delete_plan_on_approval);
    }

    #[test]
    fn delete_plan_on_approval_defaults_to_false() {
        // Pre-existing configs without the key keep loading.
        let config: Config = toml::from_str(r#"worktrees_dir = "worktrees""#).unwrap();
        assert!(!config.delete_plan_on_approval);
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
        // Omitted from the table above: falls back to the serde default.
        assert_eq!(labels.for_phase(Phase::Finished), "ghwf:finished");
        assert_eq!(
            labels.for_attention(Attention::WaitingOnUser),
            "ghwf:needs-you"
        );
        assert_eq!(
            labels.for_attention(Attention::WaitingOnGhwf),
            "ghwf:preparing"
        );
        assert_eq!(labels.all().len(), 8);
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

    #[test]
    fn blocked_label_parses() {
        let config: Config = toml::from_str(
            r#"
            worktrees_dir = "worktrees"
            blocked_label = "needs-unblock"
            "#,
        )
        .unwrap();
        assert_eq!(config.blocked_label, "needs-unblock");
    }

    #[test]
    fn blocked_label_defaults_to_blocked() {
        // Pre-existing configs without the key keep loading and get the default.
        let config: Config = toml::from_str(r#"worktrees_dir = "worktrees""#).unwrap();
        assert_eq!(config.blocked_label, "blocked");
    }

    #[test]
    fn issue_repos_default_to_empty() {
        // Pre-existing configs without the key keep loading.
        let config: Config = toml::from_str(r#"worktrees_dir = "worktrees""#).unwrap();
        assert!(config.issue_repos.is_empty());
    }

    #[test]
    fn issue_repos_parse_both_forms_mixed() {
        let config: Config = toml::from_str(
            r#"
            worktrees_dir = "worktrees"
            issue_repos = [
                "Org/plain",
                { repo = "Org/with-prefix", branch_prefix = "wp" },
                { repo = "Org/no-prefix", branch_prefix = "" },
                { repo = "Org/default-prefix" },
            ]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.issue_repo_refs().unwrap(),
            [
                ("Org".to_string(), "plain".to_string()),
                ("Org".to_string(), "with-prefix".to_string()),
                ("Org".to_string(), "no-prefix".to_string()),
                ("Org".to_string(), "default-prefix".to_string()),
            ]
        );
        // Plain form → repo name; explicit prefix wins; "" opts out; an absent
        // prefix in the table form also defaults to the repo name.
        assert_eq!(
            config.issue_branch_prefix("Org", "plain").unwrap(),
            Some("plain".to_string())
        );
        assert_eq!(
            config.issue_branch_prefix("org", "with-prefix").unwrap(),
            Some("wp".to_string())
        );
        assert_eq!(
            config.issue_branch_prefix("Org", "no-prefix").unwrap(),
            None
        );
        assert_eq!(
            config.issue_branch_prefix("Org", "default-prefix").unwrap(),
            Some("default-prefix".to_string())
        );
        // A repo that isn't listed (e.g. the main repo) gets no prefix.
        assert_eq!(config.issue_branch_prefix("Org", "main").unwrap(), None);
    }

    #[test]
    fn issue_repos_malformed_entry_errors() {
        let config: Config = toml::from_str(
            r#"
            worktrees_dir = "worktrees"
            issue_repos = ["not-a-repo"]
            "#,
        )
        .unwrap();
        assert!(config.issue_repo_refs().is_err());
    }

    #[test]
    fn pr_instructions_resolves_relative_to_config_dir() {
        let config: Config = toml::from_str(
            r#"
            worktrees_dir = "worktrees"
            pr_instructions = "docs/pr.md"
            "#,
        )
        .unwrap();
        let located = Located {
            dir: PathBuf::from("/base"),
            config,
        };
        assert_eq!(
            located.pr_instructions_path(),
            Path::new("/base/docs/pr.md")
        );
    }

    #[test]
    fn pr_instructions_defaults_next_to_config() {
        // Pre-existing configs without the key keep loading and get the default.
        let config: Config = toml::from_str(r#"worktrees_dir = "worktrees""#).unwrap();
        let located = Located {
            dir: PathBuf::from("/base"),
            config,
        };
        assert_eq!(
            located.pr_instructions_path(),
            Path::new("/base/pull-request.md")
        );
    }
}
