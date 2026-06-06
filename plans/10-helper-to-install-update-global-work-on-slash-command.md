# Plan — `ghwf install`: global `/work-on` skill + Stop hook (#10)

Today the `/work-on` slash command is a copy-paste snippet in the README,
installed per-project, and the keep-working loop (#16) is best-effort prose
in the phase banners. This adds a `ghwf install` subcommand that idempotently
installs/updates two user-global artifacts, embedded in the binary:

1. a `/work-on` **skill** at `<claude_dir>/skills/work-on/SKILL.md` (the
   modern form of custom slash commands; legacy `commands/*.md` still works
   but skills add frontmatter we want), and
2. a **Stop hook** in `<claude_dir>/settings.json` that runs
   `ghwf claude-stop-hook` — a new hidden subcommand that blocks Claude from
   stopping while its session's workflow issue is still active, telling it to
   resume the `wait`/`work-on` loop.

`<claude_dir>` is `$CLAUDE_CONFIG_DIR` or `~/.claude`, exactly as
`launch::claude_dir()` already resolves it.

Decisions locked on the issue thread:

1. **Hook included** — the Stop hook ships in this issue, not a follow-up.
   It's what turns the #16 "keep going until there's nothing left" behaviour
   from best-effort prose into something enforced.
2. **Name: `ghwf install`** — room to grow into installing anything else
   ghwf needs.
3. **Refuse unless `--force`** — never silently clobber content we didn't
   write.
4. **Skill stays `/work-on`** — collisions handled if/when they bite.

## 1. CLI surface (`main.rs`)

Two new subcommands:

- `Install { #[arg(long)] force: bool }` → `install::run(force)`.
- `ClaudeStopHook` → `stop_hook::run()`, marked `#[command(hide = true)]`
  with a doc comment saying it's the Stop-hook entry point Claude Code
  invokes, not for humans.

`launch::claude_dir()` moves to `store.rs` (pub(crate)) so `launch` and
`install` share it.

## 2. The skill (`install.rs`)

Embedded as a string constant. Shape (final wording at implementation time):

```markdown
---
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
- If ghwf hard-errors that the work belongs in a different worktree, relay
  its relaunch command to the user and stop — do not try to work around it.

This is a long-running loop, not a one-shot command. After each round of
work, run `ghwf wait $ARGUMENTS` with a 10-minute Bash timeout: exit 0 means
new activity — run `ghwf work-on $ARGUMENTS` to process it; exit 2 means
nothing yet — run `ghwf wait $ARGUMENTS` again. Keep looping until the
workflow completes or the user tells you to stop. Never poll with your own
sleep loops.
```

`disable-model-invocation: true` keeps Claude from triggering it spuriously;
`allowed-tools` pre-approves ghwf invocations so the loop doesn't stall on
permission prompts (verify the exact permission-rule syntax against current
Claude Code docs during implementation).

Install logic for `skills/work-on/SKILL.md`:

- absent → write it (creating directories);
- present and contains the `<!-- ghwf:skill` marker → overwrite (update);
- present without the marker → refuse with a clear error unless `--force`.

Also warn (don't touch) when `<claude_dir>/commands/work-on.md` exists — a
legacy command file would collide with the skill.

## 3. Settings merge (`install.rs`)

`<claude_dir>/settings.json` is user-owned, so edits are surgical: parse into
`serde_json::Value` (preserving unknown keys), ensure
`hooks.Stop[*].hooks[*]` contains an entry whose command is
`ghwf claude-stop-hook`, append `{"hooks": [{"type": "command", "command":
"ghwf claude-stop-hook", "timeout": 30}]}` to `hooks.Stop` when absent, and
write back pretty-printed. Recognise ours by the command string containing
`ghwf claude-stop-hook`, so a present entry makes the merge a no-op.

- Missing file → create it containing just our hook.
- Unparseable JSON, or `hooks`/`hooks.Stop` of the wrong type → hard error
  telling the user to fix the file by hand, even under `--force` (the flag
  overrides our marker check, not their broken settings).

The merge is pure (`fn merged_settings(existing: &str) -> Result<Option<String>>`,
`None` = already installed) for testability.

`install` finishes by reporting what it wrote, updated, or skipped.

## 4. The Stop hook (`stop_hook.rs`)

Claude Code invokes the hook with JSON on stdin (`session_id`,
`stop_hook_active`, …) on every Stop event in every session, so the rules
are: consult only local state (no network), and fail open — any parse or IO
error means exit 0 with no output (allow the stop, never break a session).

Resolution: scan `<data_dir>/issues/*/*/*.json` for an `IssueState` whose
`prep.worktree_session_id` matches the incoming `session_id`; on multiple
matches take the most recently modified state file. No match (including
no-branch sessions, which never record a session id) → allow.

Decision, given the bound issue:

- `issue_closed` (new flag, §5) → allow; the workflow is finished.
- `stop_nudges >= 3` → allow; three consecutive nudges without anything new
  arriving means Claude is stuck or the user wants out — stop fighting.
- otherwise → increment and persist `stop_nudges`, emit
  `{"decision": "block", "reason": …}` and exit 0. The reason names the
  issue and phase and restates the loop contract (mirroring
  `render::wait_instruction`): run `ghwf wait <n>` with a 10-minute timeout;
  exit 0 → `ghwf work-on <n>`; exit 2 → wait again — and ends with "if the
  user has explicitly told you to stop working on this issue, stop instead."

`stop_hook_active` is parsed but unused — the nudge counter subsumes it, and
it would otherwise cap us at a single nudge per natural stop.

## 5. State (`state.rs`, `main.rs`)

Two new `IssueState` fields, both `#[serde(default)]`:

- `issue_closed: bool` — set by every `work-on` run from
  `issue_data.state != "open"` (the fetch is already in hand).
- `stop_nudges: u32` — incremented by the hook (§4); reset to 0 by `work-on`
  whenever it observes anything new (a consumed directive, a phase
  transition, or a non-empty digest), so the cap only counts stops where
  nothing had changed.

## 6. README

Replace the copy-paste snippet in "The `/work-on` slash command" with
`ghwf install`: what it installs (skill + Stop hook), where, the `--force`
semantics, and a short subsection on how the Stop hook keeps a session in
the loop and when it lets go (issue closed, nudge cap, or the user saying
stop).

## 7. Tests

- `install.rs`: skill action choice (absent / marked / unmarked / unmarked
  with `--force`); `merged_settings` — empty file, settings without `hooks`,
  hook already present (no-op), other hooks preserved byte-for-byte in
  value terms, malformed JSON and wrong-typed `hooks.Stop` error.
- `stop_hook.rs`: decision logic as a pure function over the parsed input
  and resolved state — no binding → allow, active issue → block (reason
  names issue and phase), `issue_closed` → allow, nudge cap → allow,
  counter increments on block.
- `state.rs`: old state files load with the new fields defaulted.

Build order: 1 + 5 (plumbing), then 2–4, then 6–7.

## Out of scope / punted

- Project-level (`.claude/`) installs and an `uninstall` subcommand.
- Migrating/removing legacy `commands/work-on.md` files (we warn only).
- Plugin/marketplace distribution of the skill.
- Wiring `wait` itself into nudge-counter resets (work-on's reset suffices).
