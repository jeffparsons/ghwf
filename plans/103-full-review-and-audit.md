# Plan for #103: Full review and audit

## Goal

Issue #103 asked for a full review and audit of the codebase to surface
"opportunities for future excellence", to be presented for selection and then
turned into tickets. A five-dimension audit (robustness/concurrency,
architecture/maintainability, test coverage, CLI/UX & docs, GitHub-API/security)
was run and presented as a checklist on the issue; Jeff selected all 16 items.

This plan is the list of tickets to file. The "implementation" of this issue is
**filing these 16 follow-up issues** with `ghwf create-issue` — no source change
ships on this branch beyond this plan file.

All 16 were deduped against the already-open issues #104 and #107–#112; none
overlaps those.

## How they'll be filed

- **One issue per item, 16 total** — kept as listed rather than bundled.
- **Standalone** (`--no-block`): each is immediately workable, not blocked on
  #103.
- **Cross-linked, not hard-blocked**: closely-related tickets reference each
  other in their bodies (#1↔#2, #13↔#14, #15↔#16) but carry no `blocked_by`
  dependency, so they can be picked up in any order.
- Each body carries: a problem statement, the concrete `file:line` references
  from the audit, and a severity/value · effort tag.
- The three **minor** items are folded into the nearest ticket rather than
  ticketed separately (noted inline below).

## The 16 tickets

### State durability & concurrency

1. **Make state persistence atomic and race-safe** — *HIGH · small*
   `state::save`/`seen::save` use a non-atomic `fs::write` (`state.rs:666`,
   `seen.rs:63`) while `write_lease` already uses temp+rename
   (`state.rs:918-925`). Two consequences: (a) in-session `work-on`
   (`main.rs:937`, `:1217`) and the Stop hook (`stop_hook.rs:53-54`) both do a
   full read-modify-write of the same `<n>.json` and can clobber each other (a
   consumed approval can re-fire; the wait baseline is dropped); (b) a crash
   mid-write leaves a torn file that `issue_status` treats as permanently `Live`
   (`next.rs:248-263`), silently stranding the issue. Switch all state
   persistence to atomic temp+rename, and narrow/serialise the Stop hook's
   `stop_nudges` update so it can't rewrite the whole struct. Cross-link #2.

2. **Consolidate `work_on` to a single durable save; checkpoint directive
   consumption** — *HIGH · medium*
   `advance_phase` marks directives/reactions consumed and bumps the phase in
   memory (`main.rs:1561`, `:1577`, `:1601`); the transition is announced by a
   best-effort `post_status` (`main.rs:878-935`, `:1253-1258`) and only made
   durable at `main.rs:937`, with more fallible `?` calls before the final save
   at `:1217` (`fetch_comments` `:990`, `fetch_review_comments` `:992`,
   `seen::save` `:1202`). A failure in those windows re-fires a directive /
   re-announces a transition, or leaves the phase advanced but
   labels/wait-baseline inconsistent. Consolidate to one end-of-run save (or
   checkpoint consumption only once the transition's durable artifact is
   confirmed). Cross-link #1.

3. **Record an explicit session→issue pointer instead of inferring by mtime** —
   *MED · medium*
   The Stop/Notification hooks resolve a session's issue by scanning for the
   `worktree_session_id` match with the newest `mtime`
   (`session_binding.rs:49-56`), but every `state::save` bumps mtime and one
   session can have driven several issues. A background save (or the periodic GC
   stamp) on issue B can bind a hook fired for issue A to B. Record a definitive
   "current issue for this session" pointer at launch (e.g.
   `sessions/<id>/current`) and resolve from it.

### GitHub API robustness

4. **Add a timeout to every `gh` subprocess call** — *MED-HIGH · medium*
   None of the wrappers — `gh`/`gh_capture` (`github.rs:699`, `:716`),
   `gh_api`/`gh_api_stdin`/`gh_api_capture_stdin` (`:937`, `:954`, `:986`),
   `gh_api_conditional` (`:833`) — sets a timeout, so a stalled connection hangs
   ghwf (and an unattended `wait` loop, whose deadline only paces *between*
   calls) indefinitely. Add a per-call timeout that kills the child and feeds
   the existing `MAX_CONSECUTIVE_FAILURES` backoff as a transient failure.

5. **Retry/backoff + rate-limit recognition for the write/one-shot paths** —
   *MED · medium*
   `next`/`wait` back off carefully, but every write and one-shot read bails on
   the first non-zero `gh` exit (`github.rs:699`, `:937`, `:954`) with no retry,
   and `is_rate_limited` (`github.rs:879`) only classifies
   `gh_api_conditional` errors — a 403/429 from a plain write is an opaque bail.
   A transient blip during `create_draft_pr`/`push`/`add_assignee`/label sync
   aborts the whole run. Add a shared retry-with-backoff wrapper honouring
   `Retry-After`/rate-limit signals. Also: `gh_api_conditional` ignores
   `output.status` and can misclassify a truncated body as a short `Fresh`
   response (`github.rs:824-860`) — cross-check the exit status when the body
   fails to parse.

6. **Paginate the reaction-watch endpoint in `wait`** — *MED · medium*
   Conditional GETs request `per_page=100` but `gh_api_conditional` issues a
   single un-paginated `gh api -i` (`github.rs:824`); the reaction-watch poll
   (`wait.rs:418`) can therefore miss a genuine 👍 approval beyond the first 100
   reactions on a busy prompt, stalling the workflow with no error. The comment
   polls self-heal (any accepted comment re-baselines `since`); the reaction
   poll has no such backstop. Paginate it (or scan for the newest reactions).
   Every list-fetch in `github.rs` already paginates — this is the one gap.

### Trust / security model

7. **Fence CI-log output as untrusted in the digest** — *HIGH · small*
   `failed_run_logs` (`github.rs:744-801`) concatenates `gh run view
   --log-failed` output verbatim into the session with only a `=====` header
   (`:782`) — no access gate, no fencing — and the header's `workflow_name` is
   itself attacker-influenceable. On a public repo a fork-PR author who can make
   a job print text injects straight into the trusted-looking stream. Fence CI
   logs in a clearly-delimited, explicitly-untrusted block consistent with
   `onboarding.rs`'s "output from other tools" boundary.

8. **Require a session token for the status hide-marker** — *MED · small*
   `hidden_from_digest` (`render.rs:128`) drops any comment containing the fixed
   `STATUS_MARKER = "<!-- ghwf:v1 status -->"` (`render.rs:13`,`:114`,`:130`)
   regardless of author or token, and the check runs *before* the `AccessList`
   gate in both surfacing paths (`main.rs:~1278` before `:1286`; `wait.rs:602`
   before `:607`). So anyone — including a stranger on a public repo — can paste
   that string to mute their comment from the digest *and* the wake gate
   (suppression/evasion, not injection). Require the session token for the hide
   marker, or run the hide check after the access gate.

### CLI / observability

9. **`ghwf status` — list all in-flight issues** — *MED · medium (high value)*
   No way to ask "what is ghwf doing across all my issues?" An aggregated view
   (phase, attention axis, live-vs-stopped lease, worktree path, any
   `session_alert`) is the highest-value UX addition for a phone-plus-terminal
   operator running pools. The iteration helpers exist: `state::all_issue_states`
   (`state.rs:544`), `lease_liveness` (`state.rs:775`), `is_concluded`
   (`state.rs:315`). **Folds in the two minor doc/terminology items**: a
   terminology-consistency pass ("needs you" / "concluded" / "finished" /
   "complete" / "halted") so the status view and notices use one term per
   concept, and a note to point the README at `ghwf config example` as the
   canonical annotated config (the README's hand-maintained `ghwf.toml` omits the
   `[labels]` table — `README.md:319-393`).

10. **`ghwf reset <issue>` — clear/abort a wedged issue** — *MED · medium*
    Two error messages tell the user to "clear the issue's ghwf state"
    (`launch.rs:181`) but no command does it — they must hand-delete JSON under
    an opaque data dir. `state::delete` exists (`state.rs:603`) but is only wired
    into GC. Add `ghwf reset <issue>` (clear state; optionally remove the
    worktree; with confirmation/`--dry-run`). **Folds in the minor first-run
    preflight item**: a launcher check that the `/work-on` skill is installed
    before spawning `claude /work-on` (`main.rs:601`), so the most common
    first-run failure surfaces clearly.

11. **`next --explain` — surface per-issue skip reasons** — *LOW-MED · small*
    When `next` picks nothing the user gets one flat sentence (`next.rs:51`); the
    per-issue skip buckets (`skipped_blocked`, `skipped_tracking`,
    `skipped_unlisted_author`, `skipped_live`) are computed but discarded.
    Surface each open issue's disposition (blocked by #N / tracking / author not
    allow-listed / live elsewhere) so "why won't a worker grab #42?" is
    answerable — via the empty-result message and/or a `--explain`/`--dry-run`
    flag.

12. **Opt-in event log for unattended `forever`/pool workers** — *MED · medium*
    The supervisor/worker path logs transient failures with `eprintln!`
    (`next.rs:195`, `:137`; recovery/poll paths) to a stderr scrollback nobody
    watching from a phone can see, and a repeatedly-failing *launch* never
    reaches GitHub (only session-stuck/recovery-exhausted do, via
    `launch.rs:830`,`:844`). Add an opt-in append-only event log
    (`--log <path>` / config), surfaced by the `ghwf status` command (#9).

### Testability & architecture (longer-horizon)

13. **Introduce a `gh` boundary seam to unlock end-to-end tests** —
    *MED-HIGH · medium*
    All network goes through a few private `Command::new("gh")` wrappers
    (`github.rs:936-1010`, `:699`) with no injection point, so no command
    handler (`next`, `wait`, label sync, `collect-garbage`) can be tested against
    canned responses; tests inject closures ad hoc per function instead. Add a
    thin `GhClient` trait (or function-pointer indirection) behind the wrappers.
    Unlocks #4/#5/#6 testing and #14. Cross-link #14.

14. **Low-effort test wins on zero-coverage logic** — *LOW · small*
    Cover currently-untested, bug-prone logic cheaply: JSON parse fixtures for
    `fetch_pr`/`branch_prs`/`fetch_review_comments`/`failed_run_logs` and the
    `parse_sha`/`parse_object_sha`/`parse_tree_sha` helpers (`github.rs`,
    `models.rs`); extract a pure `label_diff(current, desired, configured)` from
    `labels::sync_thread` (`labels.rs:77-103`) and test it; round-trip tests for
    `store` token hashing (`store.rs:60-94`) and `session_binding`
    (`session_binding.rs`). Cross-link #13.

15. **Decompose `work_on`; re-home pure helpers out of `main.rs`** —
    *arch · large*
    `work_on` (`main.rs:601-1220`) is one ~620-line function (fetch + approvals +
    phase body + status posting + wait baseline + digest + label sync), and
    `main.rs` is 3139 lines — it also holds nine in-session command bodies and
    pure, already-tested helpers that belong in focused modules (the approval
    machinery `main.rs:1420-1652`, options scanning `:1308-1366`,
    `collect_new_comments` `:1268-1301`). Split `work_on` into named steps and
    move the pure helpers to `approvals`/`options`/`digest` modules. Several open
    issues (#107/#108/#110/#111) all need to touch this function. Cross-link #16.

16. **Introduce a GitHub client / `RepoCtx` type to kill tuple-threading** —
    *arch · large*
    `owner`/`repo`/`code_owner`/`code_repo` `(String, String)` tuples are
    threaded through nearly every signature in the codebase (`github.rs` is ~40
    free functions; `code_repo`/`config_repo` are re-invoked ad hoc throughout
    `main.rs`/`launch.rs`/`prep.rs`). A lightweight `RepoCtx`/`GitHub` bundling
    issue-repo + code-repo would remove most signature-noise and make the
    issue-repo-vs-code-repo distinction a typed thing rather than a convention.
    Cross-link #15.

## Acceptance

- 16 issues filed via `ghwf create-issue --no-branch`, one per item above, each
  with the problem statement, file:line refs, and severity/effort tag.
- Related tickets cross-reference each other by the issue numbers GitHub assigns.
- A summary comment on #103 listing the filed issue numbers.
