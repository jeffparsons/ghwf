# Plan — #99: Periodic garbage collection

## Goal

Let a repo opt in to having ghwf run garbage collection **automatically**, so
merged branches and their worktrees get cleaned up without anyone running
`ghwf collect-garbage` by hand.

When opted in, after `work-on` first detects that a ticket's workflow has
**finished** (its PR merged), ghwf runs the existing `collect_garbage::run`,
**at most once per configurable interval** (default once per day). The feature
is **off by default** — we never delete people's stuff without them asking.

Manual `ghwf collect-garbage` is completely unaffected: always available, never
throttled.

## Background — how things work today

- `collect_garbage::run(dry_run)` (`src/collect_garbage.rs`) is the manual
  command (`ghwf collect-garbage`, alias `gc`, dispatched in `src/main.rs`). It
  fetches, examines every non-default branch, and for each one with a merged PR
  deletes the branch / worktree / ghwf state, with strong safety rails:
  merged-only, exact-tip match against what GitHub merged, the merge must have
  landed on the default branch, never the main worktree or the worktree the
  command runs inside, never a dirty/untracked worktree, never a force-delete.
  It resolves its repo via `github::repo_or_cwd()` (= the configured `main_repo`)
  and its main-repo path via `config::find()`.
- `work_on` (`src/main.rs`) already has a **once-per-merge** hook: when a PR
  merge is first observed, `new_conclusion == Some(state::PrOutcome::Merged)` is
  true, and inside that block it calls `update_main_worktree_after_merge`
  (~lines 720–729). This is the exact "the workflow just closed out" moment. The
  config is already loaded once into `located` just above (`config::find()?`).
- Config lives in `src/config.rs` (`Config`, parsed by serde, reflected by
  facet). Per `CLAUDE.md`'s "Adding a config option" note, a new field needs:
  a `///` doc comment, a `config init` wizard entry (`src/init.rs`), a README
  annotated-example entry, and a `ghwf config example` entry (`src/config_schema.rs`)
  — whose `example_covers_every_field` destructure won't compile, and whose
  `example_*` tests fail, until the field is handled.
- `store::data_dir()` (`src/store.rs`) is ghwf's per-user data dir; per-repo
  state already nests under `data_dir/issues/<owner>/<repo>/`. `state::now_epoch()`
  returns epoch seconds.

## Design decisions (settled with the issue owner)

1. **Config shape:** two flat fields (matching `delete_plan_on_approval` etc.):
   - `auto_collect_garbage` — bool, **default `false`** (the on/off switch).
   - `auto_collect_garbage_interval_hours` — integer, **default `24`** (once per
     day). Hours, so the cadence is tunable (12 = twice daily, 168 = weekly)
     without adding a duration-parsing dependency — the codebase already works
     in epoch seconds.
2. **Trigger:** fires only on a PR **merge** (the existing once-per-merge
   `new_conclusion == Some(Merged)` block), not on closed-without-merge — GC
   only ever acts on merged branches anyway.
3. **Throttle:** a per-repo last-run timestamp under the data dir. GC runs only
   when at least the configured interval has elapsed; the timestamp is then
   stamped (regardless of GC's outcome) so a persistently-failing GC can't fire
   on every merge.
4. **Best-effort:** any error from the automatic GC is reported as a `warning:`
   on stderr and never fails the `work-on` run.
5. **Scope:** the throttle is keyed on the **code repo** (`code_owner`/`code_repo`
   in `work_on`), which is what `collect_garbage::run` operates on (both resolve
   to the configured `main_repo`).

## Implementation

### 1. Config keys (`src/config.rs`)

Add to `Config`:

```rust
/// When true, ghwf automatically runs garbage collection (the same work as
/// `ghwf collect-garbage`) after a ticket's workflow finishes — i.e. when its
/// PR is observed merged — at most once per `auto_collect_garbage_interval_hours`.
/// Off by default: opt in to let ghwf delete merged branches and their clean
/// worktrees on your behalf. The manual `ghwf collect-garbage` command is
/// unaffected by this setting (always available, never throttled).
#[serde(default)]
pub auto_collect_garbage: bool,
/// Minimum hours between automatic garbage collections (see
/// `auto_collect_garbage`). Defaults to 24 (once per day); e.g. 12 for twice a
/// day, 168 for weekly. Ignored when `auto_collect_garbage` is off.
#[serde(default = "default_auto_collect_garbage_interval_hours")]
pub auto_collect_garbage_interval_hours: u64,
```

with a module-level `fn default_auto_collect_garbage_interval_hours() -> u64 { 24 }`.

Tests (in `config.rs` `tests`):
- `auto_collect_garbage_parses` — `auto_collect_garbage = true` parses to `true`.
- `auto_collect_garbage_interval_hours_parses` — a custom value parses.
- defaults — extend the "pre-existing configs keep loading" assertions: absent
  keys give `false` and `24`.

### 2. Throttle + periodic entry point (`src/collect_garbage.rs`)

Add a per-repo last-run timestamp and the throttled runner. Keep `run` as-is and
call it from the new function so the manual path is untouched.

```rust
/// Path to the per-repo timestamp recording the last automatic GC run.
fn last_run_path(owner: &str, repo: &str) -> Result<PathBuf> {
    Ok(store::data_dir()?
        .join("gc")
        .join(owner)
        .join(repo)
        .join("last-run"))
}

/// The epoch-seconds timestamp of the last automatic GC for this repo, or
/// `None` when there is no record (or it's unreadable/garbled — treated as
/// "never run").
fn read_last_run(owner: &str, repo: &str) -> Option<u64> { … }

/// Record `now` as the last automatic-GC time for this repo.
fn stamp_last_run(owner: &str, repo: &str, now: u64) -> Result<()> {
    let path = last_run_path(owner, repo)?;
    // create_dir_all the parent, then write the integer as text.
    …
}

/// Run garbage collection automatically when this repo has opted in
/// (`auto_collect_garbage`) and at least the configured interval has elapsed
/// since the last automatic run. Best-effort: failures are warned about, never
/// propagated. The repo args key the throttle and should be the code repo GC
/// acts on.
pub fn run_periodic(config: &config::Config, owner: &str, repo: &str) {
    if !config.auto_collect_garbage {
        return;
    }
    let interval_secs = config
        .auto_collect_garbage_interval_hours
        .saturating_mul(3600);
    let now = state::now_epoch();
    if let Some(last) = read_last_run(owner, repo) {
        if now.saturating_sub(last) < interval_secs {
            return; // not due yet
        }
    }
    println!("Running periodic garbage collection…");
    if let Err(err) = run(false) {
        eprintln!("warning: periodic garbage collection failed: {err:#}");
    }
    // Stamp regardless of outcome, so a failing GC doesn't re-fire every merge.
    if let Err(err) = stamp_last_run(owner, repo, now) {
        eprintln!("warning: failed to record periodic GC timestamp: {err:#}");
    }
}
```

Notes:
- `run_periodic` takes `&config::Config` (not `Located`) so it's trivial to call
  with the already-loaded config and easy to unit-test. It re-uses `run(false)`,
  which independently resolves the same code repo via `github::repo_or_cwd()`.
- An `interval_hours` of `0` makes `interval_secs == 0`, so it effectively runs
  on every merge (no throttle) — a reasonable interpretation of "no minimum
  gap" for anyone who sets it.

Tests (in `collect_garbage.rs` `tests`, redirecting the data dir):
- The data dir comes from `store::data_dir()`, which uses the `directories`
  crate (HOME-derived). The existing `state`/`store` tests don't override HOME
  (and `CLAUDE.md` forbids it), so rather than fight the global data dir, **test
  the pure due/not-due decision** by extracting it:

  ```rust
  /// Whether an automatic GC is due given the last run, now, and interval.
  fn is_due(last_run: Option<u64>, now: u64, interval_secs: u64) -> bool {
      match last_run {
          None => true,
          Some(last) => now.saturating_sub(last) >= interval_secs,
      }
  }
  ```

  `run_periodic` uses `is_due`. Unit-test `is_due`: never-run → due; just-run →
  not due; exactly at the interval → due; `interval_secs == 0` → always due;
  `now < last` (clock skew) → not due (saturating_sub → 0 < interval).

### 3. Wire into `work_on` (`src/main.rs`)

In the existing merge-detection block (where `update_main_worktree_after_merge`
is called under `new_conclusion == Some(state::PrOutcome::Merged)` with
`located` in scope), add right after it:

```rust
if new_conclusion == Some(state::PrOutcome::Merged) {
    if let Some(located) = located.as_ref() {
        update_main_worktree_after_merge(located, &code_owner, &code_repo);
        collect_garbage::run_periodic(&located.config, &code_owner, &code_repo);
    }
}
```

(Only reachable with a config present, which GC requires anyway.) This keeps the
trigger to exactly once per merge and leaves all other phase handling untouched.

### 4. `config init` wizard (`src/init.rs`)

Following the `delete_plan_on_approval` / `blocked_label` patterns, offered only
when the key is absent:

- A `Confirm` (default `false`): "Automatically run garbage collection after a
  ticket's PR merges? (deletes merged branches and their clean worktrees)".
- When accepted: `set_auto_collect_garbage(&mut doc)` (writes
  `auto_collect_garbage = true` with an explanatory comment), then a follow-up
  `Text` prompt defaulting to `"24"` for the interval; parse to `u64` and, when
  it differs from the default / is provided, `set_auto_collect_garbage_interval_hours`.
  Mirror `blocked_label`'s "empty/parse-fail → keep the default" handling (don't
  write the interval key when the user accepts the 24h default, to keep configs
  tidy — the serde default covers it).
- `set_*` helpers use `insert_with_comment`, mirroring the existing setters.

Round-trip test like `permission_mode_round_trips` / the other setter tests.

### 5. `ghwf config example` (`src/config_schema.rs`)

- Add `auto_collect_garbage` and `auto_collect_garbage_interval_hours` to the
  `example_covers_every_field` destructure of `Config` (compile guard).
- Emit both in `render_example` with `insert(...)`:
  ```rust
  insert(&mut doc, "auto_collect_garbage", toml_edit::value(true));
  insert(&mut doc, "auto_collect_garbage_interval_hours", toml_edit::value(24));
  ```
  Place them next to `delete_plan_on_approval` among the scalar keys (before the
  `[labels]` table). The comments come from the struct doc via reflection. The
  existing `example_*` tests then cover them automatically.

### 6. README (`README.md`)

Add an annotated entry to the `ghwf.toml` example block in "## Configuration",
near `delete_plan_on_approval`:

```toml
# When true, ghwf automatically runs garbage collection (same as
# `ghwf collect-garbage`) after a ticket's PR merges, at most once per
# `auto_collect_garbage_interval_hours` (optional; default false). It deletes
# merged branches and their fully-clean worktrees, with the same safety rails as
# the manual command (never the main or current worktree, never a dirty one,
# never a force-delete). The manual command stays available and unthrottled.
auto_collect_garbage = true
# Minimum hours between automatic garbage collections (optional; default 24 —
# once per day). e.g. 12 for twice a day, 168 for weekly. Ignored when
# auto_collect_garbage is off.
auto_collect_garbage_interval_hours = 24
```

Optionally add a sentence to the "## Collecting garbage" section noting the
opt-in automation and pointing at the config keys.

## Edge cases / notes

- **At most once per interval, per repo, across sessions** — the timestamp is on
  disk, not in process memory, so concurrent/sequential sessions share it.
- **The just-merged ticket's own worktree is protected** — GC never removes the
  worktree the session runs inside or the main worktree, so the run that fires
  GC won't delete its own working directory; that branch is collected on a later
  run once the session has moved on.
- **Throttle stamped on attempt, not just success** — honours "at most once per
  interval" even when GC errors.
- **No config → never fires** (the trigger is inside `located.as_ref()`, and GC
  requires a config regardless).
- **`interval_hours = 0`** → no throttle (runs on every merge); documented
  behaviour, not a bug.

## Testing summary

- `src/config.rs`: parse + default tests for both new keys.
- `src/collect_garbage.rs`: unit tests for the `is_due` decision (never-run,
  not-due, exactly-due, zero-interval, clock-skew).
- `src/init.rs`: round-trip test for the new setter(s).
- `src/config_schema.rs`: existing `example_*` tests cover the new keys once
  emitted; the compile-time destructure guard enforces presence.
- `cargo build && cargo clippy && cargo test` clean; comments follow the repo's
  terminator conventions (full sentences terminated, fragments not).
