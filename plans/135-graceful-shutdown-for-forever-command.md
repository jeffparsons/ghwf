# Plan: Graceful shutdown for `forever` command (#135)

## Goal

Give a running `ghwf forever` worker a way to be told "wind down": finish the
in-flight issue normally (Claude is never interrupted), then exit cleanly
instead of picking the next one. A worker parked waiting for a pick should
notice the request and exit promptly too.

The signal is delivered by a new `ghwf stop` command (chosen by the user over a
process signal or a terminal gesture). It writes a small stop-request flag under
ghwf's data dir that the `forever` loop reads between issues and while parked.

## Design (confirmed in pre-plan)

- **Mechanism:** `ghwf stop` writes a stop-request flag file under
  `store::data_dir()`. `run_forever` honours it.
- **Scope:** stops *all* `forever` workers sharing this machine's ghwf data —
  the flag is global; no per-worker targeting.
- **Restart-safe, no deletion races (timestamp gate):** the flag records the
  request time. Each `forever` worker records its own start time and honours the
  flag only when `request_time > worker_start_time`. Consequences:
  - `ghwf stop` stops every worker already running (each started before the
    request).
  - A worker started *after* a stop ignores the now-stale flag, so the file can
    persist harmlessly — no worker deletes it, which avoids a delete-vs-read
    race that would otherwise let only one of several workers see the request.
  - A later `ghwf stop` simply rewrites a newer timestamp, which the new worker
    then honours.
- **Resolution:** store the timestamp in **milliseconds**, not seconds. Strict
  `>` comparison means a freshly started worker never spuriously exits on an
  equal-second stale flag, while a stop issued even milliseconds after a worker
  starts still reaches it. (Second granularity + `>=` would force an awkward
  trade-off; millis + `>` sidesteps it.)
- **Acknowledgement:** the worker prints clear lines — when it notices the
  request mid-loop ("stop requested; exiting — no further issues will be
  picked") and, if a request lands while an issue is in flight, the existing
  "concluded; looking for the next one" is replaced by a stop notice. `ghwf
  stop` itself prints a confirmation.
- **Out of scope:** `ghwf next` / `ghwf work-on` are one-shot and already stop
  on their own; only the `forever` loop consults the flag. No new `ghwf.toml`
  config field, so the "Adding a config option" checklist in CLAUDE.md does not
  apply.

## Implementation

### 1. Stop-flag helpers (`src/state.rs`)

Add near `now_epoch` (which already lives here, alongside the lease/claim file
helpers):

- `pub fn now_epoch_millis() -> u64` — `SystemTime::now().duration_since(UNIX_EPOCH)`
  as millis, `unwrap_or(0)` like `now_epoch`.
- A private `stop_flag_path() -> Result<PathBuf>` returning
  `store::data_dir()?.join("forever-stop")`.
- `pub fn request_stop() -> Result<()>` — write `now_epoch_millis()` (as a
  decimal string) to the flag path.
- `pub fn stop_requested_since(worker_start_millis: u64) -> Result<bool>` — read
  the flag; return `true` iff it parses to a value `> worker_start_millis`. An
  absent or unparseable flag reads as `false` (no stop).

For testability (matching the existing pattern where `find_issue_for_dir` /
`acquire_lease_at` take their root/time as parameters), factor the file logic
into dir-parameterised cores the public fns delegate to:

- `fn write_stop_flag_in(dir: &Path, at_millis: u64) -> Result<()>`
- `fn stop_flag_at(dir: &Path) -> Option<u64>` (parsed timestamp, or `None`)

so unit tests can drive a `scratch(...)` dir without touching the real
`data_dir()`.

### 2. Make the wait-for-pick loop abortable (`src/next.rs`)

`wait_for_pick_excluding` currently returns `Result<u64>` and is called by two
paths: `wait_for_pick` (public, used by `ghwf next --wait`) and `run_forever`.
Thread an abort check through it so only the `forever` path is affected:

- Add an `abort: impl Fn() -> bool` parameter, consulted at the top of each poll
  iteration (cheap: a `stat` + small read). Introduce a small return enum:

  ```rust
  enum WaitOutcome { Picked(u64), Aborted }
  ```

  Change the signature to `Result<WaitOutcome>`. When `abort()` is true at the
  top of an iteration, return `Ok(WaitOutcome::Aborted)` instead of continuing
  to poll.
- `wait_for_pick(timeout)` (and thus `ghwf next --wait`) passes `|| false` and
  maps `Picked(n) => Ok(n)`; `Aborted` is unreachable for it (`unreachable!()`
  with an explaining message) — behaviour is identical to today.

### 3. Teach `run_forever` to stop (`src/next.rs`)

- At the top of `run_forever`, capture `let worker_start = state::now_epoch_millis();`.
- Define a closure `let stop = || state::stop_requested_since(worker_start).unwrap_or(false);`
  (an unreadable flag is treated as "no stop" — we never wedge the worker on a
  flag I/O hiccup).
- Pass `stop` as the abort check to the wait call. On `WaitOutcome::Aborted`,
  print the acknowledgement and `return Ok(())`.
- After a `launch::Outcome::Completed`, check `stop()` before looping: if set,
  print "Issue #N concluded; stop was requested, so the forever worker is
  exiting." and `return Ok(())`; otherwise keep the existing "looking for the
  next one" message and continue.
- Also check `stop()` once at the very top of each loop iteration (before
  waiting) so a request that arrives between a completed issue and the next wait
  is caught immediately.

The `UserQuit` path is unchanged.

### 4. New `ghwf stop` command (`src/main.rs`)

- Add a `Stop` variant to the `Commands` enum with a `///` doc comment
  describing the graceful-shutdown semantics (forever workers finish their
  current issue, then exit; safe to run when nothing is running). No arguments.
- Dispatch it in the `match cli.command` block to a small handler that calls
  `state::request_stop()?` and prints a confirmation, e.g.:
  "Stop requested. Any running `ghwf forever` worker will finish its current
  issue and then exit."
- Running `ghwf stop` with no worker active just writes the flag and prints the
  same line (harmless; documented).

## Tests

- **`src/state.rs` unit tests** (using the `scratch(tag)` helper):
  - Writing a flag then reading it back via the dir-parameterised cores:
    `stop_flag_at` returns the written timestamp; absent file → `None`;
    garbage contents → `None`.
  - Gate logic: a flag at time `T` is honoured for a worker started at `T - 1`
    (`> ` true) and ignored for one started at `T` and at `T + 1` (stale).
- **`src/main.rs` CLI parse tests** (alongside the existing `forever_*` tests):
  - `ghwf stop` parses with no args; `ghwf stop --anything` is rejected.
- Run `cargo fmt`, `cargo clippy`, and `cargo test` before hand-off.

## Docs

- README: in the section covering `ghwf forever`, add a short note that `ghwf
  stop` requests a graceful shutdown — the worker finishes its current issue and
  exits, and the request reaches every forever worker on the machine.
- The command's `///` doc comment is the source `--help` surfaces; keep it
  accurate and self-contained.

## Edge cases & notes

- **Multiple workers:** all started-before workers honour one `ghwf stop`
  because none deletes the flag; the timestamp gate distinguishes "already
  running" from "started later".
- **Stale flag across days:** harmless — a worker only ever exits if the flag's
  timestamp is newer than its own start, so an ancient flag is ignored.
- **Flag I/O failure:** treated as "no stop"; the worker keeps running rather
  than exiting on a read error.
- **No effect on `next`/`work-on`:** only `run_forever` consults the flag.
