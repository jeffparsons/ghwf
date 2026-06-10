# More sophisticated tracking of active sessions

## Goal

Let worker pools (`ghwf forever`, `ghwf next --wait`) **safely auto-resume
an existing session** instead of treating every issue that has touched
ghwf state as permanently off-limits — and make a transient launch
failure stop locking the issue (or killing the worker).

Today an issue's *state-file existence* is overloaded to mean three
different things at once: "this issue has been started", "this issue is
claimed against other workers", and (by omission) "don't ever auto-pick
this again". Because the claim is taken *before* a session is known to be
running, and nothing releases it on failure, a transient hiccup leaves an
issue with a state file but no live session — picked over by every future
`next`/`forever`, resumable only by a human running `ghwf work-on <n>`.

The fix is to **track session liveness explicitly** with a short-lived
*lease*, separate from the durable workflow state, so selection can tell
*live* (a worker is on it — skip) from *not live* (safe to resume, or to
release if nothing was ever set up).

## Background

Key facts about today's mechanism (all local, single-machine — the data
dir is per-host, as `state::claim`'s doc notes):

- The per-issue state file `…/issues/<owner>/<repo>/<n>.json` is both the
  "started" marker and the cross-worker lock. `state::claim`
  (`src/state.rs:583`) creates it with `O_EXCL`; `next`'s `started_fn`
  (`src/next.rs:196`) treats *any* existing file as "started", and
  `select` skips started issues outright (`src/next.rs:323`).
- The real session identity (`PrepState::worktree_session_id`,
  `src/state.rs:405`) is only recorded later, by the in-Claude `work-on`
  run once it's executing inside the worktree (`src/main.rs:597-603`).
  Resuming is best-effort on top of it: `resumable_session`
  (`src/launch.rs:390`) returns an id only if its transcript still exists.
- There is **no liveness signal** — no pid, heartbeat, or lease. ghwf
  cannot distinguish "a session is running right now" from "a session
  started once and is long dead".
- Launch flow: outside Claude, `work_on` routes to `launch::run`
  (`src/main.rs:407`); `next` single-shot calls `pick()` (which claims)
  then `work_on` → `run`; `forever`/`next --wait` reach `run_forever`,
  which does `launch::prepare(...)?` then `supervise_once`
  (`src/next.rs:176-178`). `run` blocks in `child.wait()`;
  `supervise_once` loops in `monitor` polling state every 2 s.
- **The locking bug:** in `run_forever`, `launch::prepare(...)?` can fail
  transiently (issue fetch, model resolution, `prep::ensure_worktree` —
  `src/launch.rs:80,178,182`). Because it's `?`, one error ends the whole
  worker *and* leaves the claim taken in `wait_for_pick` on disk, with no
  session and nothing to resume.

## Design

Introduce a **session lease**: a small separate file, written by the
outside-Claude launcher process while it owns a live session, refreshed
on a heartbeat, and removed when the session ends.

- **File:** `…/issues/<owner>/<repo>/<n>.lease` (JSON), a sibling of the
  state file so it never races the in-Claude `work-on`'s `state::save`
  (which only ever rewrites `<n>.json`).
- **Contents:** `SessionLease { pid: u32, heartbeat: u64 }` —
  the launcher's process id and an epoch-seconds heartbeat
  (`SystemTime::now()`).
- **Liveness rule** (single-machine, so local pid semantics are valid):

  ```
  live  ⇔  process_alive(pid)  &&  now − heartbeat ≤ LEASE_TTL
  ```

  A live launcher always has a fresh heartbeat, so it's never falsely
  judged dead. A crashed launcher fails the pid check immediately; in the
  rare case its pid is recycled by an unrelated process, the stale
  heartbeat reclaims the lease after `LEASE_TTL`. `process_alive` is
  `libc::kill(pid, 0)` (libc is already a dependency, used in
  `src/launch.rs` for signals).

- **Constants:** `HEARTBEAT_INTERVAL = 15 s`, `LEASE_TTL = 120 s`
  (≈8× the interval, so a busy machine can miss a few beats without a
  false reclaim).

With the lease as the runtime liveness signal, an issue's *status* for
selection becomes a four-way classification rather than a yes/no:

- **Fresh** — no `<n>.json`. Pick and start (claim as today).
- **Resumable** — `<n>.json` exists, not concluded, lease not live.
  Pick and *resume* (this is the new auto-resume behaviour).
- **Live** — lease is live. A worker is on it; skip and report.
- **Done** — `<n>.json` exists and `is_concluded()`. Nothing to do; skip.

## Changes

### 1. `src/state.rs` — the lease type, file I/O, and liveness

- Add the serialisable record and a status enum:

  ```rust
  #[derive(Serialize, Deserialize)]
  pub struct SessionLease {
      pub pid: u32,
      // Epoch seconds; refreshed by the launcher's heartbeat thread.
      pub heartbeat: u64,
  }

  /// Whether an issue currently has a live launcher session.
  pub enum Liveness {
      Live,
      NotLive,
  }
  ```

- `fn lease_path(owner, repo, number) -> Result<PathBuf>` — `<n>.lease`
  beside `state_path`.
- `pub fn lease_liveness(owner, repo, number) -> Liveness` — read the
  lease file (absent/unreadable ⇒ `NotLive`) and apply the liveness rule
  via a testable inner `fn is_live(lease: &SessionLease, now: u64) -> bool`.
- `fn process_alive(pid: u32) -> bool` — `unsafe { libc::kill(pid as i32, 0) == 0 }`
  (treat `ESRCH` as dead; `EPERM` — exists but not ours — counts as alive).
- `pub struct LeaseGuard` — owns `owner/repo/number`, a heartbeat thread
  `JoinHandle`, and an `Arc<AtomicBool>` stop flag.
  - `pub fn acquire_lease(owner, repo, number) -> Result<Option<LeaseGuard>>`:
    try `O_EXCL` create of the lease file; on `AlreadyExists`, read it and
    check `is_live` — if live, return `Ok(None)` (someone else holds it);
    if stale, `remove_file` and retry the `O_EXCL` create once. Winning
    the create writes our `pid`+`heartbeat` and spawns the heartbeat
    thread (rewrites `heartbeat` every `HEARTBEAT_INTERVAL` via a temp
    file + `rename`, until the stop flag is set). The `O_EXCL` create is
    the serialisation point, so two workers reclaiming the same stale
    lease can't both win.
  - `impl Drop for LeaseGuard` — set the stop flag, join the thread, and
    `state::delete`-style remove the lease file (absence is not an error).
    Clean session exit thus releases the lease promptly; a crash leaves it
    for the staleness rule to reclaim.
- `pub fn release_if_unstarted(owner, repo, number) -> Result<()>` — the
  transient-failure recovery primitive: delete `<n>.json` **iff** it holds
  no durable progress (no `prep`, or a `prep` with neither `worktree_path`
  nor `worktree_session_id`) and is not concluded. A bare claim is thrown
  back to the pool as Fresh; an issue with a real worktree/session is left
  intact (it's Resumable).

### 2. `src/next.rs` — status-aware selection and claim/resume

- Replace the `already_started: impl Fn(u64) -> bool` parameter threaded
  through `select` / `claim_pick` / `pick_workable_leaf` with a status
  classifier:

  ```rust
  enum IssueStatus { Fresh, Resumable, Live, Done }
  ```

  and a real implementation `status_fn(owner, repo) -> impl Fn(u64) -> IssueStatus`
  composing `state::load_if_exists` (→ `Done` when `is_concluded`,
  else has-state) with `state::lease_liveness` (→ `Live`). Order:
  concluded ⇒ `Done`; else live lease ⇒ `Live`; else has state ⇒
  `Resumable`; else `Fresh`. (An unreadable state file ⇒ `Live` — the
  conservative choice; never barge into something we can't read.)

- `select` (`src/next.rs:301`): a candidate is any `Fresh` **or**
  `Resumable` issue (both ranked by the existing `sort_key` — no special
  preference; resuming vs starting is decided at launch, not by ordering).
  `Live` issues go to a new `skipped_live` bucket; `Done` issues are
  skipped silently (concluded — not noteworthy). The existing
  `skipped_blocked` / `skipped_tracking` logic is unchanged and still runs
  only for otherwise-eligible issues.

- `Selection` gains `skipped_live: Vec<u64>`; `announce_pick`
  (`src/next.rs:242`) reports them — `"Skipping #n — a session is
  currently running it."` — and, when the pick is `Resumable`, announces
  `"Resuming #n …"` rather than `"Picked #n …"`. The classifier is
  consulted once more for the winner to choose the verb. The
  `skipped_started` bucket/message is retired in favour of `skipped_live`.

- `claim_pick` (`src/next.rs:212`): only **Fresh** picks call
  `state::claim` (seeding `<n>.json` and pre-filtering concurrent workers,
  as today). A **Resumable** pick is returned without claiming — its
  single-flight is the launcher's lease acquisition (below), so a lost
  race is detected there. The injected `claim` closure is unchanged for
  Fresh; tests cover both branches.

- `pick_workable_leaf` (`src/next.rs:452`): its "prefer an already-started
  child" rule already wants in-progress work — keep it, now keyed on
  `status == Resumable || Live` ("work is underway") rather than the old
  boolean.

### 3. `src/launch.rs` — acquire the lease, resume, and survive failure

- `prepare` (`src/launch.rs:19`) returns `Result<Option<Launch>>`. After
  it has resolved the final `(owner, repo, number)` (post tracking-issue
  redirect) and loaded state, it acquires the lease:

  ```rust
  let Some(lease) = state::acquire_lease(&owner, &repo, number)? else {
      println!("Issue #{number} is already being worked by a live session; nothing to do.");
      return Ok(None);
  };
  ```

  The guard is stored on the returned `Launch` (new field `lease:
  LeaseGuard`), so it's held for the whole session and released when the
  `Launch` drops. Acquiring *after* number resolution keys the lease to
  the real leaf and lets `prepare` early-out before the worktree work when
  a live session already holds the issue.

- `run` (`src/launch.rs:483`): `let Some(launch) = prepare(...)? else { return Ok(()); }`
  then spawn/wait as today; the held `Launch` keeps the lease alive for
  the session.

- `run_forever` (`src/next.rs:174`) — make a transient failure
  non-fatal and non-locking:

  ```rust
  loop {
      let number = wait_for_pick(None)?;
      let launch = match launch::prepare(&number.to_string(), no_branch) {
          Ok(Some(launch)) => launch,
          // Another worker leased it between select and prepare: pick again.
          Ok(None) => continue,
          Err(err) => {
              eprintln!("warning: couldn't start #{number}, leaving it for later: {err:#}");
              // A bare claim with nothing set up goes back to the pool;
              // real progress is left as Resumable. The lease (if any) was
              // released when `prepare` unwound.
              let _ = state::release_if_unstarted(/* repo */, number);
              continue;
          }
      };
      match launch::supervise_once(&launch)? { /* unchanged */ }
  }
  ```

  `supervise_once` takes `&Launch` (holding the guard) as today; on return
  the `Launch` drops and the lease is released. `run_forever` resolves
  `(owner, repo)` once up front (via the same `github::repo_or_cwd` /
  `config_repo` path the picker uses) to call `release_if_unstarted`.

- Because the launcher now owns liveness, the heartbeat works uniformly
  for both `run` (which only `child.wait()`s) and `supervise_once`
  (which polls) — the `LeaseGuard`'s background thread handles it
  regardless of what the foreground does.

### 4. `src/main.rs` — single-shot paths

- `Commands::Next` single-shot (`src/main.rs:299-304`) still does
  `pick()` then `work_on(number)`. `pick` now skips `Live` and may return
  a `Resumable` number; `work_on` → `run` handles the `Ok(None)` "already
  live" case gracefully (prints and exits 0). No structural change beyond
  `run` returning early on `None`.
- The in-Claude `work-on` recording of `worktree_session_id`
  (`src/main.rs:597-603`) is unchanged — it writes `<n>.json` only and
  never touches the lease.

### 5. Tests

- `src/state.rs`: unit-test `is_live` (our own pid + fresh heartbeat ⇒
  live; an almost-certainly-dead pid ⇒ not live; live pid + stale
  heartbeat ⇒ not live); `acquire_lease` over a scratch dir (first caller
  wins, a second sees `None` while the first guard is held, a guard drop
  removes the file and lets a re-acquire succeed, a hand-written stale
  lease is reclaimed); and `release_if_unstarted` (deletes a bare default
  state file, keeps one carrying a worktree/session, keeps a concluded
  one).
- `src/next.rs`: update the existing `select`/`claim_pick` tests for the
  `IssueStatus` classifier (the `pick` helper's `|_| false` becomes
  `|_| IssueStatus::Fresh`; `|n| n == 1` cases become the matching
  status). Add: a `Live` issue is skipped and reported in `skipped_live`;
  a `Done` issue is skipped silently; a `Resumable` issue is selected (and
  announced as a resume); a `Resumable` pick does **not** call the `claim`
  closure while a `Fresh` pick does.

### 6. `README.md`

- In the `ghwf forever` / `next --wait` section, document the new
  behaviour: the pool now **resumes** an issue whose session has stopped
  (rather than skipping it forever), skips one with a live session, and
  recovers from a transient launch error by leaving the issue pickable
  instead of locking it. No new config key, so no `ghwf.toml` /
  `config init` changes are needed.

## Out of scope

- No change to how `claude --resume` itself works, or to transcript
  handling — `prepare`'s existing resume path is reused as-is.
- No cross-machine coordination: the lease is single-host, matching
  today's `state::claim` scope.
- No "prefer resumable over fresh" ordering, and no attention-label or
  retry-budget bookkeeping for issues whose resume keeps failing — a
  repeatedly-failing Resumable issue is simply left for the next worker
  run (filing a follow-up if it proves noisy in practice).

## Verification

- `cargo test` (new lease/recovery unit tests + updated selection tests).
- `cargo clippy`.
- Manual: start `ghwf forever`, `kill -9` the launcher mid-session, and
  confirm a fresh `ghwf forever` *resumes* that issue (rather than
  skipping it) once the pid is gone. Run two `ghwf forever` workers and
  confirm one leases an issue while the other skips it as live. Simulate a
  transient `prepare` failure (e.g. point at an unreachable network) and
  confirm the worker logs, leaves the issue pickable, and continues rather
  than dying.
