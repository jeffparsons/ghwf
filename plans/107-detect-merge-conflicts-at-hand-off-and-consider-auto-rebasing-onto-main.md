# Plan — #107: Detect merge conflicts at hand-off and surface/auto-merge when `main` moves

## Goal

Close the friction the #97 audit surfaced: `main` advances after a `work-on`
run starts (or while a PR idles in review) and the fresh conflict isn't noticed
until merge time, leaving the *user* to re-flag it. ghwf should be the one to
notice, at three new moments:

1. **At hand-off** — re-check before announcing a branch as ready-for-review.
2. **While the PR idles in review** — probe periodically in the `wait` loop and
   wake Claude when `main` moves under it and creates a conflict.
3. **Config-gated auto-merge** — when the branch is behind base but merges
   cleanly, optionally merge `origin/<base>` and push to keep the PR current.

Decisions locked in during pre-plan (approved by 👍):
- **Merge, not rebase.** This repo squash-merges, the existing `conflict_notice`
  already instructs `git merge origin/<base>`, and merging avoids force-pushing a
  published PR branch. Rebase is out of scope.
- **Auto-merge defaults OFF**, opt-in via config, mirroring `auto_collect_garbage`.
  Items 1 and 2 are always-on (read-only).

## Current behaviour (baseline)

- Conflict detection runs **only at the start** of an Implement/Review
  `work-on`: `main.rs:824-834` → `implement::detect_conflict` (`implement.rs:91`)
  → `try_detect_conflict` (`implement.rs:109`): `git::fetch` + `git::would_conflict`
  (in-memory `merge-tree`, `git.rs:268`), no GitHub API.
- On conflict it prefixes the banner with `render::conflict_notice`
  (`render.rs:325`) — a "resolve now" instruction to Claude, never posted to a
  thread, self-clearing once the merge is pushed.
- `hand_off` (`main.rs:1983`) and the `wait` loop (`wait.rs:31`) do no conflict
  check. A PR idling in review sits untouched until the user acts — GitHub does
  not notify a PR when its base advances.

## Design

### Shared detection: `implement::base_sync`

Generalise `try_detect_conflict` into one fetch-then-classify helper so all
three call sites share logic and the single `git fetch`.

```rust
/// How the branch stands relative to its freshly fetched base.
pub enum BaseSync {
    /// HEAD already contains origin/<base>; nothing to do.
    UpToDate,
    /// Base advanced beyond the merge-base and a trial merge is clean.
    BehindClean,
    /// Base advanced and a trial merge conflicts.
    Conflict,
}

/// Fetch, then classify HEAD against origin/<base>. `base` is returned so
/// callers can name it in messages.
fn base_sync(worktree: &Path) -> Result<(String, BaseSync)>;
```

Classification (all local, post-fetch):
- `git::is_ancestor(worktree, "origin/<base>", "HEAD")` true → `UpToDate`.
- else `git::would_conflict` true → `Conflict`.
- else → `BehindClean`.

`detect_conflict` (the existing banner entry point, used by `main.rs:824`) keeps
its signature `pub fn detect_conflict(prep) -> Option<String>` and becomes a thin
wrapper: `base_sync` → `Conflict` ⇒ `Some(base)`, else `None`. All its existing
precondition guards (no_branch / pr_number / worktree-is-dir) and its
never-break-`work-on` error swallowing stay exactly as they are. Existing tests
in `implement.rs` keep passing unchanged.

### New git helper

`git.rs` has `merge_ff_only` but not a real merge. Add:

```rust
/// Merge `target` (e.g. `origin/main`) into `dir`'s checked-out branch with a
/// merge commit. On failure, abort so the worktree is left clean.
pub fn merge(dir: &Path, target: &str) -> Result<()>;
```

Runs `git merge --no-edit <target>`; on error runs `git merge --abort`
(best-effort) before returning the error. Callers only invoke this after a
`BehindClean` classification, so a clean merge is expected; the abort is a
safety net. Add a unit test alongside `would_conflict_detects_conflicts_and_clean_merges`
(`git.rs:749`) covering a clean merge and a conflicting one (which aborts).

### Item 1 — re-check at hand-off

In `hand_off` (`main.rs:1983`), after resolving `issue_state`/`phase` and before
posting, add a guard for the ready-for-review hand-off:

- Only when `phase == Phase::Implement`, `!no_branch`, and `!question` (a blocking
  question is not an "I'm done" hand-off and must always get through).
- Call `implement::detect_conflict(prep)`. If it returns `Some(base)`, **bail**
  before posting anything:

  ```
  the branch for issue #<n> conflicts with `origin/<base>`; resolve it before
  handing off (in the worktree: `git merge origin/<base>`, resolve, commit, push),
  then hand off again.
  ```

This reuses the precondition-guarded `detect_conflict`, so for phases without a
worktree/PR it is a cheap no-op returning `None`. The fetch only happens on the
Implement ready hand-off. No partial side effects: the check runs before the
GitHub post.

If `auto_merge_base` is on (see item 3) and the status is `BehindClean`, the
hand-off path can also merge+push first so the announced branch is current — but
keep this secondary; the conflict block is the load-bearing part.

### Item 2 — proactive probe in the `wait` loop

`ghwf wait` is the only thing running while a PR idles in review. Add a
read-only, low-frequency local probe that wakes Claude when a conflict newly
appears.

State: add `conflict_seen: bool` (`#[serde(default)]`) to `WaitState`
(`state.rs:370`) so the clean→conflict edge is detected across the repeated
~9-minute `wait` invocations and we don't re-wake every cycle.

Wiring in `wait::run` (`wait.rs`):
- Compute eligibility once up front: `issue_state.phase == Phase::Review`,
  `prep` present with a `worktree_path` that is a dir, and `!no_branch`. Capture
  the worktree path. (Probing only in Review keeps it to the genuine idle-PR
  window; Implement is covered by `work-on`/hand-off.)
- Probe cadence: piggy-back on the existing direct **sweep** (every
  `FEED_SWEEP_INTERVAL` = 300 s in feed mode) and on Direct-mode cycles once
  backoff reaches the cap, so it's at most ~once/5 min — never on the fast hot
  path. A dedicated `last_probe: Instant` guard inside `run` enforces the floor
  regardless of mode.
- Each probe: `implement::detect_conflict`-style check via a small wait-local
  helper that calls `base_sync` (fetch + classify). Best-effort: on any git
  error, `eprintln!` a warning and skip — never break the wait. Fetch is safe
  (updates remote refs only; never touches the working tree) and `merge-tree` is
  read-only, so probing a worktree in use is fine.
- Edge logic: `Conflict` && `!conflict_seen` → set `conflict_seen = true`, push a
  wake reason `"`origin/<base>` moved and PR #<n> now conflicts with it."` and
  return (the normal reason path persists `wait_state` and exits 0).
  Any non-`Conflict` result → reset `conflict_seen = false` so a later re-conflict
  wakes again. Persist `conflict_seen` via the existing `persist` path.
- `wait` stays read-only: no auto-merge here (that belongs to `work-on`/hand-off
  where Claude is active). When woken, Claude runs `work-on` (Review phase),
  which shows the existing `conflict_notice` and resolves.

`base_sync` needs to be reachable from `wait.rs`; make it `pub` in `implement`
(or expose a thin `implement::conflict_in_worktree(worktree) -> Result<Option<String>>`
that wait calls and `detect_conflict` shares).

### Item 3 — config-gated auto-merge when behind-but-clean

New config field on `Config` (`config.rs`, after the `auto_collect_garbage`
block), with the mandatory `///` doc comment:

```rust
/// When true, ghwf automatically merges the base branch into a PR branch that
/// has fallen behind it whenever the merge is clean (no conflicts), then pushes
/// — keeping the open PR current with `main` and its CI fresh. ghwf never
/// auto-resolves an actual conflict; those are still surfaced for you or Claude
/// to handle. Off by default.
#[serde(default)]
pub auto_merge_base: bool,
```

Trigger point: the existing detection path in `work-on` (`main.rs:824-834`),
generalised to use `base_sync`. When the result is `BehindClean` and
`auto_merge_base` is on:
- `git::merge(worktree, "origin/<base>")` then `git::push(worktree, <branch>)`
  (branch from `prep.branch`).
- Note it in the banner (a one-line "brought the branch up to date with
  `origin/<base>`" prefix, analogous to but distinct from `conflict_notice`).
- Best-effort: any failure warns and falls through to normal behaviour (the
  branch is simply left behind; GitHub still squash-merges fine).

When `auto_merge_base` is off, `BehindClean` behaves exactly as today (no-op);
`Conflict` still yields the `conflict_notice` banner regardless of the setting.

Per the repo's "Adding a config option" checklist (CLAUDE.md), also:
- **`src/init.rs`**: add `set_auto_merge_base(doc)` and offer it in the wizard
  (mirror the `auto_collect_garbage` prompt block at `init.rs:232-254`); add a
  round-trip test mirroring `auto_collect_garbage_round_trips` (`init.rs:796`).
- **`README.md`**: document it in the annotated `ghwf.toml` example (near the
  `auto_collect_garbage` lines at `README.md:370-378`) and in prose.
- **`src/config_schema.rs`**: destructure `auto_merge_base` in the
  `example_covers_every_field` guard (`config_schema.rs:184`) and emit it in
  `ghwf config example` (`config_schema.rs:227`) with a value of `true` plus an
  explanatory comment. The guard won't compile and `example_*` tests fail until
  both are done.

## Files touched

- `src/git.rs` — add `merge`; unit tests for clean/conflicting merge.
- `src/implement.rs` — add `BaseSync` + `base_sync`; refactor `detect_conflict`
  to wrap it; expose the worktree-conflict check for `wait`.
- `src/main.rs` — `work-on` auto-merge on `BehindClean` (item 3); `hand_off`
  conflict block (item 1).
- `src/wait.rs` — periodic read-only conflict probe + wake reason (item 2).
- `src/state.rs` — `WaitState::conflict_seen` field (+ back-compat load test).
- `src/render.rs` — short "brought branch up to date" banner line for auto-merge;
  reuse `conflict_notice` for the rest.
- `src/config.rs`, `src/init.rs`, `src/config_schema.rs`, `README.md` —
  `auto_merge_base` config plumbing per the checklist.

## Testing

- **`git::merge`**: clean merge advances HEAD and contains both sides; a
  conflicting merge errors and leaves the worktree clean (aborted).
- **`base_sync`**: UpToDate (HEAD already contains base), BehindClean (base ahead,
  clean), Conflict (base ahead, conflicting) — built on the same temp-repo
  scaffold as `would_conflict_detects_conflicts_and_clean_merges`.
- **`detect_conflict`**: existing tests stay green (still returns `Some(base)`
  only on conflict; precondition guards unchanged).
- **`WaitState`**: add a back-compat load test (old JSON without `conflict_seen`
  loads with `false`), mirroring `old_wait_state_loads_without_reaction_watches`
  (`state.rs:1121`).
- **config**: `auto_merge_base_round_trips` (init) and the `example_*` guards/tests
  (config_schema) exercise the new field end to end.
- Item 1 (hand_off bail) and item 2 (wait edge logic) are largely integration
  glue over tested helpers; cover the wait edge logic (`Conflict && !seen` →
  reason + set; reset on clean) with a unit test on an extracted pure function
  that takes `(BaseSync, conflict_seen)` and returns `(Option<reason>, new_seen)`,
  so it's testable without a live git fetch.

## Out of scope / non-goals

- Rebasing or force-pushing PR branches (merge only, per pre-plan).
- Auto-merging during `wait` (the probe stays read-only; resolution happens when
  Claude is woken into `work-on`).
- Posting a separate label for the conflict signal — waking Claude (who then
  surfaces/resolves via the existing notice) is the chosen surface; the existing
  workflow labels already track attention.
