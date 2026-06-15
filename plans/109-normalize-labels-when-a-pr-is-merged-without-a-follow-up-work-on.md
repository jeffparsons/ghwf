# Plan: Normalize labels when a PR is merged without a follow-up `work-on`

## Problem

`labels::sync` only ever runs from inside `work-on`. In the normal loop a merge
is caught by `wait` (which returns exit 0), and the *next* `work-on` then sets
`phase = Finished` (`src/main.rs:726`) and syncs labels to `ghwf:finished`. The
gap is when no `work-on` follows the detection — the loop has been stopped, the
supervisor already brought the session down, or the session dies between `wait`
and `work-on`. `wait` (`src/wait.rs`) only logs `"The PR was merged."` and
returns; it never touches `phase`, `pr_outcome`, or labels, so `ghwf:review` /
`ghwf:needs-you` linger indefinitely.

The same lingering applies to a PR **closed without merging**: that also
concludes the workflow (`pr_outcome` becomes `Some`), so its attention label
(`ghwf:needs-you`) should come off too.

## Approach

Make the merge/close-detection point in `wait` self-sufficient: when `wait`
detects the PR has left the open state, normalize the labels itself before
returning, in addition to the existing wake.

This lands here (in `wait.rs`), not in #99. #99's periodic GC
(`collect_garbage`) is branch/worktree cleanup and is itself only triggered from
`work-on` on a fresh merge, so it wouldn't close this gap. The truly-unattended
case (a merge while *nothing* — neither `wait` nor `work-on` — is running)
genuinely belongs to a periodic sweep and is left to #99.

### Key constraint: don't suppress `work-on`'s once-per-merge side effects

`work-on` gates three once-per-merge actions on `new_conclusion`, computed by
comparing the *saved* `pr_outcome` against the freshly fetched one
(`src/main.rs:663-667`):

- the merge announcement / `concluded_body`,
- `update_main_worktree_after_merge` (`src/main.rs:786-788`),
- `collect_garbage::run_periodic` (`src/main.rs:791`).

If `wait` persisted `pr_outcome = Merged` into the state file, a following
`work-on` (the normal loop) would see `new_conclusion == None` and skip all
three. That's a regression. So `wait` must normalize the **labels** without
persisting a `pr_outcome`/`phase` change that hides the conclusion from the next
`work-on`.

The mechanism: a dedicated label entry point that computes the concluded
desired-label set directly (without reading or mutating `state.phase` /
`state.pr_outcome`), but still records `state.labels_synced`. Because it records
exactly the `LabelSyncRecord` that `work-on`'s own end-of-run `labels::sync`
would compute after the conclusion, the follow-up `work-on` sync no-ops (no
duplicate API calls), while `new_conclusion` still fires (saved `pr_outcome`
left untouched). If `work-on` never runs, the labels are already correct; the
stale `phase`/`pr_outcome` in the state file self-heals on any later `work-on`
(which always recomputes them from the live PR).

## Changes

### 1. `src/labels.rs` — a conclusion-aware sync entry point

Add `pub fn sync_concluded(issue_repo, code_repo, number, pr_number, conclusion:
PrOutcome, state: &mut IssueState)`.

- Load config / `[labels]` exactly as `sync` does; no-op when unconfigured.
- Compute the concluded phase: `Phase::Finished` for `Merged`; `state.phase`
  (left as-is) for `Closed`. Attention is `None` (a concluded workflow waits on
  nobody — mirrors `sync`'s `pr_outcome.is_none().then_some(...)`).
- Build `LabelSyncRecord { phase, attention: None, pr_number }`; if
  `state.labels_synced == Some(record)`, return (idempotency guard).
- Apply to each thread via the existing `sync_thread(&cfg, owner, repo, thread,
  phase, None)`; on full success, set `state.labels_synced = Some(record)`.

This reuses `sync_thread` / `desired_labels` unchanged. Factor the shared
config-load + per-thread-apply body out of `sync` so both call sites share it,
rather than duplicating.

Verify the record it writes matches what `work-on` writes:
- **Merged:** `work-on` sets `phase = Finished`, `pr_outcome = Some` ⇒ record
  `{Finished, None, pr_number}`. `sync_concluded(Merged)` writes the same. ✓
- **Closed:** `work-on` keeps `phase`, `pr_outcome = Some` ⇒ record
  `{phase, None, pr_number}`. `sync_concluded(Closed)` writes the same. ✓

### 2. `src/wait.rs` — surface the conclusion and normalize on it

Thread the detected outcome up to the return sites:

- Add `conclusion: Option<state::PrOutcome>` to `CycleOutcome`.
- `evaluate_fresh`: add a `conclusion: &mut Option<PrOutcome>` out-param; set it
  in the `PrObject` arm (`Merged` / `Closed`) alongside the existing reason.
  `direct_cycle` passes `&mut outcome.conclusion`.
- `feed_wake_reasons`: add a `conclusion: &mut Option<PrOutcome>` out-param; set
  it in the `PullRequestEvent` `"closed"` arm (merged vs. closed). Update its two
  real callers (`enter_feed_mode`, `feed_cycle`).
- `FeedEntry::Wake(Vec<String>)` → `FeedEntry::Wake { reasons, conclusion }` so
  the feed-entry wake carries it.

At each return-on-activity site in `run`, before `persist`, if
`outcome.conclusion` (resp. the `Wake` conclusion) is `Some`, call
`labels::sync_concluded(...)`. The two sites:

- the main cycle match arm (`src/wait.rs:183-188`),
- the `FeedEntry::Wake` arm of the feed handover (`src/wait.rs:232-237`).

`persist` already saves the whole `issue_state` (including the updated
`labels_synced`) and the wait state. `sync_concluded` is best-effort like all
label work (its internals already warn-not-error); it leaves `pr_outcome` /
`phase` untouched in `state`.

(The reactions-only sub-poll inside feed mode — `src/wait.rs:166` — runs over
`watch_endpoints`, which contain no PR object, so its `conclusion` is always
`None`; nothing to merge.)

## Tests

`src/labels.rs`:
- `sync_concluded` desired-label sets: merged ⇒ `{ghwf:finished}` only; closed
  from a given phase ⇒ that phase's label only (attention dropped). (Reuse the
  existing `config_with_labels` helper; assert via `desired_labels`, and/or a
  focused test that the `LabelSyncRecord` matches the merged/closed shapes.)

`src/wait.rs`:
- `evaluate_fresh` over a merged PR object sets `conclusion == Some(Merged)`;
  closed-without-merge ⇒ `Some(Closed)`; open ⇒ `None`. (Extend the existing
  `pr_object_reasons` helper to also return/assert the conclusion.)
- `feed_wake_reasons` sets `conclusion` to `Merged` / `Closed` on the matching
  `closed` PR event, and `None` for non-conclusion events. (Update existing
  `feed_wake_reasons` call sites for the new out-param.)

Update the existing `evaluate_fresh` / `feed_wake_reasons` test helpers and call
sites for the new out-params (mechanical).

## Out of scope

- The fully-unattended sweep (merge while neither `wait` nor `work-on` runs) —
  belongs to #99's periodic GC.
- Issue-closed-without-a-PR label normalization — not raised by this issue;
  leave for a separate change if it proves to matter.
- No new config option (this is always-on, best-effort label hygiene), so the
  `CLAUDE.md` "Adding a config option" checklist does not apply.
