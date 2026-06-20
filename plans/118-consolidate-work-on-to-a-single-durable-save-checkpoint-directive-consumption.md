# Plan: Consolidate `work_on` to a single durable save

Issue #118. Today `work_on` (`src/main.rs`) persists `IssueState` **twice** —
an early save and an end-of-run save — with fallible work and a user-visible
GitHub post in between. The two windows the issue describes both stem from that
split. This plan collapses the two saves into one consolidated end-of-run save
and reorders the tail of `work_on` so the save is the last state-mutating step,
with only in-memory work between the status post and the save.

## Current shape (post-`advance_phase` tail of `work_on`)

- **Early save — `state::save` at `main.rs:1022`.** Commits the phase advance,
  `consumed_directives`, `consumed_reactions`, `intro_posted`, and the prep
  artifacts (`prep::run` records `worktree_path`/`branch`/`pr_number` into
  `issue_state` in memory only — it never saves itself). It sits deliberately
  **before** the worktree guard (`needs_worktree_guard` → `worktree::ensure_inside`,
  `:1026–1034`), which hard-errors when Claude isn't inside the issue's worktree.
  That ordering is load-bearing: it persists a freshly-created worktree so the
  relaunch finds it.
- **Status post — `:941–1020`** (`post_status`, which swallows failures at
  `:1356–1361`). Lands **before** the early save.
- **Final save — `state::save` at `main.rs:1320`.** Commits the wait baseline
  (`issue_state.wait`), the attention settle (`:1240`), the `session_alert`
  clear (`:1232`), and `labels_synced` (via `labels::sync`, `:1313`).

Fallible `?` calls between the two saves: `worktree::ensure_inside` (intended
hard-error), `store::session_token` (`:1036`), `fetch_comments` (`:1078`, only
when the PR opened mid-run), `fetch_review_comments` (`:1080`), `seen::save`
(`:1305`). Each `state::save` is already atomic (temp+rename); cross-file
atomicity is the companion ticket's concern, not this one.

### The two windows

- **Window B (the meatier bug).** An error between the early save (`:1022`) and
  the final save (`:1320`) leaves the phase advanced + directives consumed
  durably, but the wait baseline / attention / `session_alert` / `labels_synced`
  lost — so labels and the wait baseline are inconsistent with the advanced
  phase until the next full `work-on` self-heals them, and a `ghwf wait` in the
  interim runs off a stale/missing baseline.
- **Window A.** Dying between the status post (`:1020`) and the early save
  (`:1022`) re-fires the directive next run (not yet consumed-saved), so the
  phase re-advances and the transition is announced twice.

## Approach

Direction (A) from the issue: **one end-of-run save**, reordered so the post is
the last risky step before it. Concretely:

1. **Remove the early save at `:1022`.** There will be exactly one `state::save`.
2. **Relocate the worktree guard (`:1026–1034`) to *after* the consolidated
   save.** The guard governs only Claude's CWD for the subsequent session, not
   ghwf's own state, so running it after a fully-consistent save is safe. In the
   (rare) guard-trip case the relaunch re-runs `work-on`, but `consumed_directives`
   / `intro_posted` were saved before the guard fired, so nothing re-fires or
   re-announces. This also *improves* guard-trip consistency: labels and the wait
   baseline are now saved before the error, where today they are skipped (they
   live after the guard) and stay stale until the relaunch.
3. **Move the actual status `post_status` calls down to immediately before the
   single save**, so only in-memory work sits between the post and the save —
   shrinking window A to a negligible, fallible-call-free gap. The `status`
   value and `status_posted` flag are still computed early (the phase banner at
   `:1261` needs `status_posted`); only the network `post_status` calls and
   their three consumers move late.

This fully closes window B (single save = last state mutation) and reduces
window A to an in-memory-only gap.

### On the `last_posted` idempotency idea (correction)

In pre-plan I floated suppressing a re-post "when `last_posted` already covers
this transition." On reflection that mechanism **cannot survive the gap it
targets**: `last_posted` is written by the same save that a crash in window A
loses, so the re-run never sees it. The only way to make the announcement
exactly-once across an *arbitrary* crash is GitHub-side idempotency — scanning
already-fetched comments for a prior announcement keyed by a marker. That works
but carries real edge cases (intro-only posts, misfire *notes* that must always
post, conclusion announcements), so it is **not** in this plan. Step 3 instead
shrinks the window to in-memory-only, which removes window A in any realistic
scenario (a SIGKILL in a sub-millisecond, allocation-free window). If you want
true exactly-once announcement, I'll file it as a follow-up rather than expand
this change. **Open question A below.**

## Concrete reordering (the new tail of `work_on`)

After the phase body, `body`/`base_banner`, `config::warn_if_absent`, and the
worktree-session-id record (unchanged, through ~`:933`), the tail becomes:

1. **Fallible fetches first:** `store::session_token` (from `:1036`), and the
   PR-comments fallback + `fetch_review_comments` (from `:1072–1084`). All move
   above the post so a fetch failure aborts the run *before* anything
   user-visible or saved.
2. Build the wait baseline (`:1042–1100`) — everything except `reaction_watches`
   (which need the post output).
3. `scan_options` / submissions (`:1125–1156`) — unchanged; network but
   warn-not-`?`. Feeds `options_watches`.
4. Digest: `seen::load`, `body_changed`, `collect_new_comments`, `new_review`,
   `outcome.ignored.extend` (`:1158–1211`).
5. `activity` / `stop_nudges` / `session_alert` clear / attention settle /
   `encouraged_labels` (`:1217–1259`).
6. Compute `status` + `status_posted` and print the phase banner + digest
   (`:1261–1285`) — uses `status_posted`, not the post result.
7. `seen::save` (`:1305`) — last fallible `?` before the save; a failure aborts
   before the post for a clean, half-state-free retry.
8. `labels::sync` (from `:1313`) — moved above the post (it only reads
   `attention`/`phase`/`pr_outcome` and mutates `labels_synced`; attention is
   already settled in step 5). Best-effort, no `?`.
9. **`post_status` calls** (from `:963–1020`): post to the thread(s), set
   `posted_issue`/`posted_pr`, `last_posted`, `intro_posted`.
10. `reaction_watches` (`:1106–1118`) and `issue_state.wait = wait_state`
    (`:1310`) — in-memory, depend on the post output.
11. **The single `state::save`.**
12. **The worktree guard** (relocated from `:1026–1034`).

Between step 9 (post) and step 11 (save): only steps 10's in-memory mutations.
No `?`, no network.

## Risks / things to get right

- **`seen::save` vs. the single save.** `seen::save` stays a `?` *before* the
  state save (step 7), so a seen-write failure aborts before the post and the
  whole run retries cleanly — no half-applied transition. A crash between
  `seen::save` and the state save is the same negligible in-memory-ish gap as
  window A and equally benign (worst case: a digest entry re-shows next run).
- **Guard-trip double work.** Moving the guard to the end means the digest /
  wait / labels / seen work runs once more on the relaunch in the rare
  guard-trip path. It's all idempotent and the case is rare (the launcher
  normally starts Claude inside the worktree); the consistency win outweighs it.
- **`labels::sync` ordering.** It must stay after the attention settle (step 5)
  and before the save — preserved (step 8). Verified it doesn't read
  `issue_state.wait`.
- **Comment hygiene.** The early-save comment ("Done after saving so a
  just-created worktree is already persisted", `:1024–1025`) and the
  "runs before the save" note on `labels::sync` (`:1311–1312`) describe the old
  ordering and must be rewritten.

## Testing

- `cargo test` + `cargo clippy`.
- Targeted unit coverage in `main.rs` already exercises `advance_phase` /
  consumed-directive behavior; add/adjust a test asserting the consolidated
  single-save path leaves phase, `consumed_*`, labels-sync record, and wait
  baseline all consistent after one `work_on` (using the existing test
  scaffolding for `IssueState`).
- Manual: drive a real pre-plan → prep-and-plan transition on this very issue
  and confirm exactly one status post and consistent labels/wait afterward.

## Open questions

- **(A) Exactly-once announcement.** Accept the negligible in-memory window A
  gap (this plan), or also add GitHub-side marker dedup for a guaranteed
  exactly-once transition announcement (more code + edge cases, or a follow-up
  issue)? My recommendation: accept the gap here, file a follow-up only if you
  want the guarantee.
