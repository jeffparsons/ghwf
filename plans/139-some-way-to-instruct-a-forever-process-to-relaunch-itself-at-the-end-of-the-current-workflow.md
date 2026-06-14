# Plan — #139: Relaunch a `forever` worker when its own binary changes

## Goal

Let a long-running `ghwf forever` worker pick up a freshly-built `ghwf` binary
**automatically**, without anyone having to stop and restart it by hand.

The worker hashes its own executable once at startup. At the end of each
workflow — between issues, never mid-workflow — it re-hashes the on-disk binary.
If the hash has changed, it re-`exec`s itself in place (same PID, same terminal,
same arguments) so the new build continues the loop. If unchanged, it carries on
to pick the next issue exactly as today.

Settled with the issue owner (#139): **automatic on-change detection only** — no
explicit `ghwf restart` command, and no unconditional "always relaunch".

## Background — how things work today

- `run_forever(no_branch)` (`src/next.rs:204-266`) is the supervisor loop. It
  resolves the repo once, records `started = state::now_epoch_millis()`, and
  defines `stop_requested = || state::stop_requested_since(started)`. Then it
  loops: `wait_for_pick_excluding` → `launch::prepare` → `launch::supervise_once`.
- `supervise_once` returns `launch::Outcome::Completed` (workflow concluded) or
  `Outcome::UserQuit` (session ended early). In the `Completed` arm
  (`src/next.rs:244-256`) the worker checks `stop_requested()` — if set, it
  prints a message and `return Ok(())` (exit); otherwise it prints "looking for
  the next one" and falls through to the next loop iteration. **This is the exact
  relaunch decision point.** The `UserQuit` arm already exits and needs no
  change.
- The stop mechanism (`src/state.rs`, `request_stop` / `stop_requested_since`)
  is a timestamp flag under the data dir, and it degrades to "no stop" on any
  I/O error so a hiccup never wedges the loop. We mirror that
  fail-safe-and-continue posture for the hash check.
- Hashing: `sha2` is already a dependency; `src/store.rs` uses
  `Sha256` (e.g. `content_hash(s: &str)` at `src/store.rs:87`). We need to hash a
  file's bytes, so we add a small bytes/file helper rather than reuse the
  `&str` one.
- Re-exec: `std::os::unix::process::CommandExt::exec()` replaces the current
  process image. The codebase already imports `CommandExt` and uses `pre_exec`
  in `src/launch.rs:611-619`, and already gates its Unix-only job-control /
  shutdown behaviour behind `#[cfg(unix)]` (`Cargo.toml:22-24` makes `libc`
  Unix-only). We follow the same Unix-gating convention.
- `std::env::current_exe()` returns the path to the running binary; reading and
  hashing that path's contents at startup, then again later, detects an
  in-place rebuild (the common `cargo install` / `cp` case, where the path is
  stable but the bytes change).

## Design decisions

1. **What is hashed.** The bytes of `std::env::current_exe()`. We use the
   resolved executable path (not `argv[0]`, which may be relative or a bare
   `ghwf` relying on `$PATH`) both for hashing and as the program to re-exec, so
   the thing we re-launch is exactly the thing we compared.

2. **When the check runs.** Only in the `Outcome::Completed` arm, **after** the
   existing `stop_requested()` check. Precedence: a pending stop wins (exit); a
   changed binary triggers relaunch; otherwise continue. Relaunch therefore only
   ever happens at a clean between-workflows boundary, never mid-workflow, and
   never overrides a requested shutdown.

3. **How it relaunches.** Re-exec in place via `CommandExt::exec()` with program
   = `current_exe()` and args = `std::env::args_os().skip(1)` (the original
   arguments, so `--no-branch` and anything else are preserved). `exec()` only
   returns on failure; on success the process image is replaced and the new
   build resumes the loop with a fresh `started` timestamp.

4. **Fail-safe everywhere.** The feature must never crash or wedge a worker that
   is otherwise healthy:
   - If `current_exe()` fails, or the file can't be read/hashed at **startup**,
     we record "no baseline" and simply never auto-relaunch this run (log a
     one-line `warning:` so it's visible).
   - If the re-hash at decision time fails, treat it as "unchanged" and continue
     (best-effort, like the stop-flag read).
   - If `exec()` returns (i.e. it failed), log a `warning:` and fall through to
     continue the loop on the current binary rather than dying.

5. **Platform.** Unix uses `exec()` (process replacement). On non-Unix we keep
   today's behaviour — no auto-relaunch — gated with `#[cfg(unix)]` /
   `#[cfg(not(unix))]`, consistent with the existing Unix-only forever/shutdown
   handling. (A non-Unix spawn-and-exit fallback is possible but out of scope;
   `forever`'s graceful-stop gesture is already documented Unix-only.)

6. **No new config, no new command.** Behaviour is unconditional and built into
   `forever`. There is nothing to opt into and nothing to remember to run, which
   is the whole point of the chosen approach. (So none of the "adding a config
   option" steps in `CLAUDE.md` apply.)

## Implementation

All changes are in `src/next.rs` (plus tests).

1. **Self-hash helper.** Add a small private helper, e.g.

   ```rust
   /// SHA-256 of this executable's bytes, or `None` if the path can't be
   /// resolved or read. Used to notice an in-place rebuild between workflows.
   fn self_exe_hash() -> Option<String>
   ```

   It calls `std::env::current_exe()`, reads the file, feeds the bytes to
   `Sha256`, and returns the hex digest. (Either add a `hash_bytes(&[u8])` next
   to `store::content_hash`, or compute inline — a coin-flip; inline keeps the
   helper self-contained.)

2. **Capture the baseline.** In `run_forever`, near `started`, compute
   `let baseline_hash = self_exe_hash();` and `warning:` once if it's `None`.

3. **Relaunch helper (Unix).**

   ```rust
   /// Re-exec the current binary in place with the original arguments, so a
   /// freshly-built `ghwf forever` continues the loop. Returns only on failure.
   #[cfg(unix)]
   fn relaunch_self() -> std::io::Error
   ```

   Builds `Command::new(current_exe()?)`, `.args(env::args_os().skip(1))`,
   `.exec()`. On non-Unix, a `#[cfg(not(unix))]` stub is unnecessary because the
   call site is itself `#[cfg(unix)]`-gated (see step 4).

4. **Decision at the relaunch point.** In the `Outcome::Completed` arm, after the
   existing `if stop_requested() { … return … }` block and before "looking for
   the next one":

   ```rust
   #[cfg(unix)]
   if let Some(baseline) = &baseline_hash {
       if self_exe_hash().as_ref().is_some_and(|h| h != baseline) {
           println!(
               "Issue #{number} concluded; the ghwf binary changed, \
                relaunching to pick up the new build."
           );
           let err = relaunch_self(); // only returns on failure
           eprintln!("warning: failed to relaunch ghwf, continuing on the \
                      current binary: {err}");
       }
   }
   ```

   Then the existing `println!("Issue #{number} concluded; looking for the next
   one.")` and loop continuation stay as the fallthrough.

## Testing

- **Unit-test the hash helper.** `self_exe_hash()` returns `Some` and is stable
  across repeated calls within a run (hashing the test binary itself). This
  guards the "unchanged → no relaunch" path.
- **Refactor for testability of the decision.** Extract the pure decision —
  "given baseline `Option<String>` and current `Option<String>`, should we
  relaunch?" — into a tiny function (e.g. `should_relaunch(baseline, current)
  -> bool`) and unit-test its truth table: `None` baseline → never; equal →
  never; differing `Some`/`Some` → yes; current `None` (re-hash failed) →
  never. This covers the precedence/fail-safe logic without spawning processes.
- **Manual smoke test** (documented in the PR, not automated — exec replacement
  is awkward to assert in a unit test): run `ghwf forever`, let one issue
  conclude, `cargo build` a trivially-changed binary into the same path between
  workflows, and confirm the worker logs the relaunch and comes back up on the
  new build; confirm an unchanged binary just continues; confirm `ghwf stop`
  still wins over a pending relaunch.
- Run `cargo test` and `cargo clippy` before handing off.

## Out of scope

- An explicit `ghwf restart` command (considered and declined for #139).
- Unconditional "always relaunch between workflows" (declined).
- Non-Unix process replacement.
