# Live smoke tests — write/shutdown paths (#112)

These paths shipped on unit-test confidence, with their live side effects
self-flagged as unexercised in the #97 history audit. This is the record of
deliberately exercising them once against live GitHub / real state, per #112.

Run on 2026-06-15 against `jeffparsons/ghwf` (with `gh` authenticated as the
`jeffatstile` collaborator account). Each result quotes the live evidence.

| Path | Status |
|------|--------|
| #58 — `create-issue` + native `blocked_by` POST | ✅ verified live |
| #69 — cross-repo `issue_repos` allowlist + endpoint | ✅ verified live (allowlist both directions); foreign-repo *writes* scoped down |
| #82 — session lease / crash-recovery (detection + reset) | ✅ verified live; multi-worker reclaim scoped down |
| #77 — double-SIGINT shutdown gesture | ⏭️ skipped (needs a human at a real TTY) — per the user, not worth it for now |

---

## #58 — `create-issue` + native `blocked_by` dependency POST

`create_issue()` (`src/main.rs`) → `add_blocked_by()` (`src/github.rs`) POSTs to
`repos/{owner}/{repo}/issues/{n}/dependencies/blocked_by`. Only label-assembly
and arg-resolution were unit-tested; the live POST had never run.

**Blocking case.** Filed a throwaway issue blocked by #112:

```
$ ghwf create-issue --title "ghwf #112 smoke: blocked_by probe" 112
→ created #150
$ gh api repos/jeffparsons/ghwf/issues/150/dependencies/blocked_by
  → [{ number: 112, state: open, title: "Smoke-test the live write/shutdown paths…" }]
$ gh api repos/jeffparsons/ghwf/issues/150 --jq '.labels[].name'
  → (none)
```

The native dependency landed and is visible through the dependencies endpoint
(not just the guard label). The guard label was correctly peeled back off after
the POST succeeded, exactly as designed.

**Standalone case.** `--no-block` filed #151 with **0** dependencies and **0**
labels — a clean standalone issue.

Both throwaway issues (#150, #151) were closed afterwards.

**Verdict: pass.** No fixes needed.

---

## #69 — cross-repo `issue_repos`

`validate_issue_url_against_issue_repos()` / `url_issue_endpoint()`
(`src/github.rs`) gate a foreign issue against the `issue_repos` allowlist and
build its `gh api` endpoint. Config parsing, URL validation, and branch naming
were unit-tested; no live foreign-repo run had happened.

A private throwaway repo `jeffatstile/ghwf-smoke-test` was created as the
foreign repo (`gh` is authenticated as `jeffatstile`, so it landed under that
account rather than `jeffparsons`; still a genuine foreign repo relative to the
configured `jeffparsons/ghwf`).

**Reject (live, read-only).** From the real config (no `issue_repos`), resolving
a foreign issue URL is refused at the allowlist gate before any network write:

```
$ ghwf wait https://github.com/jeffatstile/ghwf-smoke-test/issues/1
Error: issue URL points at jeffatstile/ghwf-smoke-test, but ghwf.toml configures
jeffparsons/ghwf.
… add it to `issue_repos` in ghwf.toml:
    issue_repos = ["jeffatstile/ghwf-smoke-test"]
```

The guidance names the exact repo and the exact knob — correct.

**Accept (live, read-only).** With a scratch config allowlisting the repo
(`issue_repos = ["jeffatstile/ghwf-smoke-test"]`), the same resolve passes the
gate and builds the foreign endpoint — it fails only on the *fetch* (issue #1
doesn't exist), which is past validation:

```
$ ghwf wait https://github.com/jeffatstile/ghwf-smoke-test/issues/1   # scratch cfg
Error: `gh api repos/jeffatstile/ghwf-smoke-test/issues/1` failed: gh: Not Found (HTTP 404)
```

No `issue_repos` guidance error → the allowlist accepted the foreign repo and
ghwf addressed the correct `repos/jeffatstile/ghwf-smoke-test/issues/1`
endpoint.

**Scoped down (per "nice-to-have"):** the foreign-repo *write* paths — posting a
comment to a foreign issue, label sync onto a foreign repo, and a full
plan→implement session driving a foreign issue (branch-prefix collision
avoidance) — were **not** auto-exercised. Two reasons: (1) the session's
auto-permission classifier blocks external-org writes (it can't see the GitHub
plan-approval as consent), and (2) a full session needs an interactive Claude
child. The write *mechanism* (`gh`-shell-out issue/label writes) is the same one
exercised live by #58 and by #82's label sync below, so the residual risk is
low. Left for a future live pass if wanted.

**Verdict: pass** on the cross-repo plumbing (allowlist both ways + endpoint
construction). Foreign-repo writes deferred. No fixes needed.

*Note:* one cosmetic observation — the reject error printed a full Rust stack
backtrace, but that was `RUST_BACKTRACE` being set in the launcher environment,
not a ghwf behaviour (re-running with `RUST_BACKTRACE=0` printed just the clean
message). Not a bug.

---

## #82 — session lease / crash-recovery

A lease is stale when its pid is dead **or** its heartbeat is older than
`LEASE_TTL` (120 s): `is_live()` / `lease_is_stale()` (`src/state.rs`).
`resync::sweep()` → `reset_if_abandoned()` (`src/resync.rs`), invoked at the top
of every `ghwf work-on`, flips a crash-signatured sibling back to
`waiting-on-user` and re-syncs its labels (#110). Lease mechanics were
unit-tested with stale-file manipulation; a crash-recovery sweep had not been
run live.

A crashed worker leaves behind exactly a `{pid, heartbeat}` lease file (the
process is gone, so its `Drop` cleanup never runs) — so a hand-written dead-pid
lease is a faithful stand-in for a real crash. Setup, against a throwaway issue
#152 in the real repo, initially labelled `ghwf:claude-working` +
`ghwf:implementing`:

- `…/issues/jeffparsons/ghwf/152.json` — `phase: implement`, `attention:
  waiting-on-claude`, unconcluded.
- `…/issues/jeffparsons/ghwf/152.lease.json` — `{"pid":2147483647,
  "heartbeat":1000}` (pid 2147483647 = no such process → stale).

(First confirmed no other issue could be affected: only `112.lease.json` existed
in the data dir, and it's this live session — so the sweep had exactly one
crash-abandoned target.)

**Sweep (live).** `ghwf work-on 112` ran the sweep:

```
Reset #152 to needs-you — its session looks to have crashed.
$ gh api repos/jeffparsons/ghwf/issues/152 --jq '.labels[].name'
  → ghwf:implementing
    ghwf:needs-you          # was ghwf:claude-working — flipped live
# 152.json attention is now "waiting-on-user"
```

So crash **detection** (stale dead-pid lease + machine attention) and
**recovery** (live label flip `claude-working`→`needs-you`, phase label kept,
state persisted) both work end-to-end. This also exercises `labels::sync` live —
the same code #69's cross-repo label sync uses.

**Idempotency (live).** A second `ghwf work-on 112` did **not** re-reset #152
(its attention is now `waiting-on-user`, so the gate declines) — the sweep is
idempotent, as intended.

Manufactured state/lease removed and #152 closed afterwards.

**Scoped down:** a genuinely concurrent two-worker reclaim race (`acquire_lease`
losing/winning the exclusive create) and the live heartbeat-thread lifecycle
were not staged — they need spawning real worker processes/sessions. Those
remain unit-test-only; the detection + recovery half is now verified live.

**Verdict: pass** on crash detection + recovery. No fixes needed.

---

## #77 — double-SIGINT shutdown gesture

Skipped per the user (2026-06-15): "Let's skip all the stuff that needs my manual
intervention; not worth it for now." Verifying that a *real* Claude session
responds to the 400 ms double-Ctrl-C and leaves the terminal cooked (not raw)
fundamentally needs a human at an interactive TTY — it can't be driven
non-interactively. `shutdown()`/`signal_child()` remain covered only by the
`sleep`-child unit test (`src/launch.rs`). Left for a future manual pass.

---

## Summary

Three of the four flagged paths were exercised live and all behaved correctly —
no behaviour bugs surfaced, so no code fixes were needed. The remaining live
gaps (foreign-repo *writes* for #69, multi-worker reclaim for #82, and the #77
real-session shutdown) are documented above as deliberately scoped down rather
than dropped.
