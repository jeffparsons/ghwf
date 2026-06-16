# Plan: Periodically check whether the main branch has moved on

## Goal

Proactively tell Claude when `origin/<base>` has moved on under an open PR — not
only when it now *conflicts* (already handled) but also when it has advanced
**cleanly**. A clean advance can still carry commits that change Claude's plan or
implementation (a refactor it should follow, work that supersedes its own, a new
helper to reuse). When that happens, surface a banner that prompts Claude to
review the new commits, decide whether they affect the plan/implementation, and
bring the branch up to date.

Scope (per the issue discussion): the **implement** and **review** phases, where
a branch and worktree already exist — reuse the existing base-sync machinery; do
*not* track a baseline commit through the planning phases.

## Background / what already exists

The "merge conflict hinting" machinery this mirrors:

- `implement::base_sync()` (`src/implement.rs:95`) fetches `origin/<base>` and
  classifies HEAD as `BaseSync::UpToDate` / `BehindClean` / `Conflict`.
  `check_base()` (`src/implement.rs:116`) wraps it with the branch/PR/worktree
  preconditions, swallowing errors so it never breaks a run.
- At a `work-on` boundary, `implement::base_banner()` (`src/implement.rs:175`)
  turns that verdict into a leading banner for the implement/review phase body
  (`src/main.rs:895-905`):
  - `Conflict` → `render::conflict_notice` ("resolve this now").
  - `BehindClean` + `auto_merge_base` on → merges `origin/<base>` in, pushes, and
    shows `render::base_merged_notice` (passive "nothing to do").
  - `BehindClean` + `auto_merge_base` off → **nothing**.
  - `UpToDate` → nothing.
  - The banner leads the body and is never posted to a thread.
- While a PR idles in the **review** phase, `ghwf wait` re-probes every
  `CONFLICT_PROBE_INTERVAL` (300 s, `src/wait.rs:25`) via
  `implement::detect_conflict` (`src/wait.rs:127-141`). `conflict_wake`
  (`src/wait.rs:974`) wakes only on the clean→conflict edge, tracked across wait
  cycles by `WaitState::conflict_seen` (`src/state.rs:407`). A persistent conflict
  stays quiet; clearing it resets the flag.

So today only **conflicts** are surfaced proactively, and a clean advance with
`auto_merge_base` off is silently ignored.

`WaitState` is rebuilt from `Default` on every `work-on` (`src/main.rs:1028`), so
any edge flag in it resets each work-on span and must be seeded from the verdict
`work-on` itself observed (otherwise the first probe re-wakes for the same
advance `work-on` just surfaced).

## The gap

1. `base_banner` says nothing for a clean-but-behind branch when
   `auto_merge_base` is off, and only passively confirms the merge when it's on —
   neither prompts Claude to *reconsider its plan*.
2. The `wait` probe wakes on conflicts only, never on a clean advance.
3. The edge tracking (`conflict_seen`) is a single bool — it can't distinguish
   "newly behind (clean)" from "newly conflicting".

## Approach

### 1. Generalise the edge state (`src/state.rs`)

Replace `WaitState::conflict_seen: bool` with the full last-seen verdict so we can
edge-trigger on either kind of advance:

```rust
// The branch's sync state against its base as of `wait`'s last probe, tracking
// the edge across wait cycles so a fresh advance wakes once, not every cycle.
// Seeded by `work-on` from the verdict it already surfaced. UpToDate for old
// state files.
#[serde(default)]
pub last_base_sync: BaseSync,
```

Reuse `implement::BaseSync` for the stored value rather than duplicating the enum:
add `Serialize, Deserialize, Default` (default = `UpToDate`) to it
(`src/implement.rs:82`) and store it directly. (Alternative if the
state→implement reference is unwanted: a mirrored `state::BaseSyncState` with a
`From<BaseSync>`. Prefer reuse.)

### 2. Wake on either advance edge (`src/wait.rs`)

- Rename `CONFLICT_PROBE_INTERVAL` → `BASE_PROBE_INTERVAL` (still 300 s; it now
  covers clean advances too) and update the comments at `src/wait.rs:114-124`.
- In the probe (`src/wait.rs:127-141`), call `implement::check_base` (returns
  `Option<(String, BaseSync)>`) instead of `detect_conflict`, and replace
  `conflict_wake` with a generalised `base_wake`:

```rust
// Wake when the branch has entered a new "behind" state since the last probe:
// UpToDate->BehindClean, UpToDate->Conflict, BehindClean->Conflict (escalation),
// or Conflict->BehindClean. A state unchanged from last probe stays quiet (no
// per-cycle nagging); UpToDate never wakes. Returns the wake reason and the new
// last-seen verdict to persist.
fn base_wake(cur: Option<&(String, BaseSync)>, number: u64, prev: BaseSync)
    -> (Option<String>, BaseSync)
```

Rule: with `cur = Some((base, verdict))`, wake iff `verdict` is a behind-state
(`BehindClean`/`Conflict`) **and** `verdict != prev`; the reason text differs by
kind:
  - `Conflict` → today's "`origin/{base}` moved on and PR #{number} now conflicts
    with it." (unchanged wording).
  - `BehindClean` → "`origin/{base}` moved on; PR #{number} is now behind it."

  Return `verdict` as the new `prev`. With `cur = None` (no branch/worktree on a
  probe — unusual in review) leave `prev` unchanged and don't wake.

The persist/print/return flow at `src/wait.rs:136-140` is unchanged.

### 3. Seed the edge state from `work-on` (`src/main.rs`, `src/implement.rs`)

`work-on` must record the post-banner verdict into the fresh `WaitState` so the
first probe doesn't re-wake for the advance `work-on` already surfaced.

Have `base_banner` also report the branch's *effective* sync state after any
auto-merge it performed. Change its return to carry both, e.g.:

```rust
pub struct BaseStatus {
    pub banner: Option<BaseBanner>,
    /// The branch's sync state after any auto-merge this call performed
    /// (UpToDate after a successful auto-merge, else the observed verdict).
    pub effective: BaseSync,
}
pub fn base_status(prep, number, auto_merge) -> Option<BaseStatus>
```

`None` only when `check_base` is `None` (no branch/PR/worktree). Otherwise:
  - `UpToDate` → `{ None, UpToDate }`.
  - `Conflict` → `{ Some(Conflict notice), Conflict }`.
  - `BehindClean` + auto_merge on + merge ok → `{ Some(Merged notice), UpToDate }`.
  - `BehindClean` + auto_merge on + merge fails → `{ None, BehindClean }` (warns,
    as today).
  - `BehindClean` + auto_merge off → `{ Some(Behind notice), BehindClean }` — the
    new heads-up (see §4).

In `work-on` (`src/main.rs:895-905`): capture a local `base_effective:
Option<BaseSync>` alongside the banner, then seed it when building `WaitState`
(`src/main.rs:1028`): `last_base_sync: base_effective.unwrap_or_default()`. For
phases/states where `base_status` isn't computed it stays `UpToDate` (the probe
only runs in review, where it *is* computed).

### 4. Notices and the new banner variant (`src/implement.rs`, `src/render.rs`)

Add a third `BaseBanner` variant for the clean-but-behind, not-auto-merged case:

```rust
pub enum BaseBanner {
    Conflict(String),  // must resolve before done
    Behind(String),    // clean advance, auto-merge off: review + integrate
    Merged(String),    // ghwf merged it in: review optional
}
```

Replace `BaseBanner::is_conflict()` (used at `src/main.rs:1208` to keep a standing
conflict counted as "activity" so the stop-nudge counter resets) with a semantic
`keeps_ball()` → true for `Conflict` and `Behind` (both want Claude to act),
false for `Merged` (informational). Update the call site.

Notices in `src/render.rs`:
  - Keep `conflict_notice` as-is.
  - New `base_behind_notice(base, number)` — clean advance, auto-merge off:
    headline "ℹ️ `origin/<base>` has moved on", body telling Claude to review the
    new commits (`git log HEAD..origin/<base>`), judge whether they affect its
    plan/implementation, then integrate (`git merge origin/<base>` or rebase) and
    push.
  - Revise `base_merged_notice` from the passive "nothing for you to do" to an
    active prompt: ghwf merged `origin/<base>` in (clean) and pushed; review the
    just-merged commits (`git log -p ORIG_HEAD..HEAD` / inspect the merge commit)
    and check whether they change the plan/implementation; adjust if needed.

Both new/revised notices, like `conflict_notice`, lead the banner and are never
posted to a thread.

### 5. Keep `detect_conflict`

`detect_conflict` (`src/implement.rs:136`) is still used by the implement→review
hand-off gate (`src/main.rs:2122-2134`), which only cares about conflicts — leave
it and that gate unchanged. Only the `wait` probe migrates to `check_base` +
`base_wake`.

## Files to change

- `src/state.rs` — swap `conflict_seen: bool` for `last_base_sync: BaseSync`
  (`#[serde(default)]`); update the two state tests that set/inspect
  `conflict_seen` (`src/state.rs:1345,1367,1382`).
- `src/implement.rs` — `Serialize/Deserialize/Default` on `BaseSync`; add the
  `Behind` `BaseBanner` variant + `keeps_ball()` (replacing `is_conflict()`);
  introduce `BaseStatus` and `base_status()` (refactor of `base_banner`).
- `src/render.rs` — add `base_behind_notice`; revise `base_merged_notice` wording.
- `src/wait.rs` — rename the interval const; probe via `check_base`; replace
  `conflict_wake` with `base_wake`; seed/track `last_base_sync`; update comments
  and the `conflict_wake` unit test.
- `src/main.rs` — call `base_status` and capture `base_effective`; seed
  `WaitState::last_base_sync`; swap `is_conflict()` → `keeps_ball()` at 1208.

No new config option (reuses `auto_merge_base`), so the CLAUDE.md "adding a config
option" checklist does not apply.

## Edge cases

- **Old state files** lack `last_base_sync` → defaults `UpToDate`. Worst case a
  single one-time wake if the branch is already behind at first probe after
  upgrade (correct enough — main did move on).
- **No re-nag.** A persistent behind/conflict state stays quiet (`cur == prev`),
  matching today's conflict behaviour. This *does* drop one pre-existing edge:
  today an unresolved conflict can re-wake once per work-on span (because
  `conflict_seen` reset each span); seeding `last_base_sync` from `work-on`'s
  verdict means an *acknowledged* standing conflict no longer re-wakes. Called
  out as an intentional, minor refinement (less naggy; any activity still
  re-surfaces it on the next `work-on`).
- **auto_merge on, fast-moving main:** each distinct advance wakes once → the next
  `work-on` merges → `UpToDate` → the next advance wakes again. Intended (keep
  current + reconsider); the 300 s floor and edge-trigger bound the rate.
- **auto_merge off:** the branch stays `BehindClean`; the probe stays quiet after
  the first wake until the state changes (further clean advances don't re-wake;
  an escalation to `Conflict` does).
- **Probe stays review-only.** Implement-phase advances are caught by the next
  `work-on` boundary, exactly as conflicts are today.
- `base_status` returning `None` (no branch / no PR / no worktree / fetch error)
  → no banner, `last_base_sync` defaults `UpToDate`, probe is a no-op.

## Testing / verification

- `cargo build`, `cargo clippy`, `cargo test` clean.
- Unit tests:
  - `base_wake`: replace/extend the `conflict_wake` test — assert wakes on
    `UpToDate→BehindClean`, `UpToDate→Conflict`, `BehindClean→Conflict`; quiet on
    unchanged states and `→UpToDate`; correct reason text per kind; `None` leaves
    `prev` and doesn't wake.
  - `render`: `base_behind_notice` names the base + issue and includes the
    review/integrate steps; revised `base_merged_notice` keeps its headline and
    adds the review prompt.
  - `base_status`/`BaseBanner`: `keeps_ball()` is true for `Conflict`/`Behind`,
    false for `Merged`.
- Manual: in a worktree with an open draft PR, advance `origin/<base>` with a
  non-conflicting commit; run `ghwf work-on` and confirm the behind/merged banner
  (per `auto_merge_base`) leads the body. Then idle in review and confirm
  `ghwf wait` wakes once ~5 min after a fresh clean advance, with the next
  `work-on` surfacing the notice; confirm a second identical-state probe stays
  quiet.

## Out of scope

- Planning-phase detection (pre-plan / prep-and-plan) via a recorded baseline
  commit — explicitly deferred in the issue discussion.
- Any new config toggle; the feature is always-on, mirroring conflict hinting and
  reusing `auto_merge_base` for the merge half.
- Changing the hand-off conflict gate or the implement-phase wait behaviour.
