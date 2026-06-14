# Plan — #108: Acknowledge an approval directive/reaction immediately when it registers

## Goal

When an allow-listed user posts an approval directive (or 👍s a prompt), give an
instant, lightweight acknowledgement *at the moment the event is consumed* —
before the slow phase body runs — so the user knows it landed and doesn't
re-post the "no feedback, so repeat it" way the #97 audit found (`/proceed` ×3–4
on #1/#3, `/approve-implementation` ×2 on #27).

Mechanism (locked in during pre-plan, approved by 👍):
- **Typed directive comment** → react 👀 (`eyes`) directly on that comment.
- **👍 reaction approval** → can't be reacted-to (you can't react to a
  reaction), so flip the `claude-working` attention label *early* instead.
- Fire for **any recognised directive/👍 from an allow-listed user**, including
  stale/premature/retired misfires (`/proceed`, `/approve-implementation`) —
  those are exactly the spammed cases, and a "seen it" signal is useful whether
  or not the event advanced a phase.

## Current behaviour (baseline)

- `advance_phase` (`src/main.rs:1523`) consumes each directive (`consumed_directives.insert`,
  `main.rs:1582`) and each 👍 (`consumed_reactions.insert`, `main.rs:1598`) in
  memory. It is deliberately **network-free** (see the doc comment at
  `main.rs:1451`) — it only records `transitions` / `notes` / `ignored` into the
  returned `AdvanceOutcome` (`main.rs:1442`).
- The only user-visible feedback is emitted at the **end** of the `work-on` run,
  *after* the phase body has executed: the status comment (`main.rs:878`+) and
  the label sync that flips attention to `claude-working`
  (settle at `main.rs:1172`, applied by `labels::sync`).
- For `/approve-pre-plan` the phase body is `prep::run` (`main.rs:803`) —
  worktree + PR creation — so there's a long silent stretch between the approval
  and any acknowledgement. That's the gap this issue closes.
- `claude-working` is the label for `Attention::WaitingOnClaude`
  (`ghwf:claude-working`, `labels.rs:166`); `labels::sync` (`labels.rs:23`) is
  idempotent — it no-ops when `state.labels_synced` already matches the desired
  `(phase, attention, pr_number)` record (`labels.rs:50`).
- `github::fetch_comment_reactions` (`github.rs:69`) already GETs the reactions
  endpoint; there is no POST helper yet.

## Design

### 1. New GitHub helper: post a reaction

Add to `src/github.rs`, next to `fetch_comment_reactions`:

```rust
/// Add a reaction (e.g. "eyes", "+1") to a conversation comment. Issue and PR
/// conversation comments share the issue-comments id namespace, so one endpoint
/// form serves both threads. GitHub treats a repeated (user, content) reaction
/// as a no-op, so this is safe to call more than once.
pub fn add_comment_reaction(owner: &str, repo: &str, comment_id: u64, content: &str) -> Result<()> {
    let endpoint = format!("repos/{owner}/{repo}/issues/comments/{comment_id}/reactions");
    let payload = serde_json::json!({ "content": content }).to_string();
    gh_api_stdin(&["--method", "POST", &endpoint, "--input", "-"], &payload).map(|_| ())
}
```

Mirrors `post_issue_comment` (`github.rs:76`): JSON body on stdin, no shell
escaping.

### 2. Record acknowledgement targets in `AdvanceOutcome`

`advance_phase` must stay network-free, so it only *records* what to acknowledge;
the actual POST/label flip happens in the caller. Extend `AdvanceOutcome`
(`main.rs:1442`):

```rust
#[derive(Default)]
struct AdvanceOutcome {
    transitions: Vec<render::Transition>,
    notes: Vec<render::DirectiveNote>,
    ignored: Vec<render::IgnoredInput>,
    /// Directive comments consumed this run (from allow-listed authors) that
    /// should get an instant 👀 reaction — advancing or misfiring alike.
    react_targets: Vec<AckTarget>,
    /// Whether any 👍 reaction was consumed this run (advancing or misfiring),
    /// triggering the early `claude-working` label flip — a 👍 can't be
    /// reacted-to, so the label is its acknowledgement.
    thumb_consumed: bool,
}

/// A consumed directive comment to acknowledge with a reaction, plus the thread
/// it lives on so the caller reacts in the right repo.
struct AckTarget {
    source: &'static str,
    comment_id: u64,
}
```

Populate them inside `advance_phase`, *only after the allow-list check passes*
(so ignored/non-allow-listed events are never acked), at the points where the
event kind is known:

- **Directive arm** — after `access.accepts_comment` passes, just before
  returning the tuple at `main.rs:1588`:
  ```rust
  outcome.react_targets.push(AckTarget { source, comment_id: comment.id });
  ```
- **Thumb arm** — after `access.accepts_reaction` passes, just before returning
  the tuple at `main.rs:1613`:
  ```rust
  outcome.thumb_consumed = true;
  ```

Both points sit *before* the shared `approves()` branching (`main.rs:1617`), so a
recorded ack covers advancing, stale, premature, and retired events alike.
(Marker-bearing ghwf comments and unparseable bodies are skipped earlier via
`continue`, so they never reach these points.)

### 3. Perform the acks early, before the slow phase body

In `work-on`, after `advance_phase` + `advance_on_pr_ready` and the merge-stamp
block (`main.rs:719`–`728`), and *before* the `body = match ...` that runs the
phase work (`main.rs:773`), add an acknowledgement step. Both `issue_repo_ref`
and `code_repo_ref` are already in scope (`main.rs:687`–`688`).

```rust
// Acknowledge consumed approvals the instant they register, before the
// (possibly slow) phase body runs, so the user sees their directive/👍 landed
// without re-posting. Best-effort: a failed reaction warns but never breaks the
// run, and is attempted once (the event is already consumed).
for target in &outcome.react_targets {
    let (o, r) = if target.source == "PR" { &code_repo_ref } else { &issue_repo_ref };
    if let Err(err) = github::add_comment_reaction(o, r, target.comment_id, "eyes") {
        eprintln!("warning: failed to react to comment {}: {err:#}", target.comment_id);
    }
}
// A 👍 approval can't be reacted-to; flip the label to claude-working now (not
// just at end of run) as its acknowledgement. Skip for a concluded workflow
// (waits on nobody) and for Review (which settles to the user). The end-of-run
// labels::sync is idempotent, so it no-ops when this already applied it.
if outcome.thumb_consumed
    && issue_state.pr_outcome.is_none()
    && issue_state.phase != state::Phase::Review
{
    issue_state.attention = state::Attention::WaitingOnClaude;
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    labels::sync(&issue_repo_ref, &code_repo_ref, number, pr_number, &mut issue_state);
}
```

Notes:
- The early label flip mirrors the existing end-of-run settle condition
  (`main.rs:1172`–`1178`: only when `pr_outcome.is_none()`; Review → user) so the
  two never disagree. The end-of-run `labels::sync` still runs and, being
  idempotent, no-ops unless something changed later (e.g. `prep::run` opened a
  PR, changing `pr_number` in the sync record — which correctly re-syncs).
- 👀 (`eyes`) reads as "seen, acting on it" — apt even for a misfire (it means
  "I registered this", not "approved"). A retired `/proceed` getting a 👀 is
  exactly the desired "stop re-posting, it landed" signal.

## Files touched

- `src/github.rs` — add `add_comment_reaction`.
- `src/main.rs` — `AdvanceOutcome::react_targets` / `thumb_consumed` + `AckTarget`
  struct; record them in `advance_phase`; perform the early ack block in
  `work-on`.

## Testing

In the existing `advance_phase` test module (`main.rs:2500`+, which already has
`comment`, `reaction`, `thumbs`, `access_all`, `state_in` helpers):

- **Advancing directive** records a react target: extend
  `matching_directive_advances_and_consumes` (`main.rs:2569`) to assert
  `outcome.react_targets` has one entry with the comment id, and
  `thumb_consumed == false`.
- **Misfiring directive** (stale/premature/retired) still records a react target:
  a directive that produces a `note` rather than a transition also yields one
  `react_targets` entry (e.g. `/approve-pre-plan` while already in `Implement`,
  or a retired `/proceed`).
- **Advancing 👍** sets `thumb_consumed == true` and leaves `react_targets`
  empty (mirror an existing thumbs-advances test).
- **Misfiring 👍** also sets `thumb_consumed == true`.
- **Non-allow-listed** directive/👍 (use an `AccessList` that rejects) records
  *no* react target and leaves `thumb_consumed == false` — acks never fire for
  ignored input.
- **Marker-bearing / unparseable** comment: no react target.

The `github::add_comment_reaction` POST and the `work-on` ack block are thin glue
over the tested `AdvanceOutcome` fields and the already-tested idempotent
`labels::sync`; the load-bearing decisions all live in `advance_phase` and are
covered above.

## Out of scope / non-goals

- A terse ack *comment* on the thread (rejected in pre-plan: adds the very thread
  noise the issue wants less of).
- Reacting to the prompt comment for the 👍 case (the early label flip is the
  chosen acknowledgement there).
- Configurability of the reaction emoji or making the ack opt-out — keep it
  simple; 👀 + early label is the fixed behaviour.
- Retrying a failed reaction POST on a later run (the event is consumed once;
  reactions are best-effort).
