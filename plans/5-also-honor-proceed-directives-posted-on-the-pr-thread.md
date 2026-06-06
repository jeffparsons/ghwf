# Plan — honor directives on the PR thread; phase-specific approvals (#5)

`advance_phase` only scans the *issue* conversation thread, so a `/proceed`
posted on the PR (natural during implement/review, where the digest surfaces
the PR thread) does nothing. This plan scans both threads, and replaces the
single overloaded `/proceed` with phase-specific approval commands.

Design decisions locked on the issue thread:

1. **Retire `/proceed`** — no generic fallback. Phase-specific approvals:
   `/approve-pre-plan` (alias `/approve-preplan`) advances pre-plan →
   prep-and-plan, `/approve-plan` advances prep-and-plan → implement,
   `/approve-implementation` advances implement → review.
2. **Both threads scanned** — the issue thread and, once `pr_number` is
   recorded, the PR conversation thread, deduped through the existing
   `consumed_directives` set (comment ids are globally unique across both).
3. **Out-of-phase directives are consumed and reported** — they can never
   mis-fire later, and the `work-on` output explains what went wrong and
   names the correct command, for Claude to relay.
4. **Prompting** — phase banners instruct Claude to end its hand-off comments
   with the exact approval command, posted on whichever threads exist (issue
   always; PR too once it exists).

## 1. Directives (`state.rs`)

Replace `PROCEED_DIRECTIVE` / `is_proceed_directive` with:

```rust
/// A workflow-advancing command a user can post as a comment.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Directive {
    ApprovePrePlan,
    ApprovePlan,
    ApproveImplementation,
    // The retired generic command; recognised so we can explain its retirement.
    Proceed,
}
```

- `Phase::approval_command(self) -> Option<&'static str>` — the canonical
  command that advances *out of* this phase (`None` for review, the terminal
  phase). Used by banners and by out-of-phase reporting.
- `Directive::approves(self) -> Option<Phase>` — the phase this directive
  approves (`None` for `Proceed`).
- `parse_directive(body: &str) -> Option<Directive>` — first match wins,
  scanning lines with the same boundary rule as today: the command must begin
  the line and be followed by end-of-line or whitespace. Commands checked:
  `/approve-pre-plan`, `/approve-preplan` (alias), `/approve-plan`,
  `/approve-implementation`, `/proceed`. (No command is a prefix of another,
  so order doesn't matter beyond the boundary rule.)
- `Phase` gains an ordering (derive `PartialOrd, Ord` — variant order is
  already workflow order) so directives can be classified as stale vs
  premature.

## 2. Advance logic (`main.rs`)

`advance_phase` takes both threads and returns a richer outcome:

```rust
struct AdvanceOutcome {
    // (from, to, command, login) when a phase transition fired.
    transition: Option<Transition>,
    // Consumed-but-not-fired directives, for banner reporting.
    notes: Vec<DirectiveNote>,
}
```

- Input: the issue comments and the PR comments (when fetched), each tagged
  with its source (`"issue"` / `"PR"`) for reporting. Merge and process in
  `created_at` order so a user can legitimately post two successive approvals
  and have both fire in one run.
- Per unconsumed, non-Claude-authored comment with a directive: insert into
  `consumed_directives`, then classify against the current phase:
  - **Matches** (`directive.approves() == Some(current)`): advance, record
    the transition (including which command and who).
  - **Stale** (approves an earlier phase — e.g. the same approval posted on
    both threads, or re-posted): consume with a mild "already past that
    phase; ignored" note.
  - **Premature** (approves a later phase): consume with a note naming the
    current phase and the command that actually advances it.
  - **Retired `/proceed`**: consume with a note that it no longer does
    anything and naming the current phase's approval command.

The transition tuple now carries the firing command so the banner can say
which directive triggered the advance.

## 3. Fetch ordering (`main.rs`)

PR comments are currently fetched only for the implement/review digest,
*after* `advance_phase`. Reorder:

- After loading state, if `prep.pr_number` is recorded, fetch the PR's
  conversation comments immediately (same `fetch_comments` call the digest
  uses — PRs share the issues comments endpoint). This covers prep-and-plan
  too, where the plan PR is open for review and `/approve-plan` on it must
  count.
- Pass them to `advance_phase`; reuse them as `subject_comments` when the
  digest subject is the PR, so nothing is fetched twice. (If the digest ever
  needs PR comments that weren't fetched early — `pr_number` set during this
  run's phase body — fall back to fetching then; today no phase body that
  sets `pr_number` coexists with a PR digest, so this is just belt-and-braces.)
- PR *data* and inline review comments stay where they are: digest-only.
- Update the now-stale comment at the `advance_phase` call site ("directives
  are always read from the issue thread").

## 4. Banners (`render.rs`, `prep.rs`, `implement.rs`)

- `render_phase_banner`: transition line names the firing command, e.g.
  "Phase advanced: prep-and-plan → implement (triggered by `/approve-plan`
  from jeffatstile)." After it, render each `DirectiveNote` as a bullet, with
  an instruction to relay genuine mistakes (premature / retired) to the user
  in a comment.
- `PRE_PLAN_BODY`: the ready-to-plan summary comment must end by prompting
  the user to comment `/approve-pre-plan` (alias `/approve-preplan`) on the
  issue; wait for that rather than `/proceed`.
- `prep.rs` `complete_body`: instruct Claude to post a hand-off comment — on
  the issue *and* the PR — saying the plan is ready for review and that
  `/approve-plan` on either thread advances to implement.
- `implement.rs` `branch_body`: when the work is ready for human review, post
  a hand-off comment on the issue and the PR prompting `/approve-implementation`
  on either thread. `no_branch_body`: same prompt, issue only (no PR exists).
- `no_prep_body` / review bodies: no directive mentions today; unchanged
  apart from any incidental wording.

## 5. Tests

- `parse_directive`: each command parses at line start; alias
  `/approve-preplan`; boundary rules (`/proceeding`, mid-line mentions, and
  e.g. `/approve-plans` don't match); `/proceed` parses as `Proceed`;
  first-match-wins on multi-line bodies.
- `Phase::approval_command` ↔ `Directive::approves` round-trip for every
  non-terminal phase.
- Advance logic (factor so it's testable on plain comment slices): matching
  directive advances and is consumed; duplicate approval across threads —
  first fires, second consumed as stale; premature directive consumed with a
  note and no advance; retired `/proceed` consumed with a note and no
  advance; already-consumed ids and Claude-authored (marker-tagged) comments
  skipped; two successive approvals in one run advance two phases.
- Banner rendering: transition line includes the command; notes render.

## Build order

1 → 2 → 3 → 4, tests alongside each.

## Out of scope / punted

- ghwf itself posting phase/status updates to the threads — that's #6
  (cross-referenced there).
- Directives in inline review comments or PR review summary bodies — only
  the two conversation threads are scanned.
- Restricting who may approve (any non-Claude commenter advances, as today).
