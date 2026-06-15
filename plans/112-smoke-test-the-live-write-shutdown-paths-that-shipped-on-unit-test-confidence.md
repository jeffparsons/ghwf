# #112 — Smoke-test the live write/shutdown paths that shipped on unit-test confidence

## Goal

Deliberately exercise, once each, the four live paths that were merged with
their real-world side effects self-flagged as unverified, and fix whatever
surfaces. This is QA/tracking, not a known-bug fix — the deliverable is
*evidence the paths work live* plus any fixes the exercise turns up. The user
has explicitly said this is a nice-to-have and that scoping down on any path
that proves awkward is fine.

The four paths (from the #97 history audit):

1. **#58** — `create-issue` + native GitHub `blocked_by` dependency POST.
2. **#69** — cross-repo `issue_repos` end-to-end against a real foreign repo.
3. **#82** — session lease / crash-recovery (stale-lease reclaim + resync).
4. **#77** — the double-SIGINT shutdown gesture against a real Claude session.

## Approach

Most of these I can drive from inside this session (all `ghwf` commands are
pre-authorised) and verify through the live GitHub API or the local state dir.
The one exception is #77's real-session terminal check, which needs a human at a
TTY — I'll supply a precise manual script for it.

Each path gets recorded in a durable smoke-test log committed to the repo, so
the verification isn't ephemeral. Code fixes for anything that surfaces go in
this PR; anything large or independent gets filed as a follow-up issue via
`ghwf create-issue`.

### Artifact

A new file `docs/smoke-tests/112-live-paths.md` recording, per path: the exact
commands run, the observed live result (with API output / state-file evidence
quoted), pass/fail, and any fix or follow-up. The `docs/smoke-tests/` directory
is new; this is its first entry.

## Path 1 — `create-issue` + `blocked_by` (#58)

`create_issue()` (`src/main.rs`) sets the blocked label atomically at creation,
then `add_blocked_by()` (`src/github.rs`) POSTs to
`repos/{owner}/{repo}/issues/{n}/dependencies/blocked_by`. Only label-assembly
and arg-resolution are unit-tested; the live POST has never run.

**Exercise (in `jeffparsons/ghwf`, the current repo):**

1. `ghwf create-issue --title "ghwf #112 smoke: blocked_by probe" 112` — files a
   throwaway issue blocked by #112.
2. Confirm via the live API that the dependency landed:
   `gh api repos/jeffparsons/ghwf/issues/<new>/dependencies/blocked_by` shows
   #112; and that the blocked label is present then handled as designed.
3. Also exercise `--no-block` once to confirm the standalone path creates an
   issue with neither the label nor a dependency.
4. **Clean up:** close the throwaway issue(s) once verified.

**Pass criteria:** the dependency is visible through the GitHub dependencies
endpoint (not just the label), and `--no-block` creates a clean standalone
issue.

## Path 2 — cross-repo `issue_repos` (#69)

`validate_issue_url_against_issue_repos()` / `code_repo()` (`src/github.rs`)
gate a foreign issue, branches get a per-repo prefix, and labels sync across
both repos (`src/labels.rs`, `src/resync.rs`, `src/wait.rs`). Config + URL
validation + branch naming are unit-tested; a real foreign-repo run never has.

**Exercise:**

1. Create a throwaway public repo `jeffparsons/ghwf-smoke-test` (approved) and
   file a test issue in it.
2. Point a scratch ghwf config at it via
   `issue_repos = ["jeffparsons/ghwf-smoke-test"]` (or the table form with a
   `branch_prefix`), without disturbing the real working config.
3. Verify the live behaviours that the mocks stand in for:
   - the allowlist gate accepts the foreign issue URL and rejects an
     un-listed one;
   - the foreign-issue branch carries the expected prefix;
   - label sync reaches the foreign repo (labels actually applied there).
4. Keep this read-mostly: the aim is to confirm ghwf *operates on* the foreign
   repo correctly, not to run a full plan→implement→merge cycle there. If a
   full session proves necessary to surface anything, that's a candidate to
   scope down per the user's latitude.
5. **Clean up:** leave the throwaway repo for now (cheap to reuse for future
   smoke tests) or delete it — note the choice in the log.

**Pass criteria:** foreign issue accepted, branch prefixed, labels applied on
the foreign repo; an un-listed repo is rejected.

## Path 3 — session lease / crash-recovery (#82)

A lease is stale when its pid is dead *or* its heartbeat is older than
`LEASE_TTL` (120 s) — `is_live()` / `lease_is_stale()` in `src/state.rs`.
`acquire_lease()` reclaims a stale lease via an exclusive create;
`reset_if_abandoned()` (`src/resync.rs`) flips a crash-signatured issue back to
`waiting-on-user`. Lease mechanics are well unit-tested with stale-file
manipulation; a real worker crashing and a second reclaiming has not been done
end-to-end.

**Exercise (scriptable — pid-death makes a lease stale immediately, no 120 s
wait needed):**

1. Start a `ghwf forever` worker on a throwaway issue so it acquires a real
   lease, then `kill -9` it to simulate a crash, leaving the lease behind.
2. Confirm the abandoned lease is stale (`lease_is_stale` semantics) and that a
   fresh `ghwf forever` / `ghwf next` reclaims the issue rather than skipping
   it.
3. Confirm `ghwf resync` (`sweep` → `reset_if_abandoned`) flips the crashed
   issue's attention/labels back to `waiting-on-user`.
4. Use a real throwaway issue (e.g. the one from Path 1, or the smoke-test repo)
   and restore its state afterwards.

**Pass criteria:** a killed worker's lease is treated as stale, the issue is
reclaimable, and `resync` resets the crash-abandoned labels.

**Scope-down note:** genuine two-worker concurrent-reclaim contention is hard to
stage deterministically; if it proves fiddly, asserting single-worker reclaim +
`resync` reset is an acceptable reduced scope.

## Path 4 — double-SIGINT shutdown gesture (#77)

`shutdown()` / `signal_child()` and the 400 ms double-Ctrl-C → SIGTERM → SIGKILL
escalation (`src/launch.rs`) are tested only against a `sleep` child. The #77
worry is specifically a *real* Claude session: does it shut down cleanly, and is
the terminal left cooked (not raw)?

**This one needs a human at a TTY** — I can't drive interactive Ctrl-C or
observe terminal mode non-interactively. Plan:

1. I write a precise manual test script + observation checklist into the
   smoke-test log: run `ghwf forever` (or a session) in a real terminal, press
   Ctrl-C twice within the gap, observe (a) the session shuts down, (b) the
   terminal is usable afterwards (`stty` sane / no raw-mode residue), (c) no
   orphaned child process.
2. The user runs it and reports back; I fix anything surfaced and record the
   outcome.

**Pass criteria:** double-Ctrl-C brings the real session down and leaves the
terminal in a sane state, per the user's manual run.

## Out of scope / deferrals

- No CI integration-test harness is being added — these are one-shot live
  exercises, as the ticket scopes them.
- Bugs that surface and are large or independent get filed as follow-up issues
  rather than expanding this PR.

## Done when

- Paths 1–3 have been exercised live with results recorded in
  `docs/smoke-tests/112-live-paths.md`.
- Path 4 has a manual test script ready for the user (and is checked off once
  they run it).
- Any surfaced fixes are committed here or filed as follow-ups, and throwaway
  test artifacts are cleaned up (or their retention noted).
