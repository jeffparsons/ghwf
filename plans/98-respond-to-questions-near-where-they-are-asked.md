# Plan: Respond to questions near where they are asked (#98)

## Goal

Steer the agent to answer each user question **in the same place it was
asked**, and make that actually possible for inline review comments:

- comment on the issue thread → reply on the issue thread;
- comment on the PR conversation thread → reply on the PR conversation thread;
- inline review comment on a PR diff → reply inline in that review thread.

## Background — what already exists

The building blocks are mostly present:

- `create-issue-comment [issue]` posts to a thread. Passing the **issue**
  number targets the issue thread; passing the **PR** number targets the PR
  conversation thread (GitHub treats a PR as an issue, and
  `state::find_workflow_issue` resolves a PR number back to its workflow issue,
  so the #90 bound-issue guard passes).
- `reply-review-comment --id <id>` replies inline within a PR review thread
  (`github::reply_review_comment`, `src/github.rs:52`).
- `render_work_on` (`src/render.rs:681`) already groups incoming comments by
  origin — issue thread / PR conversation thread / inline review — so the agent
  can *see* where each question was asked.

## The two real gaps

1. **Inline replies are impractical because the comment `id` isn't surfaced.**
   The inline-review section of `render_work_on` (`src/render.rs:730-747`)
   prints author, timestamp, `location`, and body — but not
   `view.comment.id`. `reply-review-comment --id <id>` needs that id, and the
   agent has no way to get it from the digest. This is the blocker that makes
   "reply inline" a non-starter today.

2. **No guidance steers replies to where they were asked.** The phase banners
   and the shared `question_instruction` (`src/render.rs:236`) route everything
   to the issue thread by default; the `/work-on` skill only mentions
   `reply-review-comment` in passing. So in practice the agent answers on the
   issue thread regardless of where the user wrote.

## Deliberate carve-out (out of scope)

The workflow-advancing actions — `hand-off`, `hand-off --question`, `ask` —
drive the needs-you label / attention state machine, which is inherently
issue-centric. They **stay on the issue thread**. "Reply where it was asked"
governs *conversational answers and clarifications*, not the phase
hand-off / blocking-question machinery. The new guidance must not contradict
this (the agent still hands off / asks blocking questions on the issue thread).

## Changes

### 1. Surface the inline-comment id + reply hint — `src/render.rs`

In the inline-review section of `render_work_on` (the loop at
`src/render.rs:736-747`), for each `ReviewCommentView` also print the comment
id and a ready-to-use reply command, e.g. append a line like:

> Reply in this thread with `ghwf reply-review-comment --id <id>` (body from stdin).

using `view.comment.id`. Add a one-line lead-in to the section header making
the "reply here, in the inline thread" expectation explicit, rather than the
agent defaulting to the issue thread.

### 2. A shared "reply where asked" instruction — `src/render.rs`

Add a small shared helper (sibling to `question_instruction` /
`wait_instruction`), e.g. `reply_where_asked_instruction()`, returning one
short paragraph:

- answer each question in the thread it was asked in;
- issue-thread comment → `create-issue-comment` (the issue);
- PR-conversation comment → `create-issue-comment <PR#>` (the PR);
- inline review comment → `reply-review-comment --id <id>`;
- (reinforce the carve-out: blocking questions and phase hand-offs still go
  via `hand-off` / `ask` on the issue thread).

### 3. Wire it into the implement / review banners — `src/implement.rs`

Include `reply_where_asked_instruction()` in `branch_body`
(`src/implement.rs:137`) and `review_body` (`src/implement.rs:174`) — the
phases where a PR (and therefore PR-thread / inline comments) can exist. Leave
`no_branch_body` / `review_no_branch_body` alone: with `--no-branch` there is
no ghwf PR, so only the issue thread applies.

Leave `pre_plan_body` (`src/render.rs:287`) unchanged: before a PR exists the
issue thread is the only place, so its "discuss on the issue itself" is already
correct.

### 4. Update the `/work-on` skill — `src/install.rs`

Extend the `SKILL_CONTENT` bullet about posting comments
(`src/install.rs:43-48`) to state the "reply where asked" principle and name
the three channels, so an installed skill carries the steer even outside the
per-run banner. (`SKILL_CONTENT` is a compile-time const; `ghwf install`
regenerates the skill file from it.)

### 5. Doc polish — `src/main.rs`

Clarify the `CreateIssueComment` doc comment (`src/main.rs:127-140`) to spell
out that passing a PR number targets the PR conversation thread, so the "reply
on the PR thread" path is discoverable from `--help`.

## Tests

- `src/render.rs` tests: extend the inline-review-comment render test
  (around `render_work_on_*` / the existing "New inline review comments"
  assertion at `src/render.rs:1357`) to assert the comment **id** and the
  `reply-review-comment --id` hint now appear in the output.
- `src/implement.rs` tests: add a case asserting `branch_body` and
  `review_body` contain the reply-where-asked steer (mirroring the existing
  `waiting_bodies_*` table tests at `src/implement.rs:244-268`).
- Existing tests (`pre_plan_body_*`, `waiting_bodies_*`, install skill
  marker/up-to-date checks) must continue to pass.

## Verification

- `cargo build` and `cargo test`.
- Manually eyeball a rendered `render_work_on` digest containing an inline
  review comment to confirm the id + reply command read naturally.

## Out of scope

- Making `hand-off` / `hand-off --question` / `ask` location-aware (e.g.
  asking a blocking clarifying question back inside an inline thread). Flagged
  in pre-plan as a larger, mechanism-level change; not pursued here.
