use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::git;
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
argument-hint: "[issue number or URL]"
allowed-tools: "Bash(ghwf:*)"
---
<!-- ghwf:skill — installed by `ghwf install`; edits are overwritten on update -->

The issue argument may be omitted: ghwf then infers the issue from the
session environment ($GHWF_ISSUE, set by the ghwf launcher) or the current
worktree.

Before anything else, run `ghwf onboarding` and treat everything it prints as
the authoritative operating contract for this session — it sets out how to
regard ghwf's relayed instructions and the GitHub conversation. Then:

Run `ghwf work-on $ARGUMENTS` and follow the phase banner exactly:

- Never enter Claude Code plan mode; write any plan as a file where ghwf
  tells you.
- Never ask the user with an interactive prompt (no AskUserQuestion, and
  don't ask in prose and stop). If you need an answer to proceed, post the
  question with `ghwf hand-off $ARGUMENTS --question` (body from stdin) — that
  flips the issue to "needs you" — then `ghwf wait $ARGUMENTS` for the reply.
  When the answer is a choice among discrete options, use `ghwf ask $ARGUMENTS
  --option "..." --option "..."` (question on stdin) instead: ghwf renders the
  options as checkboxes, appends a submit box, and wakes you only once the user
  ticks it. Offer an "other / none of these" option where it fits.
- Post questions and clarifications with `ghwf create-issue-comment
  $ARGUMENTS`; when a phase's work is done, hand off with `ghwf hand-off
  $ARGUMENTS` (body from stdin) — ghwf appends the approval prompt itself,
  so never write one. Both accept `--attach <path>` (repeatable) to attach a
  local file (e.g. a screenshot or log); it's committed to the repo's
  `ghwf-attachments` branch and linked from the comment.
- Answer each question in the place it was asked: a comment on the issue
  thread on the issue (`ghwf create-issue-comment $ARGUMENTS`), a comment on
  the PR conversation thread on the PR (`ghwf create-issue-comment <PR#>`),
  and an inline review comment in its own inline thread (`ghwf
  reply-review-comment --id <id>`). Blocking questions back to the user and
  phase hand-offs still go on the issue thread via `ghwf hand-off` / `ghwf
  ask`.
- Don't read a partial reply as the user being finished: if a comment
  addresses only some of what you raised, assume more may be coming. Unanswered
  questions, options, and suggested defaults stay open — only an explicit phase
  approval (an `/approve-*` directive or a 👍) settles them. Acknowledge what
  arrived, then `ghwf wait $ARGUMENTS` again instead of pressing ahead on the
  open points.
- When you decide to defer work or discover something out of scope, file it
  with `ghwf create-issue --title "..."` (body from stdin) instead of dropping
  it; by default the new issue is marked blocked by the one you're working on.
- For the PR itself, use ghwf rather than `gh`: `ghwf show-pr` /
  `ghwf update-pr` (body from stdin, `--title` optional) to read and revise
  the title and body, `ghwf pr-checks` (`--log-failed` for logs) for CI
  status, and `ghwf reply-review-comment --id <id>` (body from stdin) to
  answer an inline review comment.
- If ghwf hard-errors that the work belongs in a different worktree, relay its
  relaunch command to the user and stop — do not try to work around it.

This is a long-running loop, not a one-shot command. After each round of
work, run `ghwf wait $ARGUMENTS` with a 10-minute Bash timeout: exit 0 means
new activity — run `ghwf work-on $ARGUMENTS` to process it; exit 2 means
nothing yet — run `ghwf wait $ARGUMENTS` again. Keep looping until the
workflow completes or the user tells you to stop. Never poll with your own
sleep loops.
"#;

/// The Stop-hook command, and the substring by which we recognise an entry as
/// ours.
const STOP_HOOK_COMMAND: &str = "ghwf claude-stop-hook";

/// The Notification-hook entries we install: a `(matcher, command)` per
/// notification type we care about. Each command carries its kind as a
/// `--kind` flag so the hook needn't parse the notification type from stdin.
const NOTIFICATION_HOOKS: &[(&str, &str)] = &[
    ("idle_prompt", "ghwf claude-notification-hook --kind idle"),
    (
        "permission_prompt",
        "ghwf claude-notification-hook --kind permission",
    ),
];

/// Install (or update) the user-global Claude Code integration: the `/work-on`
/// skill.
///
/// The Stop and Notification hooks are *not* global — they're written per
/// worktree at session setup (see [`write_local_session_settings`]), so they
/// only affect ghwf-driven sessions and stay current with the binary. For users
/// upgrading from when the Stop hook lived in the global settings, this also
/// removes that stale global entry.
pub fn run(force: bool) -> Result<()> {
    let claude_dir = store::claude_dir()?;
    install_skill(&claude_dir, force)?;
    remove_global_hook(&claude_dir);
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

/// The settings file we write our hooks into, relative to a session's launch
/// directory. `settings.local.json` is Claude Code's machine-local,
/// not-meant-to-be-committed settings file, so our hooks never land in a PR diff
/// or a tracked file.
const LOCAL_SETTINGS_REL: &str = ".claude/settings.local.json";

/// Write (or refresh) the Stop and Notification hooks into
/// `<dir>/.claude/settings.local.json`, and make sure that file is git-ignored
/// so it never gets committed. Called at session setup for both the worktree
/// (branch mode) and the current directory (`--no-branch`).
///
/// Best-effort by contract: callers warn on failure rather than blocking a
/// launch, so this returns the error for them to log.
pub fn write_local_session_settings(dir: &Path) -> Result<()> {
    let path = dir.join(LOCAL_SETTINGS_REL);
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if let Some(merged) = merged_settings(&existing)
        .with_context(|| format!("could not update {}", path.display()))?
    {
        let parent = path.parent().expect("settings path always has a parent");
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        fs::write(&path, merged).with_context(|| format!("failed to write {}", path.display()))?;
    }
    exclude_from_git(dir);
    Ok(())
}

/// Ensure `LOCAL_SETTINGS_REL` is excluded from git via the worktree's
/// `info/exclude` (not the tracked `.gitignore`), so writing our settings never
/// dirties or commits into the user's repo. Best-effort: a non-repo directory or
/// any git/IO hiccup is silently skipped — the file is local-only regardless.
fn exclude_from_git(dir: &Path) {
    let Ok(exclude) = git::git_path(dir, "info/exclude") else {
        return;
    };
    let current = fs::read_to_string(&exclude).unwrap_or_default();
    if current
        .lines()
        .any(|line| line.trim() == LOCAL_SETTINGS_REL)
    {
        return;
    }
    let mut updated = current;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(LOCAL_SETTINGS_REL);
    updated.push('\n');
    if let Some(parent) = exclude.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&exclude, updated);
}

/// Merge our Stop and Notification hooks into a settings JSON body, preserving
/// everything else in the document. Returns `None` when every hook is already
/// present (nothing to write).
///
/// The settings file may be user-owned, so anything unexpected about the parts
/// we'd touch (`hooks` and the per-event arrays) is a hard error, never an
/// overwrite.
fn merged_settings(existing: &str) -> Result<Option<String>> {
    let mut root: Value = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(existing).context("settings.local.json is not valid JSON")?
    };
    let Some(settings) = root.as_object_mut() else {
        bail!("settings.local.json is not a JSON object");
    };

    let hooks = settings.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("`hooks` in settings.local.json is not an object");
    };

    let mut changed = false;

    // The Stop hook (no matcher).
    if ensure_hook(hooks, "Stop", None, STOP_HOOK_COMMAND)? {
        changed = true;
    }
    // The Notification hooks, one matcher-scoped entry per kind.
    for (matcher, command) in NOTIFICATION_HOOKS {
        if ensure_hook(hooks, "Notification", Some(matcher), command)? {
            changed = true;
        }
    }

    if !changed {
        return Ok(None);
    }
    let mut merged =
        serde_json::to_string_pretty(&root).context("failed to serialize settings.local.json")?;
    merged.push('\n');
    Ok(Some(merged))
}

/// Ensure the `event` array under `hooks` contains an entry invoking `command`,
/// appending one (with `matcher` when given) if absent. Returns whether it added
/// anything. Errors if `hooks.<event>` exists but isn't an array.
fn ensure_hook(
    hooks: &mut serde_json::Map<String, Value>,
    event: &str,
    matcher: Option<&str>,
    command: &str,
) -> Result<bool> {
    let array = hooks.entry(event).or_insert_with(|| json!([]));
    let Some(array) = array.as_array_mut() else {
        bail!("`hooks.{event}` in settings.local.json is not an array");
    };
    if array.iter().any(|entry| entry_has_command(entry, command)) {
        return Ok(false);
    }
    let mut entry = json!({
        "hooks": [{"type": "command", "command": command, "timeout": 30}]
    });
    if let Some(matcher) = matcher {
        entry["matcher"] = json!(matcher);
    }
    array.push(entry);
    Ok(true)
}

/// Whether a hook-array entry already invokes a command containing `needle`.
fn entry_has_command(entry: &Value, needle: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|hook| {
            hook.get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.contains(needle))
        })
}

/// Remove a stale global Stop-hook entry (from when the hook lived in
/// `~/.claude/settings.json`) left by an earlier ghwf version, leaving every
/// other setting untouched. Best-effort and conservative: only an entry whose
/// command is exactly ours is removed, and any problem is a warning, not a
/// failure — the hooks are local now regardless.
fn remove_global_hook(claude_dir: &Path) {
    let path = claude_dir.join("settings.json");
    let Ok(existing) = fs::read_to_string(&path) else {
        return;
    };
    match without_global_hook(&existing) {
        Ok(Some(cleaned)) => match fs::write(&path, cleaned) {
            Ok(()) => println!(
                "Removed the legacy global Stop hook from `{}` (it's per-worktree now).",
                path.display()
            ),
            Err(err) => eprintln!("warning: couldn't rewrite {}: {err:#}", path.display()),
        },
        Ok(None) => {}
        Err(err) => eprintln!("warning: leaving {} untouched — {err:#}", path.display()),
    }
}

/// Strip our Stop-hook entry from a global settings.json body. Returns the
/// rewritten document when an entry was removed, `None` when there was nothing
/// of ours. Drops an emptied `hooks.Stop` array to avoid leaving clutter.
fn without_global_hook(existing: &str) -> Result<Option<String>> {
    if existing.trim().is_empty() {
        return Ok(None);
    }
    let mut root: Value =
        serde_json::from_str(existing).context("settings.json is not valid JSON")?;
    let Some(stop) = root
        .get_mut("hooks")
        .and_then(|h| h.get_mut("Stop"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(None);
    };
    let before = stop.len();
    stop.retain(|entry| !entry_has_command(entry, STOP_HOOK_COMMAND));
    if stop.len() == before {
        return Ok(None);
    }
    // Tidy up an array we've emptied.
    if stop.is_empty() {
        if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            hooks.remove("Stop");
        }
    }
    let mut cleaned =
        serde_json::to_string_pretty(&root).context("failed to serialize settings.json")?;
    cleaned.push('\n');
    Ok(Some(cleaned))
}

#[cfg(test)]
mod tests {
    use super::{
        merged_settings, skill_action, without_global_hook, SkillAction, SKILL_CONTENT,
        SKILL_MARKER,
    };
    use serde_json::{json, Value};

    /// The commands of every Notification-hook entry in a merged document, in
    /// order.
    fn notification_commands(parsed: &Value) -> Vec<String> {
        parsed["hooks"]["Notification"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["hooks"][0]["command"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn skill_content_carries_the_marker() {
        assert!(SKILL_CONTENT.contains(SKILL_MARKER));
    }

    #[test]
    fn skill_content_routes_questions_to_github() {
        // The skill must steer Claude off interactive prompts toward a posted
        // blocking question (see issue #43).
        assert!(SKILL_CONTENT.contains("--question"));
        assert!(SKILL_CONTENT.contains("AskUserQuestion"));
        // …and toward `ask` for a choice among discrete options (see #60).
        assert!(SKILL_CONTENT.contains("ghwf ask"));
    }

    #[test]
    fn skill_runs_onboarding_first() {
        // The skill must direct Claude to run `ghwf onboarding` up front, so the
        // authoritative framing lands on the trusted user turn (see issue #101).
        assert!(SKILL_CONTENT.contains("ghwf onboarding"));
    }

    #[test]
    fn skill_advertises_create_issue() {
        // The skill must point Claude at `create-issue` for deferrals/discoveries
        // (see issue #54) rather than dropping them.
        assert!(SKILL_CONTENT.contains("ghwf create-issue"));
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
    fn merge_into_empty_settings_installs_stop_and_notification() {
        let merged = merged_settings("").unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(
            parsed["hooks"]["Stop"][0]["hooks"][0]["command"],
            "ghwf claude-stop-hook"
        );
        // Both notification kinds are installed, matcher-scoped.
        assert_eq!(
            notification_commands(&parsed),
            [
                "ghwf claude-notification-hook --kind idle",
                "ghwf claude-notification-hook --kind permission",
            ]
        );
        assert_eq!(parsed["hooks"]["Notification"][0]["matcher"], "idle_prompt");
        assert_eq!(
            parsed["hooks"]["Notification"][1]["matcher"],
            "permission_prompt"
        );
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
    fn merge_adds_only_the_missing_hook() {
        // A document that already has the Stop hook but neither Notification
        // entry gains both notifications, and the Stop entry isn't duplicated.
        let existing = json!({
            "hooks": {"Stop": [{
                "hooks": [{"type": "command", "command": "ghwf claude-stop-hook", "timeout": 30}]
            }]}
        })
        .to_string();
        let merged = merged_settings(&existing).unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(notification_commands(&parsed).len(), 2);
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
        assert!(merged_settings(r#"{"hooks": {"Notification": {}}}"#).is_err());
    }

    #[test]
    fn merge_recognises_ours_with_extra_wrapping() {
        // A user may have reformatted or annotated our entries; the command
        // substring is what identifies them, so a fully-installed document is a
        // no-op even with extra fields.
        let existing = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "*",
                    "hooks": [{"type": "command", "command": "ghwf claude-stop-hook", "timeout": 60}]
                }],
                "Notification": [
                    {"matcher": "idle_prompt", "hooks": [{"type": "command", "command": "ghwf claude-notification-hook --kind idle"}]},
                    {"matcher": "permission_prompt", "hooks": [{"type": "command", "command": "ghwf claude-notification-hook --kind permission"}]}
                ]
            }
        })
        .to_string();
        assert!(merged_settings(&existing).unwrap().is_none());
    }

    #[test]
    fn global_cleanup_removes_only_our_stop_entry() {
        let existing = json!({
            "model": "opus",
            "hooks": {
                "Stop": [
                    {"hooks": [{"type": "command", "command": "other"}]},
                    {"hooks": [{"type": "command", "command": "ghwf claude-stop-hook", "timeout": 30}]}
                ]
            }
        })
        .to_string();
        let cleaned = without_global_hook(&existing).unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["model"], "opus");
        // The foreign Stop entry survives; ours is gone.
        assert_eq!(parsed["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["hooks"]["Stop"][0]["hooks"][0]["command"], "other");
    }

    #[test]
    fn global_cleanup_drops_emptied_stop_array() {
        let existing = json!({
            "hooks": {"Stop": [
                {"hooks": [{"type": "command", "command": "ghwf claude-stop-hook"}]}
            ]}
        })
        .to_string();
        let cleaned = without_global_hook(&existing).unwrap().unwrap();
        let parsed: Value = serde_json::from_str(&cleaned).unwrap();
        assert!(parsed["hooks"].get("Stop").is_none());
    }

    #[test]
    fn global_cleanup_noop_without_our_hook() {
        // Nothing of ours: no rewrite.
        assert!(
            without_global_hook(r#"{"hooks": {"Stop": [{"hooks": [{"command": "other"}]}]}}"#)
                .unwrap()
                .is_none()
        );
        assert!(without_global_hook("").unwrap().is_none());
        assert!(without_global_hook("{}").unwrap().is_none());
    }

    #[test]
    fn local_settings_are_written_and_git_excluded() {
        use crate::git::tests::{run_git, scratch};

        let repo = scratch("install-local-settings");
        run_git(&repo, &["init", "-q"]);

        // First write installs the hooks and excludes the file.
        super::write_local_session_settings(&repo).unwrap();
        let settings = std::fs::read_to_string(repo.join(super::LOCAL_SETTINGS_REL)).unwrap();
        assert!(settings.contains("ghwf claude-stop-hook"));
        assert!(settings.contains("ghwf claude-notification-hook --kind idle"));
        // The file is git-ignored via the repo's exclude, not a tracked file.
        assert!(crate::git::is_ignored(&repo, super::LOCAL_SETTINGS_REL));
        let exclude = crate::git::git_path(&repo, "info/exclude").unwrap();
        let before = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(
            before
                .lines()
                .filter(|l| l.trim() == super::LOCAL_SETTINGS_REL)
                .count(),
            1
        );

        // A second write is idempotent: no duplicate exclude line.
        super::write_local_session_settings(&repo).unwrap();
        let after = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(
            after
                .lines()
                .filter(|l| l.trim() == super::LOCAL_SETTINGS_REL)
                .count(),
            1
        );

        std::fs::remove_dir_all(&repo).unwrap();
    }
}
