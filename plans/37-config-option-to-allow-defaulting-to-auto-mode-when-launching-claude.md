# Plan: Config option to default to auto mode when launching Claude (#37)

## Goal

Let `ghwf.toml` specify a permission mode that the launcher passes to Claude
as `claude --permission-mode <value>`, so unattended sessions can run in the
new classifier-supervised "auto" mode (or any other mode) instead of stalling
on permission prompts. Absent option means today's behaviour. (Settled with
the user on the issue thread.)

## Design decisions (from pre-plan discussion)

- "Auto mode" is just another value of Claude Code's `--permission-mode`
  flag; the installed CLI lists `acceptEdits`, `auto`, `bypassPermissions`,
  `default`, `dontAsk`, `plan`. So the option is a free-form **string**,
  `permission_mode`, passed through verbatim — ghwf does not validate the
  value, the claude CLI already rejects invalid choices with a clear error,
  and pass-through stays correct as modes are added.
- The flag applies to **every** launch path: fresh sessions, resumed
  sessions (`--resume` and `--permission-mode` compose), and `--no-branch`.
- Launch paths that currently work without a config (existing worktree,
  `--no-branch`) keep working: config loading for this option is
  best-effort — no config, or a config that fails to load, means no flag.
- The docs' restriction on project-level `settings.json` setting
  `defaultMode: "auto"` doesn't apply to the CLI flag, so no workaround is
  needed. Auto mode needs Claude Code ≥ 2.1.83 and a 4.6+ Opus/Sonnet
  model; that's between the user and the claude CLI, not ghwf's concern.

## Changes

### 1. `src/config.rs` — new config key

- Add to `Config`:

  ```rust
  /// Permission mode passed to launched Claude sessions as
  /// `--permission-mode <value>` (e.g. "auto"). Absent means Claude's
  /// default prompting behaviour.
  pub permission_mode: Option<String>,
  ```

- Tests: the key parses; configs without it keep loading with `None`.

### 2. `src/launch.rs` — thread the mode into `exec_claude`

- In `run(...)`, resolve the mode once, near the top, best-effort:
  `config::find()` → `Ok(Some(located))` yields
  `located.config.permission_mode`; `Ok(None)` yields `None`; `Err` prints a
  warning (mirroring `refresh_main_repo`'s wording) and yields `None`.
- `exec_claude` gains a `permission_mode: Option<&str>` parameter and adds
  `cmd.args(["--permission-mode", mode])` when set. Both call sites (the
  `--no-branch` path and the worktree path) pass the resolved value.
- When a mode is being applied, mention it in the existing launch println
  (e.g. the "Resuming…"/"Starting a fresh Claude session…" lines) so the
  user can see why Claude came up in that mode.

### 3. `README.md` — document the key

- Add `permission_mode` to the Configuration section's example ghwf.toml
  with a comment: optional, passed as `claude --permission-mode <value>`,
  `"auto"` recommended for unattended use, omit for normal prompting.

## Out of scope

- A generic `claude_args` escape hatch (rejected in pre-plan as broader than
  the issue asks for).
- Validating the mode string or probing the installed claude version /
  model for auto-mode support — the claude CLI surfaces its own errors.
- Per-issue or per-launch overrides (e.g. a `--permission-mode` flag on
  `ghwf work-on`); follow-up material if ever needed.

## Verification

- `cargo test` (new config-parsing tests included).
- `cargo clippy` / `cargo fmt` clean.
- Manual: add `permission_mode = "auto"` to this repo's ghwf.toml, run
  `ghwf work-on <n>` from outside a session, and confirm the launched
  Claude starts in auto mode; remove the key and confirm default behaviour.
