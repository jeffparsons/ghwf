# Plan: Harden Claude Code sessions against getting stuck (#96)

## Goal

Stop ghwf-launched sessions from silently wedging — sitting idle after a
network failure, parked on a permission prompt, or otherwise dropping out of
the `wait`/`work-on` loop — so you don't have to walk back to the machine that's
running Claude to un-stick it. The session should either recover itself or, when
that's unsafe, surface the problem to GitHub quickly so it reaches your phone.

This is detection + recovery, not prevention: per Claude Code's docs a
`PreToolUse` hook can't match `AskUserQuestion`, and `ExitPlanMode` is invisible
to the hook system, so we can't hard-block the "ask the user" path. What we
*can* use is the **`Notification` hook** (`idle_prompt`, `permission_prompt`) —
side-effect-only, but a perfect signal for an external monitor — plus the child
process's own exit, plus the existing Stop-hook nudge counter.

The onboarding/authority idea (treating ghwf + relayed GitHub comments as
authoritative) was split out to **#101** and is out of scope here.

## Background — what exists today

- **Global install** (`src/install.rs`): `ghwf install` writes the `/work-on`
  skill to `~/.claude/skills/work-on/SKILL.md` and merges a Stop hook
  (`ghwf claude-stop-hook`) into `~/.claude/settings.json`.
- **Stop hook** (`src/stop_hook.rs`): on every Stop event, binds the
  `session_id` (from stdin JSON) to an issue by scanning state files for a
  matching `prep.worktree_session_id`, and — if the workflow isn't concluded and
  `stop_nudges < NUDGE_CAP` (3) — blocks the stop, nudging Claude back into the
  loop. Local-only, fail-open, no network. After the cap it lets the stop
  through and Claude sits idle at its prompt.
- **Launcher / supervisor** (`src/launch.rs`, `src/next.rs`): outside a Claude
  session, `prepare()` builds a `Launch` (worktree, resume id, lease, model,
  permission mode). `run()` (single foreground session) spawns Claude and just
  `child.wait()`s. `forever` (pool worker, `next::run_forever`) loops
  `supervise_once()` → `monitor()`, which polls the issue state every 2 s and
  returns `Completed` when the workflow concludes (sends the shutdown gesture) or
  `UserQuit` when the child exits first. **Any** child exit before conclusion is
  read as `UserQuit`, and in `forever` that stops the worker.
- **`prep::ensure_worktree`** is the single worktree-creation point, shared by
  the prep-and-plan phase and the launcher (which creates the worktree as early
  as pre-plan).
- The session's `worktree_session_id` is recorded into `PrepState` by the
  in-session `work-on` run (`src/main.rs:788`), so hooks/recovery can bind to it
  only after the first in-session `work-on` — fine, that always runs at startup.
- **`refuse_to_start`** (`src/launch.rs`) is the existing template for "post to
  the issue + flip to waiting-on-user": `github::post_issue_comment` →
  `render::build_status_comment_body`, set `attention = WaitingOnUser`,
  `labels::sync`, `state::save`. The recovery path reuses this shape.

Recovery only applies to sessions **ghwf launched** (`run` / `forever`). When
`work-on` is invoked from inside an already-running Claude session there is no
external ghwf supervisor process; the Stop hook still nudges there. This is
inherent and acceptable — the issue is about the launched sessions.

## Design overview

1. **Move the hooks local, written at session setup.** Stop hook **and** a new
   Notification hook go into a worktree/cwd-local `.claude/settings.local.json`,
   written (and refreshed from the current binary) wherever a session is about
   to launch — both branch and `--no-branch`. No global hook fallback. The file
   is kept out of git via the worktree's git exclude, so it never lands in a PR
   diff or dirties your real repo. `ghwf install` keeps only the global skill.
2. **A Notification hook records an idle/blocked signal** into the issue state,
   local-only and fail-open like the Stop hook.
3. **The supervisor reads that signal + the child's exit** and applies a
   fast-on-unambiguous / patient-on-ambiguous recovery policy: communicate to
   GitHub immediately, auto-resume quickly on unambiguous lockups, hold off 10
   minutes on ambiguous ones, cap the restarts, and park on "needs you" past the
   cap.
4. **Recovery covers both `run` and `forever`** by unifying them onto one
   supervised loop.
5. **Interactive-prompt situations are surfaced to GitHub** via the same
   notification signal (best-effort, since `AskUserQuestion` itself isn't
   hookable).

## Changes

### 1. Local hook settings written at setup — `src/install.rs` (+ caller hooks)

Generalise the settings-merge logic so it can emit both events into a target
file, and add a worktree-local writer.

- Add a `settings_with_hooks(existing: &str) -> Result<Option<String>>` (or
  extend `merged_settings`) that merges **both** a `Stop` entry
  (`ghwf claude-stop-hook`) and two `Notification` entries into a settings JSON
  body, matcher-scoped:

  ```json
  "Notification": [
    {"matcher": "idle_prompt",       "hooks": [{"type":"command","command":"ghwf claude-notification-hook --kind idle",       "timeout": 30}]},
    {"matcher": "permission_prompt", "hooks": [{"type":"command","command":"ghwf claude-notification-hook --kind permission", "timeout": 30}]}
  ]
  ```

  Carrying `--kind` as our own CLI arg (rather than parsing the notification
  type out of the hook's stdin JSON) keeps us off undocumented stdin fields.
  Keep the idempotency + "preserve unrelated settings / hard-error on wrong
  shapes" guarantees the current `merged_settings` has, applied per event.

- Add `install::write_local_session_settings(dir: &Path) -> Result<()>` that
  merges the hooks into `<dir>/.claude/settings.local.json` (creating it) and
  ensures it's git-excluded (below). Best-effort from the callers — a failure
  warns but never blocks a launch (mirrors the rest of the launcher).

- **Git exclude, not `.gitignore`.** To avoid touching tracked files, append
  `.claude/settings.local.json` to the worktree's git exclude file, located via
  `git rev-parse --git-path info/exclude` (correct for linked worktrees), if not
  already present. This covers both the dedicated worktree and the
  `--no-branch` real repo.

- **Call sites:**
  - `prep::ensure_worktree` (`src/prep.rs`) — after the worktree path is known
    (both the create and the already-exists branches), write/refresh the local
    settings. This is the "setup phase" and gives "new worktrees always get the
    latest hooks."
  - `launch::prepare` (`src/launch.rs`) — in the `--no-branch` arm, write into
    the current directory before returning the `Launch`; and in the
    existing-worktree arm, refresh (so a worktree created by an older binary
    picks up new hooks on its next launch).

### 2. `ghwf install` drops the hook, keeps the skill — `src/install.rs`, `src/main.rs`

- `install::run` installs only the skill now. Remove `install_hook` from the
  global path (the hook lives local). Update the `Install` command doc comment
  in `src/main.rs` (currently mentions the Stop hook).
- **Migration:** existing users have a Stop hook in `~/.claude/settings.json`
  from a previous `ghwf install`. It's harmless (still binds by session id and
  no-ops for non-ghwf sessions), but to honour "don't smear global state" add a
  one-line cleanup: on `install`, if our hook command is present in the global
  `settings.json`, remove just that entry (leaving everything else), printing
  what it did. (Reuse the existing `contains_our_hook` matcher to find it.)
  Keep this conservative — only remove an entry whose command is exactly ours.

### 3. New Notification-hook subcommand — `src/main.rs`, new `src/notification_hook.rs`

- Add `Commands::ClaudeNotificationHook { kind: NotificationKind }` (hidden,
  like `ClaudeStopHook`), dispatched to `notification_hook::run(kind)`.
- `NotificationKind` ∈ `{ Idle, Permission }` (clap `ValueEnum`).
- `notification_hook::run`: read the hook JSON from stdin (need only
  `session_id`), reuse the Stop hook's `find_bound_issue` (lift it into a shared
  spot — e.g. a small `session_binding` helper module, or make `stop_hook`'s
  finder `pub(crate)`), and if it resolves to an unconcluded issue, record the
  signal into state and exit 0. Fail-open and **no network**, exactly like the
  Stop hook (a Notification hook's output is ignored anyway).

### 4. State: the idle/blocked signal — `src/state.rs`

Add to `IssueState` (all `#[serde(default)]`, back-compatible with existing
files):

```rust
/// Set by the Notification hook when a launched session goes idle / parks on a
/// permission prompt. Read and cleared by the supervisor. `seq` increases on
/// every notification so the supervisor can tell a fresh event from one it has
/// already acted on.
pub session_alert: Option<SessionAlert>,
```

```rust
pub struct SessionAlert {
    pub kind: AlertKind,        // Idle | Permission
    pub session_id: String,     // guard against a stale signal from a prior session
    pub at: i64,                // unix seconds the hook fired (for the 10-min grace)
    pub seq: u64,               // monotonic; bumped each fire
}
```

The hook bumps `seq` and overwrites `kind`/`at`/`session_id`. `work-on` clears
`session_alert` (and resets `stop_nudges`, as it already does) whenever it
observes new activity, so a recovered, working session starts clean.

### 5. Supervisor recovery — `src/launch.rs` (+ `src/next.rs`)

Rework `monitor` into a recovery-aware supervise loop and unify `run` onto it.

- **One supervised entry point.** Extract the spawn/monitor/recover cycle into a
  `supervise(launch) -> Result<Outcome>` used by both `run` and
  `forever`/`supervise_once`. `run` swaps its bare `child.wait()` for this so a
  foreground session gets recovery too. (In `run` the user shares the terminal;
  a bring-down + `--resume` is visible but acceptable — and is exactly the
  un-sticking the issue asks for. Flagged as a decision below.)

- **The loop**, per spawned child:
  1. Poll every `POLL_INTERVAL` (2 s), as today, for conclusion (→ shutdown +
     `Completed`) and child exit.
  2. **On child exit before conclusion**, classify:
     - exit code `0` → clean user quit → return `UserQuit` (respect it).
     - non-zero → **crash (unambiguous)** → recover.
  3. **On each poll, load `session_alert`**; if it's for the current session and
     `seq` is newer than the last one handled:
     - **Communicate quickly (always):** post a status comment to the issue and
       flip `attention = WaitingOnUser` + `labels::sync` (reuse the
       `refuse_to_start` shape), once per alert.
     - **Classify for auto-recovery:**
       - *Unambiguous lockup* → recover quickly. Specifically: the Stop hook has
         given up — `stop_nudges >= NUDGE_CAP` — and the session is now idle
         (`kind == Idle`). The loop is definitively broken.
       - *Ambiguous* (`kind == Idle` with `stop_nudges < NUDGE_CAP`, or
         `kind == Permission`) → do **not** auto-recover until
         `now - at >= 10 min`; then, if still flagged, recover. (A permission
         wall may not be fixed by a restart, but after 10 min with no human
         intervention a resume is the best remaining move; the comment already
         told you about it.)
  4. **Recover** = bring the child down (reuse `shutdown`'s escalation), then
     re-`spawn` with `--resume` recomputed from the freshly-read state
     (`resumable_session`), re-running `/work-on`. Increment an **in-memory**
     restart counter.
  5. **Cap:** after `MAX_AUTO_RESTARTS` (start at 2), stop auto-resuming, post a
     final "this needs you — parked" comment, flip to waiting-on-user, and return
     `UserQuit` (so `forever` stops on this issue and leaves it resumable; a
     fresh `work-on`/launch starts the count over).

  The restart counter is in-memory in the supervisor (simplest; a supervisor
  crash resets it, which is acceptable since the issue is then resumable). New
  constants: `RECOVERY_GRACE = 10 min`, `MAX_AUTO_RESTARTS = 2`.

- **Keep the resume id correct across recovery.** The lease is held by the live
  `Launch`, so we don't re-`prepare` (that would see the lease as live).
  Instead recompute `resume` in-place from the recorded `worktree_session_id`
  before each re-spawn.

- `next::run_forever`: unchanged in shape — `Completed` → next issue,
  `UserQuit` → stop. Recovery happens inside `supervise`, transparent to it.

### 6. Surface interactive prompts (item 5)

This falls out of §5 step 3's "communicate quickly": an `idle_prompt` /
`permission_prompt` is precisely the visible symptom of Claude having opened an
interactive prompt (including, best-effort, an `AskUserQuestion` dialog once it
idles). The posted comment names what we saw ("the session appears to be idle /
waiting on input or a permission prompt") and points you at the machine if you
want to look. The soft skill guidance against `AskUserQuestion`/plan mode stays
as the first line of defence; we can't hard-block it.

### 7. Docs & config surface

- README: update the install/integration description (hooks are now local,
  written at setup; `ghwf install` is skill-only) and add a short "recovery"
  paragraph to the phases/overview describing the idle-detection + auto-resume
  behaviour and when it parks on "needs you".
- No new `ghwf.toml` field is strictly required (constants can ship hard-coded
  first). **Open question below:** whether to expose grace/cap/enable as config.
  If yes, follow CLAUDE.md's "adding a config option" checklist (doc comment,
  `init` wizard, README example, `config example` + guard).

## Testing

Unit-testable without a live Claude (mirroring the existing suites):

- **`install`**: `settings_with_hooks` merges both Stop and Notification entries,
  is idempotent, preserves unrelated settings, hard-errors on wrong shapes;
  global-hook cleanup removes exactly our entry and leaves others.
- **Git exclude**: writing into a temp git repo / linked worktree adds the line
  once and is idempotent (reuse `git::tests` helpers).
- **`notification_hook`**: binds by session id like the Stop hook (lift/share
  its `find_bound_issue` tests); records `session_alert` with the right `kind`
  and a bumped `seq`; no-ops (fail-open) on bad stdin / no binding / concluded.
- **Recovery classification** (pure function over `(SessionAlert, stop_nudges,
  now, exit status)` → `RecoveryAction { None | Recover | ParkAndStop }`): pull
  the decision out of the IO loop so it's a table test — unambiguous-idle
  recovers immediately; ambiguous waits for the grace then recovers; permission
  waits for the grace; non-zero exit recovers; zero exit is a user quit; the cap
  flips to park.
- **State**: `session_alert` round-trips; absent in old files defaults to `None`.

The spawn/monitor IO and the actual `--resume` re-spawn stay thin and are
covered by the existing `shutdown_terminates_and_reaps_a_child`-style tests; the
recovery *decision* is where the logic-heavy tests live.

## Decisions deferred / open questions

These don't block writing the code; calling them out for review:

1. **Apply recovery to `run` (foreground) too, or only `forever`?** Plan assumes
   yes (consistency, and the issue's "don't walk to the machine" applies to a
   single launched session as well). Easy to scope to `forever` only if
   preferred.
2. **User-quit vs crash heuristic.** Exit code `0` = quit, non-zero = crash. A
   double-Ctrl-C quit can exit non-zero (≈130) and would earn one auto-resume
   before the cap. Acceptable? Alternative: treat a SIGINT-coded exit as a quit.
3. **Expose timing/cap as `ghwf.toml` config**, or ship hard-coded constants
   first (10 min grace, 2 restarts)? Plan ships constants; config is a small
   follow-up if wanted.
4. **Permission-prompt recovery.** A restart won't clear a recurring permission
   wall; the real fix is `permission_mode`. Plan still resumes after the grace
   (best remaining move) but leans on the immediate comment. OK, or never
   auto-resume on `permission` and only notify?

## Out of scope

- The onboarding / "treat ghwf + relayed GitHub comments as authoritative"
  work → **#101** (blocked by this).
- Hard-blocking `AskUserQuestion` / plan mode — not possible via hooks.
