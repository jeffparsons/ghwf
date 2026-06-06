# Plan — surface PR conversation comments in every digest (#19)

`work-on` digests exactly one thread: the PR conversation during
implement/review, the issue thread otherwise (`digest_pr` in `main.rs`). A
non-directive comment posted on the other thread is silently invisible —
observed on #7, where a substantive question on the plan PR (#18) never
surfaced during prep-and-plan.

Design decisions locked on the issue thread:

1. **Both threads, every phase** — once a PR exists, the digest covers the
   issue thread *and* the PR conversation thread in all phases, not just
   prep-and-plan. (The reverse gap — issue-thread comments invisible during
   implement/review — is real too.)
2. **Inline review comments whenever a PR exists** — plan feedback can arrive
   as inline comments on the plan file's diff, so `fetch_review_comments`
   runs in every phase with a PR, not just implement/review.
3. **PR body/title never digested** — the PR description is ghwf-authored
   boilerplate; editing it as a feedback channel would be silly. The issue is
   always the digest's primary subject (header + body-changed detection).

This also fixes a latent `wait`/`work-on` disagreement introduced by #7:
`wait` already polls both threads and the inline review comments endpoint
whenever a PR exists (`poll_endpoints` in `wait.rs`), so today it wakes on a
prep-and-plan PR comment that the follow-up `work-on` then doesn't show.
Worse, `work-on` only folds inline review comments into the wait baseline
when it fetched them (implement/review), so an inline comment posted during
prep-and-plan re-wakes every subsequent `wait` forever. Fetching everything
whenever a PR exists makes the digest and the baseline cover exactly what
`wait` watches.

## 1. Digest assembly (`main.rs`)

Replace the `digest_pr` subject-selection block with unconditional
issue-primary assembly:

- The issue (`issue_data`, `issue_comments`) is always the digest subject:
  `body_hash` / `body_changed` compare the *issue* body against
  `record.issue_body_hash`, in every phase.
- When `pr_number` is `Some` (re-read after the phase body, which may have
  just opened the PR): reuse `early_pr_comments` for the PR conversation
  thread, falling back to a fetch when the PR appeared during this run; and
  `fetch_review_comments`. No more `fetch_issue` for the PR — the PR object
  itself is not digested.
- Factor the new-or-changed conversation-comment loop into a helper
  (`collect_new_comments(comments, &record.comments, &my_token)` →
  `Vec<CommentView>`) and run it once per thread, producing separate
  `new_issue` and `new_pr` lists. Both diff against the same
  `record.comments` map — conversation comment ids share one global
  namespace, so a single map stays correct.
- Wait-baseline folding: the early issue + PR comment fold stays as is; the
  post-digest fold now folds PR comments (when fetched late) and inline
  review comments. The PR object's `updated_at` is no longer folded into
  `since` — `wait` never polls the PR object endpoint, and the comment
  `updated_at` folds cover the comment lists it does poll.
- Seen-record save: `comments` becomes the union of both threads' id→hash
  entries; `review_comments` is replaced whenever a PR exists (now: every
  phase with a PR) and carried over unchanged otherwise (pre-PR runs).

Migration note: an existing mid-implement seen record holds PR comment
hashes in `comments` and the PR body hash in `issue_body_hash`. After this
change the next `work-on` re-surfaces the issue body and any unseen issue
comments once. One-off noise; no code needed.

## 2. Render (`render.rs`)

Rework `render_work_on` for a two-thread digest:

```rust
pub fn render_work_on(
    issue: &Issue,
    body_changed: bool,
    new_issue: &[CommentView],
    pr_number: Option<u64>,
    new_pr: &[CommentView],
    new_review: &[ReviewCommentView],
) -> String
```

- The `noun` parameter goes away — the header and body section are always
  the issue's. `capitalize_first` loses its caller and goes too.
- Section order: issue body (when changed) → issue-thread comments →
  PR-thread comments → inline review comments, `<hr>`-separated as today.
- The two conversation sections get distinguishing headings:
  "New comments on the issue thread since you last ran `ghwf work-on`:" and
  "New comments on the PR (#M) conversation thread since you last ran
  `ghwf work-on`:". The inline review heading is unchanged (inline comments
  are unambiguously the PR's).
- The "No new activity" early return triggers only when all four inputs are
  empty, and names both threads when a PR exists: "No new activity on issue
  #N \"title\" or PR #M since you last ran `ghwf work-on`."

## 3. Seen cache (`seen.rs`)

No schema change. Update the `comments` field comment to say it holds
conversation comments from the issue thread and, once a PR exists, the PR
thread too (one global id namespace).

## 4. Tests

- `main.rs`: unit-test the `collect_new_comments` helper — new, updated,
  unchanged, and hidden (status / own-session) comments.
- `render.rs`: update existing `render_work_on` tests for the new signature;
  add: PR section renders with its `#M` heading and composes after the
  issue section; no-activity message names both threads when a PR exists
  and only the issue otherwise; all-empty early return requires all four
  inputs empty.

Build order: 1 and 2 together (the signature change links them), then 3 and
4 alongside.

## Out of scope / punted

- Digesting PR body/title edits (decision 3).
- Review *summary* bodies (`pulls/{n}/reviews`) — still punted, as in #4.
- Suppressing the one-off re-surfacing for in-flight seen records.
