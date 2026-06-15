# Plan — #110: Re-sync labels/attention after a crashed or killed session

## Problem

When a launcher process crashes or is killed mid-workflow it leaves behind:

1. a **stale lease file** (`<n>.lease.json`) whose pid is dead or whose
   heartbeat has aged past `LEASE_TTL` (120s);
2. the issue's **state file**, frozen at whatever phase/attention the dead run
   last wrote; and
3. the **GitHub labels**, which already match that frozen state — the last
   `labels::sync` succeeded and recorded `labels_synced`.

So the labels haven't *drifted* from the state file; the state file itself is
the lie. An issue killed mid-prep keeps showing `ghwf:preparing`
(`waiting-on-ghwf`) or `ghwf:claude-working` (`waiting-on-claude`) — "the
machine is on it" — when nothing is running. For anyone not running a `forever`
pool (which would re-pick the `Resumable` issue and re-sync on resume), that
badge sits there until a human manually re-enters the issue.

Decision taken in pre-plan (issue #110): an abandoned issue's attention should
flip to **`waiting-on-user`** (`ghwf:needs-you`) and its labels be re-synced, so
the human sees ghwf bailed and it's their move to restart it.

## Approach

A lightweight **sweep** over the local issue state files, driven once per
`work-on` run. For each issue whose recorded session crashed, reset its
attention to `WaitingOnUser`, persist that to the state file, and re-sync the
GitHub labels via the existing `labels::sync`.

### The crash signature: a *stale lease that still exists*

The reset must fire only for genuinely crashed sessions, never for an issue
that's merely between launch steps. The precise discriminator is the lease
file:

- **Crashed / killed** → the lease file is *present* but not live (dead pid, or
  heartbeat past TTL). This is exactly the window #110 is about.
- **Just claimed, lease imminent** → *no* lease file yet (the launcher creates
  it moments after the claim). Must not be touched.
- **Bare claim / clean exit** → no lease file (a `LeaseGuard` drop removes it).
  Not a crash; out of scope.

So the condition keys off *"a lease file exists AND is not live"*, not the
existing `lease_liveness` (which collapses absent and stale into `NotLive`). A
freshly created lease is live immediately (live pid + fresh heartbeat), so there
is no race against a launching worker.

### Reset condition (per issue)

Reset iff **all** hold:

1. state file exists and `!state.is_concluded()`;
2. `state.attention` is a "machine is working" state — `WaitingOnGhwf` or
   `WaitingOnClaude`. `WaitingOnUser` is already truthful, so it's left alone
   (also makes the sweep idempotent — see below);
3. a lease file exists for the issue but is **not** live (the crash signature).

On reset: `state.attention = WaitingOnUser`, `state::save`, then `labels::sync`
with the issue's code repo and `state.prep.pr_number`.

### Why this needs no throttle and no new config

- After a reset, the state file records `attention = WaitingOnUser`, so on the
  next sweep condition (2) fails in memory — the issue is skipped with **zero**
  GitHub API calls. Concluded and live issues are likewise skipped cheaply. The
  only steady-state cost is a local directory walk + JSON parse, so no
  persistent throttle stamp (à la GC's `last-run`) is needed.
- `labels::sync` already no-ops entirely when no `[labels]` section is
  configured. The sweep extends that same automatic, non-destructive label
  hygiene to crashed siblings, so — like `labels::sync` itself — it is **not**
  gated behind any opt-in (and unlike #99's `auto_collect_garbage`, it deletes
  nothing). The sweep early-returns when labels aren't configured, so it costs
  nothing for repos that don't use labels.

### Why *not* a reclaim-time reset

The pre-plan hand-off floated also resetting at lease-reclaim time as
belt-and-braces. On reflection that's logically inconsistent with the chosen
semantics: a reclaim happens precisely when a new worker is *taking the issue
over*, so moments later it sets `WaitingOnClaude` and works it — flipping to
`WaitingOnUser` there would be wrong. The reclaim/resume path already re-syncs
correctly; the sweep is what covers the "nobody takes it over" gap. So no
reclaim-time hook.

### Scope boundary: clean user-quits

A session the user quits cleanly (guard dropped → lease file removed) leaves no
crash signature, so the sweep won't reset it even if it's unconcluded and shows
`claude-working`. That matches #110's framing ("crashed or killed", lease left
behind) and avoids second-guessing deliberate exits. Noted as intentional.

## Changes

### 1. `src/state.rs` — expose the stale-lease check

Add a small, testable helper that distinguishes *stale-but-present* from
*absent*, mirroring `lease_liveness`/`is_live`:

```rust
/// Whether an issue's lease file exists but is not live — the signature of a
/// crashed or killed session (as opposed to an absent lease, which means no
/// session has started). Absent or unreadable lease reads as `false`.
pub fn lease_is_stale(owner: &str, repo: &str, number: u64) -> bool
```

Implemented via a path-taking inner fn (e.g. `lease_is_stale_at(path, now)`) so
it's unit-testable with the existing `lease_scratch` pattern, like
`acquire_lease_at`. `false` when the file is absent or unparseable; `true` when
it loads and `!is_live(&lease, now)`.

### 2. New module `src/resync.rs` — the sweep

```rust
/// Reset the attention/labels of any crash-abandoned issue across the repos
/// this config covers. Best-effort: every failure is a stderr warning, never
/// propagated. No-op when no `[labels]` section is configured.
pub fn sweep(located: &config::Located) -> Result<()>
```

- Early-return `Ok(())` if `located.config.labels.is_none()`.
- Build the issue-repo set: `github::repo_or_cwd()?` plus
  `located.config.issue_repo_refs()?` (dedup). State files for a foreign issue
  live under its own `issues/<owner>/<repo>/` dir, so this set covers every
  issue the config knows about; the common single-repo case is one dir.
- For each repo, walk its `<n>.json` state files (reuse the directory-walk shape
  already in `state.rs`; factor a shared helper if clean, otherwise a small
  local walk), and for each issue call `reset_if_abandoned`.

```rust
/// Reset one issue if it bears the crash signature. Returns whether it acted.
fn reset_if_abandoned(issue_repo: &RepoRef, number: u64, state: IssueState) -> Result<bool>
```

- Pure decision factored out for tests:
  `fn should_reset(concluded: bool, attention: Attention) -> bool`
  (`!concluded && matches!(attention, WaitingOnGhwf | WaitingOnClaude)`).
- Combine with `state::lease_is_stale(owner, repo, number)`.
- When all hold: set `state.attention = WaitingOnUser`, `state::save(...)`,
  resolve `code_repo = github::code_repo(issue_repo)?` and
  `pr_number = state.prep.and_then(|p| p.pr_number)`, then
  `labels::sync(issue_repo, &code_repo, number, pr_number, &mut state)`.
- `log`/`println!` a one-line note when it actually resets an issue, so the
  hygiene isn't fully silent (e.g. `Reset #N to needs-you — its session looks
  to have crashed.`).

Register `mod resync;` in `src/main.rs`.

### 3. `src/main.rs` — drive the sweep from `work_on`

In `work_on` (after `let located = config::find()?;` is available), call the
sweep once per run, best-effort:

```rust
if let Some(located) = located.as_ref() {
    if let Err(err) = resync::sweep(located) {
        eprintln!("warning: stale-session resync sweep failed: {err:#}");
    }
}
```

This runs for both manual `ghwf work-on N` users and `forever` pools (whose
sessions run `work-on` every round). The current issue is never its own target —
it holds a live lease. A `forever` worker only parks when no `Resumable` issues
exist, and an abandoned issue *is* `Resumable`, so a parked worker has nothing to
sweep; driving from `work-on` is sufficient (no separate `forever`/`next` hook).

## Tests

- `state.rs`: `lease_is_stale_at` — `false` for absent file; `false` for a fresh
  self-pid lease; `true` for a dead-pid lease; `true` for a live-pid lease with a
  heartbeat past TTL. (Mirror the existing `is_live_*` tests.)
- `resync.rs`: `should_reset` truth table — `WaitingOnGhwf`/`WaitingOnClaude` +
  not concluded → `true`; `WaitingOnUser` → `false`; concluded → `false`
  regardless of attention.
- `resync.rs`: an end-to-end-ish test over a scratch issues dir + scratch lease
  files exercising `reset_if_abandoned`'s decision (the label HTTP call stays
  behind `labels::sync`'s "no `[labels]` configured" no-op, so the test asserts
  the state-file attention flip and idempotence on a second pass), if it can be
  driven without the network; otherwise keep coverage at the pure-decision +
  `lease_is_stale` layer.

## Out of scope / non-goals

- No change to selection/eligibility — it keys off the state file, so a pool
  still re-picks and resumes these (which flips attention back to
  `claude-working` as normal).
- Nothing touches branches or worktrees — labels/attention only.
- No reclaim-time reset (see rationale above).
- No new config option (consistent with `labels::sync` being ungated).
