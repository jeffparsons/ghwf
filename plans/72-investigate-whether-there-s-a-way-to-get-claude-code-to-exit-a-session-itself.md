# Plan: supervisor-spawned Claude + `ghwf next --forever`

## Background / investigation outcome

The issue asked whether Claude Code can end its own interactive session and
return control to the calling process (no `-p`). Investigation (recorded on the
issue) concluded **no**: Claude can't execute a `/exit` it emits as text, no
hook (Stop with `continue:false`, `SessionEnd`, etc.) can terminate the CLI
process, and `--max-turns`/lifetime limits are print-mode only. The only ways to
make the process exit are a human typing the exit gesture or an external process
signalling it.

So rather than the *session* exiting itself, ghwf becomes a **supervisor**: it
spawns Claude as a child (today it `exec`s, replacing itself), monitors the
issue's workflow state, and — when the workflow concludes — signals the child to
shut down. On top of that primitive, `ghwf next --forever` loops as a
self-perpetuating pool worker: pick an issue → run Claude to completion → bring
the session down → pick the next.

## Goals

1. Always **spawn** Claude as a child instead of `exec`-replacing the ghwf
   process (Jeff's call: consistency, and a home for future supervisor logic).
   Single-issue behaviour stays identical from the user's point of view.
2. Add `ghwf next --forever`: keep claiming and working issues one at a time,
   shutting each Claude session down on completion, until the user steps in.

## Design

### 1. `spawn_claude` replaces `exec_claude` (`src/launch.rs`)

Replace the `exec_claude` helper with a `spawn_claude` that builds the same
`Command` (env `$GHWF_ISSUE`, optional `--resume`/`--permission-mode`, trailing
`/work-on`, cwd = worktree) but **spawns** it with inherited stdio so it keeps
the terminal and stays interactive/subscription-billed. Two entry behaviours:

- **Single (default) mode** — used by every existing launch path
  (`launch::run`, both the `--no-branch` and worktree branches). Spawn, install
  the supervisor signal guard (below), `wait()` for the child, then
  `std::process::exit(child_status.code().unwrap_or(1))` so the shell sees
  Claude's exit code. This is exactly what the current non-unix branch of
  `exec_claude` already does; we drop the unix `exec` branch entirely.
- **Supervised mode** — used by `--forever`. Spawn and return the child handle
  (plus the resolved issue coords) to the caller's monitor loop instead of
  exiting.

Because we no longer `exec`, the `#[cfg(unix)] CommandExt::exec` path goes away;
`std::env::set_current_dir` on the parent is replaced by `Command::current_dir`
on the child so the supervisor's own cwd is untouched between iterations.

### 2. Supervisor signal handling (the one genuinely new concern)

With `exec` there is a single process; with spawn, ghwf and Claude share the
foreground process group, so a terminal Ctrl-C (SIGINT) is delivered to **both**.
We must preserve today's feel: Claude handles its own Ctrl-C exit gesture, and a
stray Ctrl-C must never kill the supervisor out from under a live child
(which would return the shell prompt while Claude keeps running, reparented).

Approach (simple, low-code): while a child is running, the supervisor **ignores**
`SIGINT` (and `SIGQUIT`); it just blocks in `wait()` and reaps. Add the `libc`
crate and use `libc::signal(SIGINT, SIG_IGN)` / restore afterwards (single mode
exits anyway; forever mode restores between iterations or keeps them ignored for
the loop's lifetime). Leave `SIGTSTP` (Ctrl-Z) at default — suspending the child
just blocks the supervisor's `wait()`, which is fine.

- Note in the plan for the implementer: a more "correct" job-control approach is
  to put the child in its own process group and `tcsetpgrp` it to the
  foreground so terminal signals reach only the child. That's more code
  (SIGTTOU handling, pgrp setup). Start with the ignore-SIGINT approach; fall
  back to process-group/foreground handling only if testing shows the simple
  one misbehaves.

### 3. Completion detection (`--forever`)

No new IPC channel: reuse the per-issue **state file** the Stop hook already
keys off. The terminal predicate is exactly the inverse of
`stop_hook::should_block`'s early-outs: `state.issue_closed ||
state.pr_outcome.is_some()`. Extract that into a shared
`state`-level helper (e.g. `IssueState::is_concluded()`), and have both
`stop_hook` and the supervisor call it so they can't drift.

**Ordering guarantee that makes polling safe:** in the in-session `work_on`, the
final status/conclusion comment is posted (main.rs ~572–621) *before*
`state::save` persists the terminal outcome (main.rs:652). So when the
supervisor observes a concluded state file, Claude's durable final comment is
already on GitHub — signalling shutdown then cannot truncate it.

Monitor loop per child: poll `state::load(owner, repo, number)` on a short fixed
interval (~2–3 s) while the child is alive. Outcomes:

- **State becomes concluded** → ghwf-initiated shutdown: send the shutdown
  gesture to the child (§4), reap, classify as `Completed` → loop continues to
  the next pick.
- **Child exits on its own before the state concludes** → the human stepped in
  and quit Claude mid-issue: classify as `UserQuit` → **break** the forever
  loop. This gives the user a natural "stop the worker" gesture (quit an
  unfinished session) without needing to also kill the SIGINT-ignoring
  supervisor.

(Optional refinement, not required for v1: to avoid visually truncating Claude's
final acknowledgement in the TUI after conclusion, either add a small grace
delay before signalling, or have the Stop hook touch a sentinel when it allows a
stop on a concluded session — that marks "Claude is now idle" precisely. The
durable artifact is already safe either way; this is cosmetic.)

### 4. Shutdown gesture + escalation ladder

A single `SIGINT` likely won't exit Claude — its TUI treats one Ctrl-C as
"interrupt / press again to exit". So the supervisor mimics the real exit
gesture: send `SIGINT`, brief pause (~250–500 ms), send `SIGINT` again, then
`wait()` with a timeout. Escalate if it doesn't exit: after ~5 s send `SIGTERM`,
after a further ~5 s `SIGKILL`, so a wedged session can never hang the
supervisor forever. **The exact gesture is the main thing to verify
experimentally** during implementation (double-SIGINT vs other); the escalation
ladder is the safety net regardless. Use `libc::kill(child_pid, sig)` targeting
the child pid specifically (not the group).

### 5. `next --forever` wiring (`src/main.rs`, `src/next.rs`)

Add `#[arg(long)] forever: bool` to the `Next` command (conflicts with
`--timeout`, which is a one-shot give-up; `--forever` is the opposite). Dispatch:

- Without `--forever`: unchanged (`pick`/`wait_for_pick` → `work_on`, which now
  spawns-and-exits via single mode).
- With `--forever`: a new loop (e.g. `next::run_forever(no_branch)`):
  `wait_for_pick(None)` → resolve/prepare worktree and spawn in supervised mode
  → run the monitor loop (§3) → on `Completed` continue, on `UserQuit` break.
  Reuse the existing launcher prep (`launch::run`'s worktree/resume/permission
  logic) by factoring the "prepare then spawn" part so both single and forever
  paths share it.

`--forever` implies the `--wait` "block until an eligible issue appears"
behaviour on each iteration, so an empty queue parks the worker rather than
exiting.

## Files to touch

- `src/launch.rs` — `exec_claude` → `spawn_claude` (+ single/supervised modes);
  factor the prepare-worktree-then-spawn path so `next --forever` can reuse it.
- `src/main.rs` — add `--forever` to `Next`; dispatch to the forever loop; drop
  reliance on the exec path.
- `src/next.rs` — `run_forever` loop (pick → spawn → monitor → classify).
- `src/state.rs` — `is_concluded()` helper shared with `stop_hook`.
- `src/stop_hook.rs` — use `is_concluded()` (optional sentinel refinement only
  if we do §3's cosmetic nicety).
- `Cargo.toml` — add `libc`.
- `README.md` — document `ghwf next --forever` and the supervisor model (the
  "pool of single-use workers" note becomes "single-use or, with `--forever`,
  self-renewing").
- No `Config`/`ghwf.toml` key, so the `init.rs`/README-config checklist in
  `CLAUDE.md` does not apply.

## Testing

- Unit tests: `is_concluded()` predicate; the `Completed` vs `UserQuit`
  classification logic (factor it to take an injected "child exited?" / "state
  concluded?" so it's testable without real processes); arg parsing
  (`--forever` conflicts with `--timeout`). Mirror the existing `stop_hook`/
  `next` test style.
- Manual/experimental (can't be unit-tested): the actual shutdown gesture
  against a real Claude session — confirm double-SIGINT exits cleanly and the
  terminal is restored; confirm a stray Ctrl-C during a normal single session
  doesn't kill the supervisor; confirm a full `next --forever` cycle (work an
  issue to merge, watch it shut down and pick the next) and that quitting an
  unfinished session stops the loop. Capture findings in the PR.

## Risks / open questions

- **Exact exit gesture** — double-SIGINT is the leading candidate but unverified;
  the escalation ladder de-risks it.
- **Terminal restoration** — graceful exit (Claude handling the gesture itself)
  should restore the terminal; SIGTERM/SIGKILL escalation may not. Prefer the
  gentlest gesture that works.
- **Simple ignore-SIGINT vs full job-control** — start simple; escalate to
  process-group/`tcsetpgrp` only if needed.
- **Cosmetic truncation** of Claude's post-conclusion acknowledgement — durable
  artifacts are safe (posted before state concludes); the grace-delay/sentinel
  refinement is optional.
