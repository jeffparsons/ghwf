# Plan — detect PR merge and stop the loop (#30)

The workflow's terminal phase is Review, but nothing detects "done": the PR's
merged state is never fetched anywhere. The `Issue` model only distinguishes
open/closed; `wait` polls the issue object, comment lists, and reaction
watches but not the PR object; the events-feed handler ignores
`PullRequestEvent`. The Claude-side loop ("run `ghwf wait`, then
`ghwf work-on`, repeat") therefore never ends on its own — the Stop hook only
relents when the *issue* is closed (`issue_closed` in `IssueState`) or the
nudge cap is hit. Review-phase status prose already promises "merging or
closing the PR concludes the workflow" (`render.rs`); this plan makes that
true.

Design decisions locked on the issue thread:

1. **Merged PR = workflow complete.** All three layers detect it: `wait`
   wakes, `work-on` records it and tells Claude to stop looping, the Stop hook
   lets the session end.
2. **Closed-without-merge also stops the loop**, with distinct "halted, not
   complete" wording so Claude surfaces it to the user rather than silently
   exiting.
3. **ghwf does not close the issue itself** — GitHub's "Fixes #N" linking or
   the user does that. (The existing issue-closed handling already covers the
   issue getting closed by any means.)
4. **One final status comment** is posted noting why the workflow concluded.

## 1. Model and fetch (`models.rs`, `github.rs`)

- New `PullRequest` model, trimmed to what conclusion detection needs:
  `number: u64`, `state: String` ("open"/"closed"), `merged: bool` (the
  single-PR fetch always carries it), `html_url: String`.
- New `github::fetch_pr(owner, repo, pr) -> Result<PullRequest>` →
  `gh api repos/{owner}/{repo}/pulls/{pr}`.

## 2. Recording the outcome (`state.rs`)

- New `PrOutcome` enum (`Merged`, `Closed`), kebab-case serde, with a
  `label()` for prose ("merged" / "closed without merging").
- `IssueState` gains `#[serde(default)] pub pr_outcome: Option<PrOutcome>`
  (old state files load with `None`).
- New pure helper `pr_outcome(pr: &PullRequest) -> Option<PrOutcome>`:
  `merged` → `Merged`; else `state != "open"` → `Closed`; else `None`.

Like `issue_closed`, the field is *recomputed from the fetched PR on every
`work-on` run*, not latched: a closed-then-reopened PR self-heals back to
`None` and the loop resumes. (`Merged` can never revert.)

## 3. `work-on` detects and concludes (`main.rs`, `implement.rs`)

In `work_on`, where `pr_number` is read and the early PR comments are fetched,
also fetch the PR object and recompute `issue_state.pr_outcome`, remembering
the previous value for change detection.

When the outcome is `Some`:

- **The phase body is replaced** by a concluded banner
  (`render::concluded_body`, §5) — the phase match (`prep::run` /
  `implement::run` / `implement::review`) is skipped entirely. In particular
  `implement::review`'s draft→ready flip never runs against a merged/closed
  PR (where `gh pr ready` would fail).
- **The worktree guard is skipped** (`needs_worktree_guard` short-circuits on
  a recorded outcome): hard-erroring about being in the wrong directory is
  pointless when the work is over.
- **A final status comment is posted once**, when the outcome *changed* this
  run (None→Some, or Closed→None→… after a reopen): the existing
  status-posting machinery (primary/stub threads, `last_posted`,
  `intro_posted`) is reused by passing the conclusion into
  `render_status_comment` (§5). Directive processing still runs first, as
  today — a stale `/approve-implementation` next to a merge is still consumed
  and noted.

Approval directives that arrive after conclusion classify as today (stale /
premature against the current phase); no new handling needed.

## 4. The Stop hook relents (`stop_hook.rs`)

`should_block` returns `false` when `state.pr_outcome.is_some()`, same as
`issue_closed` — purely local state, no network.

## 5. Rendering (`render.rs`)

- New `concluded_body(outcome, pr_url, number) -> String`, used in place of
  the phase body. Merged: "The PR has been merged — the workflow for issue
  #N is complete. Stop the wait/work-on loop: do not run `ghwf wait N` or
  `ghwf work-on N` again unless the user asks." Closed: "The PR was closed
  without being merged — the workflow has halted. Surface this to the user
  and stop the wait/work-on loop." Neither includes `wait_instruction`.
- `render_status_comment` gains a `conclusion: Option<PrOutcome>` parameter.
  When `Some`, the comment is always worth posting (even with no transitions
  or notes), and the closing paragraph replaces `phase_status_prose`:
  "The PR was merged; the workflow for this issue is **complete**." /
  "The PR was closed without being merged; the workflow has **halted**." A
  concluded status never prompts an approval, so `parse_prompted_directive`
  must find no command in it (no reaction watch gets created for it).

## 6. `wait` wakes on merge/close (`wait.rs`)

Direct mode:

- `poll_endpoints` gains, when a PR is recorded, an `Endpoint { key: "pr",
  url: repos/{owner}/{repo}/pulls/{pr}, kind: EndpointKind::PrObject }`.
- `evaluate_fresh` for `PrObject`: parse `PullRequest`; merged → reason
  "The PR was merged."; closed-unmerged → "The PR was closed without
  merging."; open → nothing. No baseline needed: a wait should only be
  running while the PR is open (work-on stops the loop once it isn't), and a
  wait started against an already-concluded PR waking immediately is the
  correct, self-consistent behaviour. Note the PR object's ETag bumps on
  pushes and comment activity too; those fresh-but-open responses produce no
  reason and just refresh the stored ETag, exactly like the issue object
  endpoint today.

Feed mode:

- `FeedSubject` gains `#[serde(default)] merged: Option<bool>` (the
  `PullRequestEvent` payload embeds the full PR object; the field is simply
  absent on issue subjects).
- `feed_wake_reasons` handles `PullRequestEvent` with `action == "closed"`
  for our PR number: `merged == Some(true)` → "The PR was merged (via the
  events feed)."; otherwise → "The PR was closed without merging (via the
  events feed).". Other actions (`opened`, `synchronize`, `reopened`) stay
  ignored — pushes must not wake the wait.

## 7. Tests

- `state.rs`: `pr_outcome` mapping (merged / closed / open); serde
  back-compat (old state files lack `pr_outcome`); round trip.
- `stop_hook.rs`: a recorded `Merged` or `Closed` outcome allows the stop;
  `None` still blocks.
- `wait.rs`: `PrObject` evaluation — merged wakes, closed-unmerged wakes,
  open doesn't; feed — `PullRequestEvent` closed+merged and closed+unmerged
  wake with the right wording, other PRs and other actions don't.
- `render.rs`: concluded bodies name the right outcome and contain no wait
  instruction; conclusion status comment renders with empty
  transitions/notes, names the outcome, and has no prompted directive;
  ordinary status comments are unchanged when `conclusion` is `None`.
- `main.rs`: `needs_worktree_guard` is false once an outcome is recorded.

## 8. Docs

README: document that merging the PR completes the workflow automatically
(and closing it without merging halts it) — ghwf detects it, posts a final
status update, and the Claude session ends on its own.

Build order: 1 (model/fetch) → 2 (state) → 3 + 5 together (work-on/render
link via the conclusion) → 4 (stop hook) → 6 (wait) → 7, 8 alongside.

## Out of scope / punted

- Auto-closing the issue on merge.
- Detecting branch deletion or force-closed issues beyond what
  `issue_closed` already covers.
- A "reopened" wake reason or resumed-workflow status comment after a closed
  PR is reopened (the loop resumes naturally on the next `work-on`).
- Cleaning up the worktree/branch after merge.
