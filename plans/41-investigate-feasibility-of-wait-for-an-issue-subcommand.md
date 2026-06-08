# Plan — `ghwf next --wait`: a launcher-pool worker (#41)

The goal: start N plain terminals, each running `ghwf next --wait`, and have
them sit idle until issues become available — at which point each worker
claims one issue (exactly one worker per issue), assigns it to the user on
GitHub, creates the worktree, and execs an interactive Claude session that
starts working immediately. The whole flow is then drivable from a phone via
GitHub: open an issue, and a pool worker picks it up and engages.

Design decisions locked on the issue thread:

1. **Option A — the pool waits *outside* Claude** (launcher pool), not inside
   already-running Claude sessions. This keeps the one-session-per-worktree
   invariant intact: the session is born anchored in its issue's worktree, so
   resume, the Stop hook, and the worktree guard all work unchanged.
2. **Launched sessions self-start via an initial prompt.** `claude "/work-on"`
   (a positional prompt, *without* `-p`) is the documented
   "start interactive session with initial prompt" mode, billed to the
   subscription; only `-p`/`--print` (headless) is the programmatic category
   that moves to API-rate billing. The existing `launch.rs` comment claiming
   any prompt is programmatic use is wrong and gets corrected. Always enabled
   — no config key.
3. **Command shape: `ghwf next --wait [--timeout <secs>]`**, not a separate
   subcommand. Without `--timeout` the worker waits indefinitely (the normal
   pool case); with it, exit 2 on timeout mirrors `wait`'s contract.
4. **Claiming is local-only and atomic.** The per-issue state file is already
   the "started" marker that makes `next` skip an issue; claiming = creating
   it with `create_new` (atomic exclusive create — local contention is all we
   need per the issue). A worker that loses the race re-runs selection and
   keeps waiting.
5. **A claim assigns the issue to the user on GitHub**, best-effort, so the
   pickup is visible from the phone.

## 1. Atomic claim (`state.rs`)

```rust
/// Atomically record an issue as started, claiming it against concurrent
/// local workers. Returns false when some session already holds state for it.
pub fn claim(owner: &str, repo: &str, number: u64) -> Result<bool> {
    let path = state_path(owner, repo, number)?;
    fs::create_dir_all(path.parent().…)?;
    claim_file(&path)
}

/// The exclusive create against a concrete path (parameterized for tests).
fn claim_file(path: &Path) -> Result<bool> {
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            // Write the default state so the file parses everywhere.
            file.write_all(serde_json::to_string_pretty(&IssueState::default())?…)?;
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err)…,
    }
}
```

Notes:

- `create_new` is atomic on local filesystems, which is the stated contention
  scope. No locks, no PIDs, no cleanup protocol.
- A claimed-then-abandoned issue (worker killed before Claude engaged) looks
  exactly like today's started-but-stalled issues: `next` skips it and says
  so, and `ghwf work-on <n>` resumes it explicitly. No rollback on
  post-claim failures — racy to undo, and the resume path already exists.

## 2. Claim-aware selection (`next.rs`)

`select()` is unchanged. On top of it, a claim loop that tolerates losing
races: try the winner, and if another worker got there first re-run selection
(the freshly-claimed issue now fails the `already_started` check) until a
claim sticks or no candidate remains.

```rust
/// Pick and claim the best eligible issue: select, attempt the claim, and on
/// a lost race re-select. `claim` is injected for tests.
fn claim_pick<'a>(
    issues: &'a [IssueListing],
    me: &str,
    priority_labels: &[String],
    already_started: impl Fn(u64) -> bool,
    mut claim: impl FnMut(u64) -> Result<bool>,
) -> Result<Selection<'a>> { … }
```

`pick()` becomes: fetch the listing once, `claim_pick`, then (on success)
best-effort assignment + the existing pick/rationale output. Plain
`ghwf next` gains the same claim + assign semantics — two simultaneous `next`
runs can no longer double-pick, and the behaviour is uniform with `--wait`.

The skipped-started report stays as is.

## 3. Assignment on claim (`github.rs`)

```rust
/// Add `login` to an issue's assignees.
pub fn add_assignee(owner: &str, repo: &str, number: u64, login: &str) -> Result<()>
```

`POST repos/{owner}/{repo}/issues/{number}/assignees` via the existing
`gh_api` plumbing. Called after a successful claim, skipped when the issue is
already assigned to the user, and best-effort: a failure warns and the claim
stands (assignment is a visibility nicety, not a correctness requirement —
self-assignment never changes the issue's eligibility for other workers,
which the claim already excludes).

## 4. The wait loop (`next.rs`)

```rust
/// Block until an eligible issue can be claimed; return its number. With a
/// timeout, exits the process with EXIT_TIMEOUT (2) when it elapses.
pub fn wait_for_pick(timeout_secs: Option<u64>) -> Result<u64>
```

- Resolve config (priority labels), repo, and the authenticated user once.
- Loop: conditional GET of `repos/{owner}/{repo}/issues?state=open&per_page=100`
  via the existing `github::gh_api_conditional`, holding the ETag in memory
  (no persistence — a restarted worker just pays one uncached fetch).
  - 200 → parse `Vec<IssueListing>`, run the section-2 claim loop. A claim
    returns the number; the caller proceeds exactly as `next` does today.
  - 304 / no eligible candidate → sleep with backoff and poll again.
- Backoff mirrors `wait.rs`: floor 5 s, doubling to a 60 s cap, resetting to
  the floor when a poll sees a fresh (200) response.
- **Direct polling only — no events-feed idle mode.** `wait` polls up to five
  endpoints per cycle, which made the feed handover worth its complexity;
  this loop polls exactly one. At the cap that's 60 requests/hour per worker
  (304s *do* count against the REST rate limit — verified empirically in
  plan 7), so even a ten-worker pool idles at ~12 % of the 5000/hour budget.
  Feed-first idling can be bolted on later if pools grow.
- Failure handling: rate-limit responses pin the backoff at the cap (as in
  `wait.rs`). Other consecutive failures abort — but at a higher threshold
  than `wait`'s 3 (30, ≈ half an hour at the cap), because a pool worker is
  unattended by design and shouldn't die to a transient network blip;
  any success resets the counter.
- Messaging: announce what's being waited for on entry, the pick + rationale
  on success (existing output), and the skipped-started lines as they occur.

Pagination parity note: `list_open_issues` reads one page of 100, and the
wait loop inherits that. Same limitation as today's `next`; not addressed
here.

## 5. CLI wiring (`main.rs`)

```rust
Next {
    #[arg(long)]
    no_branch: bool,
    /// Block until an eligible issue appears, claim it, then start work.
    #[arg(long)]
    wait: bool,
    /// With --wait: give up after this many seconds, with exit code 2.
    #[arg(long, requires = "wait")]
    timeout: Option<u64>,
},
```

Dispatch: `wait` → `work_on(&next::wait_for_pick(timeout)?.to_string(), …)`;
otherwise the existing `next::pick()` path. Outside Claude, `work_on`
delegates to the launcher, which creates the worktree and (now) execs a
self-starting session — so `ghwf next --wait` in a plain terminal is the
complete pool-worker story. Running it inside a Claude session isn't blocked
(it degrades to today's in-session `next` behaviour after the wait), but the
README documents it as an outside-Claude tool.

## 6. Self-starting sessions (`launch.rs`)

- `exec_claude` gains the positional initial prompt `/work-on`, passed in
  every launch variant: fresh, resume, and `--no-branch`.
- The "Never pass `-p`/`--print` (or any prompt)" comment is rewritten: the
  programmatic/headless category is `-p`/`--print` (billed separately from
  the subscription); a positional initial prompt on an *interactive* session
  is documented, subscription-billed use. We still never pass `-p`.
- `print_fresh_reminder` goes away; the launch messages now say the session
  will pick up the workflow itself.

Manual verification (part of this phase, since the behaviour lives at the
`claude` CLI boundary): `claude "/work-on"` starts interactive and submits
the prompt; `claude --resume <id> "/work-on"` does the same on resume. If
resume-with-prompt turns out not to submit the prompt, the resume path keeps
a printed reminder instead, and the plan's PR notes it.

## 7. Documentation (`README.md`)

- New section on the worker pool: open N terminals, run `ghwf next --wait`
  in each; each worker claims at most one issue, assigns it, and launches a
  self-starting session. `--timeout` and the exit-code contract.
- Update launch-flow prose that says to type `/work-on` once Claude is up.
- No config changes (the initial prompt is always on), so no wizard or
  annotated-`ghwf.toml` updates.

## 8. Tests

- `state::claim_file`: first claim returns true and writes parseable default
  state; second returns false; other IO errors propagate. (Temp-dir based,
  like the existing state tests.)
- `next::claim_pick`: winner claimed on the happy path; a lost race falls
  through to the next candidate; all candidates lost → `None`; the
  `skipped_started` report unaffected.
- Existing `select` tests unchanged.
- The polling loop's GitHub interactions and `exec_claude`'s prompt are
  verified manually (two-terminal race included: open two `next --wait`
  workers, file one issue, confirm exactly one claims it and the other keeps
  waiting).
