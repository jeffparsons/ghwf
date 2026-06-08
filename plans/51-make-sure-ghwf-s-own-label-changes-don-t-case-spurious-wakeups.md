# Plan: stop ghwf's own label changes from causing spurious wakeups

Issue #51.

## Problem

`ghwf wait` returns "new activity" (exit 0) immediately after `ghwf
work-on` arms it, woken by ghwf's *own* metadata mutations rather than any
user activity. Observed live during this issue's own pre-plan loop:

```
The issue was labeled (via the events feed).
The issue was unlabeled (via the events feed).
The issue was assigned (via the events feed).
```

## Root cause

The feed-mode handler for `IssuesEvent` in `feed_wake_reasons`
(`src/wait.rs:658-660`) is a catch-all that wakes on *any* issue action:

```rust
"IssuesEvent" if ours(&event.payload.issue) => {
    let action = event.payload.action.as_deref().unwrap_or("changed");
    reasons.push(format!("The issue was {action} (via the events feed)."));
}
```

GitHub emits an `IssuesEvent` for every label add/remove (`labeled` /
`unlabeled`) and every assignment (`assigned` / `unassigned`), plus pins,
milestones, locks, etc. ghwf performs two of these itself:

- `labels::sync()` (`src/labels.rs`) mirrors workflow phase/attention onto
  GitHub labels — `add_issue_labels` / `remove_issue_label`.
- `next.rs:212-213` self-assigns the issue to the claiming user via
  `github::add_assignee`.

Both land in the events feed as `IssuesEvent`s and trip the catch-all.

The timing makes the label case fire on essentially every cycle: in
`work-on`'s teardown (`src/main.rs:710-713`) the wait baseline (the `since`
watermark) is recorded *before* `labels::sync()` runs, so ghwf's own label
events are always newer than the watermark and read as fresh activity.

This only affects the feed path. Direct-mode polling fingerprints just
title/body/state (`state::issue_fingerprint`, used at `src/wait.rs:373-377`),
so label-only and assignment-only changes never wake through it. Label sync
only runs for users who enable the optional `[labels]` config, but the
self-assignment path is unconditional.

## Fix

Make the feed `IssuesEvent` arm wake on exactly the issue changes that
direct mode already treats as wakes — title/body/state — and ignore all
metadata-only actions. The actions that change title/body/state are
`edited`, `closed`, and `reopened`. Everything else (`labeled`, `unlabeled`,
`assigned`, `unassigned`, `pinned`, `milestoned`, `locked`, …) is metadata
noise that direct mode ignores, so the feed arm should ignore it too.

This converts the catch-all into an explicit allowlist with a default
`continue`, mirroring the `PullRequestEvent` arm immediately below it
(`src/wait.rs:664-686`), which already uses exactly this pattern.

### Code change — `src/wait.rs`, the `IssuesEvent` arm (~lines 658-660)

Replace the catch-all with:

```rust
// Only issue changes that direct mode also treats as wakes —
// title/body (`edited`) and state (`closed`/`reopened`). Metadata-only
// actions (labeled/unlabeled, assigned/unassigned, pinned, milestoned,
// locked, …) are noise; ghwf makes some of them itself (label sync,
// self-assignment) and must not wake on them.
"IssuesEvent" if ours(&event.payload.issue) => {
    match event.payload.action.as_deref() {
        Some("closed") => reasons
            .push("The issue was closed (via the events feed).".to_string()),
        Some("reopened") => reasons
            .push("The issue was reopened (via the events feed).".to_string()),
        Some("edited") => reasons
            .push("The issue was edited (via the events feed).".to_string()),
        _ => continue,
    }
}
```

Keep the existing comment block above the `PullRequestEvent` arm as-is; add
the short rationale comment shown above to the `IssuesEvent` arm.

## Tests — `src/wait.rs` test module

- Keep `feed_wakes_on_issue_state_event` (uses `action: "closed"`) — it
  still passes under the allowlist.
- Add a test that `labeled`, `unlabeled`, and `assigned` `IssuesEvent`s on
  our issue produce **no** wake reasons (the regression guard for this
  issue). Build `FeedEvent`s the same way the existing test does, with
  `created_at` after `SINCE`.
- Add a test that `edited` and `reopened` actions **do** wake, to lock in
  the allowlist's positive cases.

Run `cargo test` and `cargo clippy` to confirm green.

## Non-goals / notes

- Not changing direct-mode polling — it already ignores labels and
  assignment correctly.
- Not reordering the baseline-vs-`labels::sync()` calls in `main.rs`.
  Reordering alone wouldn't help (label events arriving mid-wait would
  still trip the catch-all), and the allowlist fix makes ordering
  irrelevant. Leaving it avoids churn.
- Not filtering by actor: ghwf authenticates as the user's own `gh` token,
  so its label/assignment edits are indistinguishable from the user's by
  actor. The allowlist is the robust approach and is also future-proof
  against any new noisy `IssuesEvent` action types GitHub adds.
- `transferred` / `deleted` are deliberately *not* in the allowlist: they're
  rare, out of scope for this issue, and the direct-mode 404 handling for a
  vanished issue is a separate concern.

## Verification

After the change, an end-to-end `ghwf wait` following a `work-on` cycle that
syncs labels and self-assigns should no longer wake on those events; only
real comments, reactions, PR state changes, and genuine issue
edits/close/reopen should.
