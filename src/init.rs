use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use inquire::error::InquireError;
use inquire::validator::Validation;
use inquire::{Confirm, Select, Text};
use toml_edit::DocumentMut;

use crate::config;
use crate::git;
use crate::labels;

/// The two `ghwf.toml` placements the wizard offers.
const REPO_ROOT_LAYOUT: &str =
    "Repo root — ghwf.toml at the root of this git repo; worktrees in a gitignored directory under it";
const PARENT_DIR_LAYOUT: &str =
    "Parent dir — ghwf.toml in the current directory, alongside the repo and the worktrees directory";

/// Stub written when the wizard creates a `pull-request.md`.
const PR_STUB: &str = "\
Instructions for writing this project's pull request titles and bodies.
Free-form prose, read by Claude whenever it creates or updates a PR;
replace these examples with your own conventions.

- Keep the title short and imperative.
- Describe what the change does and why, not a file-by-file how.
";

/// `ghwf config init`: an interactive wizard that creates or extends
/// `ghwf.toml` — essentials when missing, then optional extras. All prompts
/// run first and nothing is written until a final confirmation, so aborting
/// (Esc/Ctrl-C) at any point mutates nothing.
pub fn run() -> Result<()> {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!("`ghwf config init` is interactive; run it in a terminal.");
    }
    let cwd = std::env::current_dir().context("failed to read the current directory")?;

    // Locate an existing config. The raw text goes through `toml_edit` rather
    // than the typed parser: a half-written file (say, missing
    // `worktrees_dir`) is exactly what the wizard is here to repair, and
    // edits must preserve the user's formatting and comments.
    let existing = config::locate();
    let (mut doc, mut config_dir) = match &existing {
        Some(path) => {
            println!("Found an existing config at {}.", path.display());
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let doc: DocumentMut = text.parse().with_context(|| {
                format!(
                    "{} is not valid TOML; fix it by hand and re-run",
                    path.display()
                )
            })?;
            let dir = path
                .parent()
                .expect("config path always has a parent directory")
                .to_path_buf();
            (doc, dir)
        }
        // The config's directory follows from the layout choice below.
        None => (DocumentMut::new(), cwd.clone()),
    };

    // Gather phase: every prompt runs and the outcomes accumulate here;
    // nothing on disk changes until after the final confirmation.
    let mut doc_changed = false;
    let mut create_worktrees_dir: Option<PathBuf> = None;
    let mut gitignore_line: Option<String> = None;
    let mut pr_stub: Option<PathBuf> = None;
    let mut run_labels = false;
    let mut labels_pointer = false;

    // Essentials: `worktrees_dir` is the one required key.
    if !doc.contains_key("worktrees_dir") {
        println!(
            "If you want to rearrange your directory layout, now — before the wizard \
             writes paths into the config — is the best time to do it. Esc aborts \
             without writing anything."
        );
        let main_repo = if existing.is_none() {
            let in_repo = git::is_inside_work_tree(&cwd);
            let options = vec![REPO_ROOT_LAYOUT, PARENT_DIR_LAYOUT];
            let suggested = if in_repo { 0 } else { 1 };
            let choice = prompt(
                Select::new("Which layout?", options)
                    .with_starting_cursor(suggested)
                    .with_help_message("the highlighted option matches what was detected here")
                    .prompt(),
            )?;
            if choice == REPO_ROOT_LAYOUT {
                if !in_repo {
                    bail!(
                        "the current directory is not inside a git work tree; \
                         run the wizard from inside the repo to use the repo-root layout."
                    );
                }
                config_dir = git::toplevel(&cwd)?;
                None
            } else {
                Some(ask_main_repo(&config_dir)?)
            }
        } else if doc.contains_key("main_repo") || git::is_inside_work_tree(&config_dir) {
            // Repairing an existing file: its location is fixed, and either
            // the repo is already configured or the config sits inside one.
            None
        } else {
            Some(ask_main_repo(&config_dir)?)
        };

        let worktrees_dir = prompt(
            Text::new("Directory for per-issue worktrees (relative to the config):")
                .with_default("worktrees")
                .with_validator(|input: &str| {
                    if input.trim().is_empty() {
                        Ok(Validation::Invalid("enter a directory name".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt(),
        )?;
        let worktrees_dir = worktrees_dir.trim().to_string();
        set_essentials(&mut doc, main_repo.as_deref(), &worktrees_dir);
        doc_changed = true;

        let worktrees_path = config_dir.join(&worktrees_dir);
        if !worktrees_path.exists()
            && prompt(
                Confirm::new(&format!("Create {}?", worktrees_path.display()))
                    .with_default(true)
                    .prompt(),
            )?
        {
            create_worktrees_dir = Some(worktrees_path);
        }

        // Repo-root layout: the worktrees live inside the repo's work tree,
        // so they must be ignored or they'd pollute every git status.
        if !doc.contains_key("main_repo")
            && git::is_inside_work_tree(&config_dir)
            && !git::is_ignored(&config_dir, &worktrees_dir)
        {
            let line = format!("/{}/", worktrees_dir.trim_matches('/'));
            if prompt(
                Confirm::new(&format!(
                    "Add `{line}` to .gitignore? (worktrees must not be tracked)"
                ))
                .with_default(true)
                .prompt(),
            )? {
                gitignore_line = Some(line);
            }
        }
    } else {
        println!("Essentials are already configured; checking optional extras.");
    }

    // Optional extras, each offered only when not already configured.
    if !doc.contains_key("priority_labels")
        && prompt(
            Confirm::new("Configure priority labels? (`ghwf next` prefers issues carrying them)")
                .with_default(false)
                .prompt(),
        )?
    {
        let input =
            prompt(Text::new("Priority labels, most urgent first (comma-separated):").prompt())?;
        let priority_labels = parse_priority_labels(&input);
        if priority_labels.is_empty() {
            println!("No labels given; skipping.");
        } else {
            set_priority_labels(&mut doc, &priority_labels);
            doc_changed = true;
        }
    }

    if !doc.contains_key("pr_instructions") {
        let path = config_dir.join(config::PR_INSTRUCTIONS_FILE);
        if !path.exists()
            && prompt(
                Confirm::new(&format!(
                    "Create a stub {}? (instructions Claude follows for PR titles and bodies)",
                    config::PR_INSTRUCTIONS_FILE
                ))
                .with_default(true)
                .prompt(),
            )?
        {
            pr_stub = Some(path);
        }
    }

    if !doc.contains_key("permission_mode")
        && prompt(
            Confirm::new(
                "Set a permission mode for launched Claude sessions? (recommended for unattended use)",
            )
            .with_default(true)
            .prompt(),
        )?
    {
        let mode = prompt(
            Text::new("Permission mode (passed as `claude --permission-mode`):")
                .with_default("auto")
                .prompt(),
        )?;
        let mode = mode.trim().to_string();
        if mode.is_empty() {
            println!("No mode given; skipping.");
        } else {
            set_permission_mode(&mut doc, &mode);
            doc_changed = true;
        }
    }

    if !doc.contains_key("delete_plan_on_approval")
        && prompt(
            Confirm::new(
                "Delete the plan commit from history once implementation is approved? \
                 (force-pushes the branch)",
            )
            .with_default(false)
            .prompt(),
        )?
    {
        set_delete_plan_on_approval(&mut doc);
        doc_changed = true;
    }

    if !doc.contains_key("blocked_label")
        && prompt(
            Confirm::new(
                "Customise the label `ghwf create-issue` marks follow-ups blocked with? \
                 (defaults to `blocked`)",
            )
            .with_default(false)
            .prompt(),
        )?
    {
        let label = prompt(
            Text::new("Blocked label name:")
                .with_default("blocked")
                .prompt(),
        )?;
        let label = label.trim().to_string();
        if label.is_empty() {
            println!("No label given; keeping the default.");
        } else {
            set_blocked_label(&mut doc, &label);
            doc_changed = true;
        }
    }

    if !doc.contains_key("issue_repos")
        && prompt(
            Confirm::new(
                "Allow working on issues from other repos? (the code, worktree, and PR \
                 still live in this repo)",
            )
            .with_default(false)
            .prompt(),
        )?
    {
        let input = prompt(
            Text::new("Additional issue repos, comma-separated (e.g. `Org/docs`):").prompt(),
        )?;
        let issue_repos = parse_issue_repos(&input);
        if issue_repos.is_empty() {
            println!("No repos given; skipping.");
        } else {
            set_issue_repos(&mut doc, &issue_repos);
            doc_changed = true;
            println!(
                "Tip: to shorten or disable the branch-name prefix for a repo, edit its \
                 entry into the table form, e.g. `{{ repo = \"Org/docs\", branch_prefix = \"docs\" }}` \
                 (see the README)."
            );
        }
    }

    if !doc.contains_key("labels") {
        if prompt(
            Confirm::new("Set up workflow status labels now? (creates labels in the GitHub repo)")
                .with_default(true)
                .prompt(),
        )? {
            run_labels = true;
        } else {
            labels_pointer = true;
        }
    }

    // Confirm, then execute.
    let config_path = config_dir.join(config::CONFIG_FILE);
    let mut actions = Vec::new();
    if doc_changed {
        let verb = if existing.is_some() {
            "update"
        } else {
            "write"
        };
        actions.push(format!("{verb} {}", config_path.display()));
    }
    if let Some(path) = &create_worktrees_dir {
        actions.push(format!("create directory {}", path.display()));
    }
    if let Some(line) = &gitignore_line {
        actions.push(format!(
            "append `{line}` to {}",
            config_dir.join(".gitignore").display()
        ));
    }
    if let Some(path) = &pr_stub {
        actions.push(format!("create a stub {}", path.display()));
    }
    if run_labels {
        actions.push(format!(
            "create the workflow status labels in the GitHub repo and add a [labels] section to {}",
            config_path.display()
        ));
    }
    if actions.is_empty() {
        println!("Nothing to do — everything offered here is already configured.");
        if labels_pointer {
            println!("Run `ghwf config labels` to set up workflow status labels.");
        }
        return Ok(());
    }

    println!("\nAbout to:");
    for action in &actions {
        println!("  - {action}");
    }
    if !prompt(Confirm::new("Proceed?").with_default(true).prompt())? {
        bail!("aborted; nothing was written.");
    }

    // The config file first: the labels setup appends to it on disk.
    let text = doc.to_string();
    // A pre-existing structural problem (e.g. a partial [labels] table)
    // surfaces here, before anything is written.
    let typed: config::Config = toml::from_str(&text).with_context(|| {
        format!(
            "the resulting {} would not parse; fix {} by hand and re-run",
            config::CONFIG_FILE,
            config_path.display()
        )
    })?;
    if doc_changed {
        std::fs::write(&config_path, &text)
            .with_context(|| format!("failed to write {}", config_path.display()))?;
        println!("Wrote {}.", config_path.display());
    }
    if let Some(path) = &create_worktrees_dir {
        std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        println!("Created {}.", path.display());
    }
    if let Some(line) = &gitignore_line {
        let path = config_dir.join(".gitignore");
        append_line(&path, line)?;
        println!("Added `{line}` to {}.", path.display());
    }
    if let Some(path) = &pr_stub {
        std::fs::write(path, PR_STUB)
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("Created {}; edit it to taste.", path.display());
    }
    if run_labels {
        let located = config::Located {
            dir: config_dir,
            config: typed,
        };
        // Best-effort from the wizard's point of view: the config is already
        // saved, and `ghwf config labels` can pick up where this left off
        // (it skips labels that already exist).
        if let Err(err) = labels::configure_at(&located) {
            eprintln!("warning: labels setup failed: {err:#}");
            labels_pointer = true;
        }
    }
    if labels_pointer {
        println!("Run `ghwf config labels` to set up workflow status labels.");
    }
    println!(
        "Done. If you haven't already, run `ghwf install` to set up the Claude Code integration."
    );
    Ok(())
}

/// Unwrap a prompt result, turning a cancelled or interrupted prompt into a
/// clean whole-wizard abort.
fn prompt<T>(result: Result<T, InquireError>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            bail!("aborted; nothing was written.")
        }
        Err(err) => Err(err).context("prompt failed"),
    }
}

/// Prompt for the `main_repo` path, pre-filled when exactly one git repo sits
/// among `config_dir`'s children, and validated to point at a repo.
fn ask_main_repo(config_dir: &Path) -> Result<String> {
    let candidates = detect_child_repos(config_dir);
    let mut text = Text::new("Path to the main git repo (relative to the config):");
    if candidates.len() == 1 {
        text = text.with_default(&candidates[0]);
    }
    let base = config_dir.to_path_buf();
    let answer = prompt(
        text.with_validator(move |input: &str| {
            if git::is_repo(&base.join(input.trim())) {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid(
                    format!("`{}` is not a git repository", input.trim()).into(),
                ))
            }
        })
        .prompt(),
    )?;
    Ok(answer.trim().to_string())
}

/// Immediate children of `dir` that are git repos (bare ones included),
/// sorted by name. Dot-directories are skipped.
fn detect_child_repos(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut repos: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .filter(|name| git::is_repo(&dir.join(name)))
        .collect();
    repos.sort();
    repos
}

/// Insert a key-value at the end of the document's root table (before any
/// sub-tables, per TOML semantics) with an explanatory `# comment` above it,
/// and a separating blank line unless it's the first thing in the file.
fn insert_with_comment(doc: &mut DocumentMut, key: &str, item: toml_edit::Item, comment: &str) {
    let first = doc.as_table().is_empty();
    doc.insert(key, item);
    let mut prefix = String::new();
    if !first {
        prefix.push('\n');
    }
    for line in comment.lines() {
        prefix.push_str("# ");
        prefix.push_str(line);
        prefix.push('\n');
    }
    if let Some(mut key) = doc.key_mut(key) {
        key.leaf_decor_mut().set_prefix(prefix);
    }
}

/// Write the essential keys: `main_repo` (when the layout calls for it) and
/// `worktrees_dir`.
fn set_essentials(doc: &mut DocumentMut, main_repo: Option<&str>, worktrees_dir: &str) {
    if let Some(main_repo) = main_repo {
        insert_with_comment(
            doc,
            "main_repo",
            toml_edit::value(main_repo),
            "Path to the main git repo, relative to this file's directory.",
        );
    }
    insert_with_comment(
        doc,
        "worktrees_dir",
        toml_edit::value(worktrees_dir),
        "Directory under which ghwf creates per-issue worktrees.",
    );
}

/// Write the `priority_labels` array.
fn set_priority_labels(doc: &mut DocumentMut, priority_labels: &[String]) {
    let array: toml_edit::Array = priority_labels.iter().map(String::as_str).collect();
    insert_with_comment(
        doc,
        "priority_labels",
        toml_edit::value(array),
        "Labels marking an issue as urgent, most urgent first (used by `ghwf next`).",
    );
}

/// Write the `permission_mode` key.
fn set_permission_mode(doc: &mut DocumentMut, mode: &str) {
    insert_with_comment(
        doc,
        "permission_mode",
        toml_edit::value(mode),
        "Permission mode for the Claude sessions ghwf launches, passed through\n\
         as `claude --permission-mode <value>`.",
    );
}

/// Write the `delete_plan_on_approval` key (only ever set to `true` — the
/// wizard offers it only when absent, and the default is `false`).
fn set_delete_plan_on_approval(doc: &mut DocumentMut) {
    insert_with_comment(
        doc,
        "delete_plan_on_approval",
        toml_edit::value(true),
        "When true, ghwf rewrites the plan commit out of the branch's history\n\
         once the implementation is approved (the draft PR is marked ready for\n\
         review), then force-pushes the branch. Skipped with a warning when it\n\
         can't be done safely; a no-op in --no-branch mode.",
    );
}

/// Write the `blocked_label` key.
fn set_blocked_label(doc: &mut DocumentMut, label: &str) {
    insert_with_comment(
        doc,
        "blocked_label",
        toml_edit::value(label),
        "Label `ghwf create-issue` applies to a follow-up to mark it blocked by\n\
         the issue it was filed from (default `blocked`).",
    );
}

/// Parse the comma-separated priority-labels answer: trimmed, empties dropped.
fn parse_priority_labels(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .collect()
}

/// Write the `issue_repos` key as an array of plain `"owner/repo"` strings. The
/// richer `{ repo = …, branch_prefix = … }` form is a documented hand-edit.
fn set_issue_repos(doc: &mut DocumentMut, issue_repos: &[String]) {
    let array: toml_edit::Array = issue_repos.iter().map(String::as_str).collect();
    insert_with_comment(
        doc,
        "issue_repos",
        toml_edit::value(array),
        "Repos whose issues may be worked on while the code, worktree, and PR\n\
         stay in this repo. Each entry is \"owner/repo\", or a table\n\
         { repo = \"owner/repo\", branch_prefix = \"…\" } to set the branch prefix.",
    );
}

/// Parse the comma-separated issue-repos answer: trimmed, empties dropped.
fn parse_issue_repos(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(str::to_string)
        .collect()
}

/// Append `line` to the file at `path`, creating it if needed and making sure
/// the previous content ends with a newline first.
fn append_line(path: &Path, line: &str) -> Result<()> {
    let mut text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line);
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        parse_issue_repos, parse_priority_labels, set_blocked_label, set_delete_plan_on_approval,
        set_essentials, set_permission_mode, set_priority_labels, PR_STUB,
    };
    use crate::config::Config;
    use std::path::PathBuf;
    use toml_edit::DocumentMut;

    #[test]
    fn fresh_essentials_parse_in_both_layouts() {
        // Parent-dir layout: main_repo written.
        let mut doc = DocumentMut::new();
        set_essentials(&mut doc, Some("repo.git"), "worktrees");
        let config: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(config.main_repo, Some(PathBuf::from("repo.git")));
        assert_eq!(config.worktrees_dir, PathBuf::from("worktrees"));

        // Repo-root layout: main_repo omitted.
        let mut doc = DocumentMut::new();
        set_essentials(&mut doc, None, "wt");
        let config: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(config.main_repo, None);
        assert_eq!(config.worktrees_dir, PathBuf::from("wt"));
    }

    #[test]
    fn fresh_file_starts_with_a_comment_not_a_blank_line() {
        let mut doc = DocumentMut::new();
        set_essentials(&mut doc, None, "worktrees");
        assert!(doc.to_string().starts_with("# Directory under which"));
    }

    #[test]
    fn editing_preserves_existing_bytes() {
        let original = "# Hand-written comment.\nworktrees_dir = \"worktrees\"  # trailing\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        set_priority_labels(&mut doc, &["urgent".to_string(), "soon".to_string()]);
        let out = doc.to_string();
        assert!(
            out.starts_with(original),
            "existing content was rewritten:\n{out}"
        );
        let config: Config = toml::from_str(&out).unwrap();
        assert_eq!(config.priority_labels, ["urgent", "soon"]);
    }

    #[test]
    fn insertion_lands_before_subtables() {
        // A key added to the root table must render before any [section]
        // header, or it would change meaning entirely.
        let original = "\
worktrees_dir = \"worktrees\"

# A section comment that must survive.
[labels.phase]
pre-plan = \"a\"
";
        let mut doc: DocumentMut = original.parse().unwrap();
        set_priority_labels(&mut doc, &["urgent".to_string()]);
        let out = doc.to_string();
        assert!(out.find("priority_labels").unwrap() < out.find("[labels.phase]").unwrap());
        assert!(out.contains("# A section comment that must survive."));
        let reparsed: DocumentMut = out.parse().unwrap();
        assert!(reparsed.contains_key("priority_labels"));
    }

    #[test]
    fn permission_mode_round_trips() {
        let mut doc = DocumentMut::new();
        set_essentials(&mut doc, None, "worktrees");
        set_permission_mode(&mut doc, "auto");
        let config: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(config.permission_mode.as_deref(), Some("auto"));
    }

    #[test]
    fn delete_plan_on_approval_round_trips() {
        let mut doc = DocumentMut::new();
        set_essentials(&mut doc, None, "worktrees");
        set_delete_plan_on_approval(&mut doc);
        let config: Config = toml::from_str(&doc.to_string()).unwrap();
        assert!(config.delete_plan_on_approval);
    }

    #[test]
    fn blocked_label_round_trips() {
        let mut doc = DocumentMut::new();
        set_essentials(&mut doc, None, "worktrees");
        set_blocked_label(&mut doc, "needs-unblock");
        let config: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(config.blocked_label, "needs-unblock");
    }

    #[test]
    fn priority_labels_answer_parses_loosely() {
        assert_eq!(
            parse_priority_labels(" urgent , soon ,, "),
            ["urgent", "soon"]
        );
        assert!(parse_priority_labels("  ").is_empty());
    }

    #[test]
    fn issue_repos_answer_parses_loosely() {
        assert_eq!(
            parse_issue_repos(" Org/docs , Org/wiki ,, "),
            ["Org/docs", "Org/wiki"]
        );
        assert!(parse_issue_repos("  ").is_empty());
    }

    #[test]
    fn set_issue_repos_writes_plain_string_array() {
        let mut doc = "worktrees_dir = \"worktrees\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        super::set_issue_repos(&mut doc, &["Org/docs".to_string()]);
        let config: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(
            config.issue_repo_refs().unwrap(),
            [("Org".to_string(), "docs".to_string())]
        );
    }

    #[test]
    fn pr_stub_is_nonempty_prose() {
        assert!(PR_STUB.lines().count() > 1);
    }
}
