use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::store;

/// Marker identifying skill content ghwf wrote. Its presence means the file is
/// ours to overwrite on update; its absence means a human wrote it, so we
/// refuse without `--force`.
const SKILL_MARKER: &str = "<!-- ghwf:skill";

/// The `/work-on` skill, written to `<claude_dir>/skills/work-on/SKILL.md`.
///
/// `disable-model-invocation` keeps Claude from triggering the skill on its
/// own, and `allowed-tools` pre-approves ghwf invocations so the wait loop
/// doesn't stall on permission prompts.
const SKILL_CONTENT: &str = r#"---
description: Drive ghwf on a GitHub issue.
disable-model-invocation: true
argument-hint: <issue number or URL>
allowed-tools: "Bash(ghwf:*)"
---
<!-- ghwf:skill — installed by `ghwf install`; edits are overwritten on update -->

Run `ghwf work-on $ARGUMENTS` and follow the phase banner exactly:

- Never enter Claude Code plan mode; write any plan as a file where ghwf
  tells you.
- In pre-plan, post questions and your final summary with
  `ghwf create-issue-comment $ARGUMENTS`.
- If ghwf hard-errors that the work belongs in a different worktree, relay its
  relaunch command to the user and stop — do not try to work around it.

This is a long-running loop, not a one-shot command. After each round of
work, run `ghwf wait $ARGUMENTS` with a 10-minute Bash timeout: exit 0 means
new activity — run `ghwf work-on $ARGUMENTS` to process it; exit 2 means
nothing yet — run `ghwf wait $ARGUMENTS` again. Keep looping until the
workflow completes or the user tells you to stop. Never poll with your own
sleep loops.
"#;

/// The command `install` writes into settings.json, and the substring by which
/// it later recognises an entry as ours.
const HOOK_COMMAND: &str = "ghwf claude-stop-hook";

/// Install (or update) the user-global Claude Code integration: the `/work-on`
/// skill and the Stop hook.
pub fn run(force: bool) -> Result<()> {
    let claude_dir = store::claude_dir()?;
    install_skill(&claude_dir, force)?;
    install_hook(&claude_dir)?;
    Ok(())
}

/// What to do with the skill file, given what's already there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SkillAction {
    // No file yet: write it.
    Install,
    // Our marker is present (or `--force` was given): overwrite.
    Update,
    // Already byte-identical: nothing to do.
    UpToDate,
    // Unrecognised content and no `--force`: hard error.
    Refuse,
}

/// Decide the skill action from the existing file content (if any).
fn skill_action(existing: Option<&str>, force: bool) -> SkillAction {
    match existing {
        None => SkillAction::Install,
        Some(content) if content == SKILL_CONTENT => SkillAction::UpToDate,
        Some(content) if content.contains(SKILL_MARKER) || force => SkillAction::Update,
        Some(_) => SkillAction::Refuse,
    }
}

/// Write (or update) `<claude_dir>/skills/work-on/SKILL.md`.
fn install_skill(claude_dir: &Path, force: bool) -> Result<()> {
    let path = claude_dir.join("skills").join("work-on").join("SKILL.md");
    let existing = fs::read_to_string(&path).ok();

    match skill_action(existing.as_deref(), force) {
        SkillAction::Refuse => bail!(
            "`{}` already exists and doesn't look like ghwf wrote it \
             (no `{SKILL_MARKER}` marker); re-run with --force to overwrite it.",
            path.display()
        ),
        SkillAction::UpToDate => {
            println!("The /work-on skill at `{}` is up to date.", path.display());
        }
        action @ (SkillAction::Install | SkillAction::Update) => {
            let dir = path.parent().expect("skill path always has a parent");
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
            fs::write(&path, SKILL_CONTENT)
                .with_context(|| format!("failed to write {}", path.display()))?;
            let verb = match action {
                SkillAction::Install => "Installed",
                _ => "Updated",
            };
            println!("{verb} the /work-on skill at `{}`.", path.display());
        }
    }

    // A legacy command file would define a colliding /work-on; it's the user's,
    // so warn rather than touch it.
    let legacy = claude_dir.join("commands").join("work-on.md");
    if legacy.is_file() {
        eprintln!(
            "warning: `{}` also defines /work-on and collides with the skill; \
             consider deleting it.",
            legacy.display()
        );
    }
    Ok(())
}

/// Merge our Stop hook into `<claude_dir>/settings.json`, creating the file
/// when absent.
fn install_hook(claude_dir: &Path) -> Result<()> {
    let path = claude_dir.join("settings.json");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    match merged_settings(&existing)
        .with_context(|| format!("could not update {}; fix it by hand", path.display()))?
    {
        Some(merged) => {
            fs::create_dir_all(claude_dir)
                .with_context(|| format!("failed to create {}", claude_dir.display()))?;
            fs::write(&path, merged)
                .with_context(|| format!("failed to write {}", path.display()))?;
            println!(
                "Installed the Stop hook (`{HOOK_COMMAND}`) in `{}`.",
                path.display()
            );
        }
        None => println!(
            "The Stop hook (`{HOOK_COMMAND}`) is already installed in `{}`.",
            path.display()
        ),
    }
    Ok(())
}

/// Merge our Stop hook into a settings.json body, preserving everything else
/// in the document. Returns `None` when the hook is already present.
///
/// Settings.json is user-owned, so anything unexpected about the parts we'd
/// touch (`hooks`, `hooks.Stop`) is a hard error, never an overwrite —
/// `--force` overrides our skill marker check, not the user's settings.
fn merged_settings(existing: &str) -> Result<Option<String>> {
    let mut root: Value = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(existing).context("settings.json is not valid JSON")?
    };
    let Some(settings) = root.as_object_mut() else {
        bail!("settings.json is not a JSON object");
    };

    let hooks = settings.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("`hooks` in settings.json is not an object");
    };
    let stop = hooks.entry("Stop").or_insert_with(|| json!([]));
    let Some(stop) = stop.as_array_mut() else {
        bail!("`hooks.Stop` in settings.json is not an array");
    };

    if stop.iter().any(contains_our_hook) {
        return Ok(None);
    }
    stop.push(json!({
        "hooks": [{"type": "command", "command": HOOK_COMMAND, "timeout": 30}]
    }));

    let mut merged =
        serde_json::to_string_pretty(&root).context("failed to serialize settings.json")?;
    merged.push('\n');
    Ok(Some(merged))
}

/// Whether a `hooks.Stop` entry already invokes our hook command.
fn contains_our_hook(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|hook| {
            hook.get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.contains(HOOK_COMMAND))
        })
}

#[cfg(test)]
mod tests {
    use super::{merged_settings, skill_action, SkillAction, SKILL_CONTENT, SKILL_MARKER};
    use serde_json::{json, Value};

    #[test]
    fn skill_content_carries_the_marker() {
        assert!(SKILL_CONTENT.contains(SKILL_MARKER));
    }

    #[test]
    fn skill_absent_installs() {
        assert_eq!(skill_action(None, false), SkillAction::Install);
    }

    #[test]
    fn skill_marked_updates() {
        let old = format!("---\n---\n{SKILL_MARKER} v0 -->\nold body\n");
        assert_eq!(skill_action(Some(&old), false), SkillAction::Update);
    }

    #[test]
    fn skill_identical_is_up_to_date() {
        assert_eq!(
            skill_action(Some(SKILL_CONTENT), false),
            SkillAction::UpToDate
        );
    }

    #[test]
    fn skill_unmarked_refuses_without_force() {
        assert_eq!(
            skill_action(Some("hand-written"), false),
            SkillAction::Refuse
        );
        assert_eq!(
            skill_action(Some("hand-written"), true),
            SkillAction::Update
        );
    }

    #[test]
    fn merge_into_empty_settings() {
        let merged = merged_settings("").unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        let command = &parsed["hooks"]["Stop"][0]["hooks"][0]["command"];
        assert_eq!(command, "ghwf claude-stop-hook");
    }

    #[test]
    fn merge_preserves_unrelated_settings() {
        let existing = r#"{
            "model": "opus",
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "x"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "other"}]}]
            }
        }"#;
        let merged = merged_settings(existing).unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["model"], "opus");
        assert_eq!(parsed["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        // The pre-existing Stop entry survives, ours is appended after it.
        assert_eq!(parsed["hooks"]["Stop"][0]["hooks"][0]["command"], "other");
        assert_eq!(
            parsed["hooks"]["Stop"][1]["hooks"][0]["command"],
            "ghwf claude-stop-hook"
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let merged = merged_settings("").unwrap().unwrap();
        assert!(merged_settings(&merged).unwrap().is_none());
    }

    #[test]
    fn merge_rejects_malformed_json() {
        assert!(merged_settings("{not json").is_err());
    }

    #[test]
    fn merge_rejects_wrong_shapes() {
        assert!(merged_settings(r#"[1, 2]"#).is_err());
        assert!(merged_settings(r#"{"hooks": "nope"}"#).is_err());
        assert!(merged_settings(r#"{"hooks": {"Stop": {}}}"#).is_err());
    }

    #[test]
    fn merge_recognises_ours_with_extra_wrapping() {
        // A user may have reformatted or annotated our entry; the command
        // substring is what identifies it.
        let existing = json!({
            "hooks": {"Stop": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": "ghwf claude-stop-hook", "timeout": 60}]
            }]}
        })
        .to_string();
        assert!(merged_settings(&existing).unwrap().is_none());
    }
}
