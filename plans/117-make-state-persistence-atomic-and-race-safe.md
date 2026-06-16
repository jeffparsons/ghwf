# Plan: Make state persistence atomic and race-safe (#117)

## Goal

Two distinct bugs in local state persistence, both stemming from non-atomic
writes and unsynchronised read-modify-write:

1. **Torn file → permanent strand.** `state::save` / `seen::save` use a plain
   `fs::write` (`state.rs:695`, `seen.rs:63`). A crash mid-write leaves a
   truncated `<n>.json`; `issue_status` then reads the parse failure as `Live`
   (`next.rs:379`), so no `next`/`forever` worker ever picks the issue up again.
2. **Lost update.** Several writers load the *whole* `IssueState`, change one
   field, and write it all back. One firing around a concurrent `work-on` save
   can write its stale snapshot back over `work-on`'s phase advance and
   `consumed_directives` / `consumed_reactions` — and a clobbered consumed-set
   lets an already-fired approval re-fire.

The narrow read-modify-write writers in scope:

- Stop hook → `stop_nudges` (`stop_hook.rs:53-54`)
- Notification hook → `session_alert` (`notification_hook.rs:52-58`)
- supervisor `clear_alert` → `session_alert` (`launch.rs:802-808`)

## Approach

Approved direction (pre-plan 👍): **atomic writes everywhere + a per-issue
advisory lock that serialises the narrow writers against full saves**. Keep
`IssueState` and all its readers intact (no sidecar files).

### 1. Atomic writes

Add a generic helper in `store.rs`:

```rust
/// Atomically replace `path`'s contents: write a sibling temp file, fsync-free
/// rename into place, so a concurrent reader (or a crash) never sees a
/// half-written file. The temp name carries our pid so concurrent writers in
/// different processes don't collide.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()>
```

This is the temp+rename pattern already in `state::write_lease`
(`state.rs:1011-1018`), lifted out. Route through it:

- `state::save` (`state.rs:695`) — under the lock (see below).
- `seen::save` (`seen.rs:63`) — directly; seen records are written only by
  `work-on` (single writer), so they need atomicity but not the lock.
- `state::write_lease` (`state.rs:1011`) — reuse the shared helper so the
  pattern lives in one place.

The temp file must be a sibling of the target (same directory ⇒ same
filesystem ⇒ atomic `rename`). Reuse the existing `path.with_extension(format!
("{}.tmp", std::process::id()))` convention.

### 2. Per-issue lock + `mutate`

Serialisation point: a dedicated lock file `<n>.lock` next to `<n>.json` under
the issues dir, locked with `flock(2)` (`LOCK_EX`). We lock a *separate* file,
not the state file itself, because the state file's inode is replaced on every
atomic rename — an `flock` on the old inode wouldn't serialise against a writer
that renamed a new inode into place.

`flock` is available via the existing `libc` dependency, which is already
`[target.'cfg(unix)']`-only. On non-Unix the lock degrades to a no-op guard
(atomic writes still prevent torn files; only the RMW serialisation is lost),
keeping the crate buildable everywhere — consistent with the supervisor's
existing `cfg(unix)` posture.

New in `state.rs`:

```rust
/// Path to the per-issue lock file: `<issues>/<owner>/<repo>/<n>.lock`.
fn lock_path(owner, repo, number) -> Result<PathBuf>

/// Open (creating) the issue's lock file and hold an exclusive flock for the
/// closure's duration. The guard releases on drop. cfg(unix); a no-op guard
/// elsewhere.
fn with_issue_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T>

/// Read-modify-write an issue's recorded state under the lock: re-read the
/// latest file inside the lock, apply `f`, write atomically. A narrow writer
/// (a hook, the supervisor) uses this so it overwrites only the field it owns
/// and never the concurrent writer's phase/consumed-sets. Does nothing (no
/// file is created) when no state exists yet.
pub fn mutate(owner, repo, number, f: impl FnOnce(&mut IssueState)) -> Result<()>
```

Why this is correct: `save()` holds the lock only across its `[write tmp;
rename]`; `mutate()` holds it across `[read; apply; write; rename]`. Same lock
file ⇒ the two critical sections can't interleave. So a `mutate` runs either
*fully before* a concurrent `save`'s write (its delta is lost — but that delta
is only the benign nudge/alert field) or *fully after* it (it re-reads the
fresh snapshot and preserves the phase advance + consumed-sets). `work-on`'s
phase/consumed-sets — which it alone writes — are never lost.

### 3. Convert the narrow writers to `mutate`

- **Stop hook** (`stop_hook.rs:47-56`): keep the `should_block` decision on the
  bound read, but perform the increment via
  `state::mutate(.., |s| s.stop_nudges += 1)`. On `mutate` error, fail open
  (return `Ok(())` without nudging), matching today's behaviour.
- **Notification hook** (`notification_hook.rs:46-58`): compute `episode_start`
  from the bound read (its inputs — kind/session_id — don't race), then
  `state::mutate(.., |s| s.session_alert = Some(SessionAlert { .. }))`.
  Best-effort: ignore a `mutate` error as today.
- **`clear_alert`** (`launch.rs:802-808`): replace the load/modify/save with
  `state::mutate(.., |s| s.session_alert = None)`.

## Out of scope (follow-up filed)

- **`post_and_flag`** (`launch.rs:856-874`) also does a whole-struct
  read-modify-save (sets `attention` + runs `labels::sync`). It's *not*
  converted here: `labels::sync` makes network calls, and holding a file lock
  across network I/O is wrong. Its clobber risk is limited to the
  attention/label axis (self-healing on the next `work-on` run), not the
  consumed-set re-fire this issue targets. A follow-up issue tracks tightening
  it (do the network/label work first, then apply the narrow state delta under
  the lock).
- **`issue_status` "unparseable ⇒ Live"** (`next.rs:379`) is left as-is.
  Per the pre-plan 👍, atomic writes prevent the torn files that cause the
  strand in the first place; hardening the read path is not pulled in.

## Tests

- `store::atomic_write`: writing over an existing file replaces it; no `*.tmp`
  sibling is left behind on success.
- `state::mutate`: applies the closure and persists; a concurrent full `save`
  of an unrelated field (simulated by writing between a staged read and the
  mutate) does not lose the phase/consumed-set — i.e. `mutate` re-reads rather
  than using a stale snapshot. Drive it against a concrete scratch path, mirror
  the existing `acquire_lease_at`/`stop_flag_at` split so tests don't need a
  real data dir (add a `mutate_at` / `with_issue_lock_at` test seam as needed).
- `state::mutate` on a missing state file is a no-op (creates nothing).
- Existing `seen` round-trip / parse tests still pass after the atomic-write
  switch.
- Stop hook: existing `should_block` tests are unaffected (decision logic
  unchanged); the increment path is covered via the `mutate` test.

## Files touched

- `src/store.rs` — `atomic_write` helper.
- `src/state.rs` — `lock_path`, `with_issue_lock`, `mutate`; `save` and
  `write_lease` routed through the lock + `atomic_write`.
- `src/seen.rs` — `save` routed through `atomic_write`.
- `src/stop_hook.rs` — increment via `mutate`.
- `src/notification_hook.rs` — alert write via `mutate`.
- `src/launch.rs` — `clear_alert` via `mutate`.
