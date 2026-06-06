# Plan — inline PR review comments in the implement digest (#4)

During implement/review, `work-on` digests only the PR *conversation* thread
(the shared issues comments endpoint). This plan folds in **inline review
comments** — those attached to specific lines/diffs via `pulls/{n}/comments` —
with the same new-or-changed semantics, reusing the per-session seen cache.

Design decisions locked on the issue thread:

1. **Own digest section** — inline comments render under their own heading
   rather than interleaved chronologically with conversation comments.
2. **`path:line` context only** — no `diff_hunk` snippet; Claude can read the
   file directly.
3. **Separate seen-cache map** — inline review comment ids come from a
   different namespace than issue comment ids, so they get their own map
   rather than sharing `comments`.
4. **Out of scope:** review *summary* bodies (`pulls/{n}/reviews`) — a third
   endpoint; conversation + inline covers the feedback loop for now.

## 1. Model (`models.rs`)

Add `ReviewComment`: the shared comment fields (`id`, `user`, `body`,
`created_at`, `updated_at`, `html_url`, `author_association`) plus:

- `path: String` — the file the comment is anchored to.
- `line: Option<u64>` — `null` when the comment is outdated against the
  latest diff, or file-level.
- `original_line: Option<u64>` — the anchor in the diff the comment was left
  on; the fallback when `line` is `null`.

Give it a `location()` helper: `path:line`, falling back to
`path:original_line`, falling back to bare `path` (file-level comments have
neither).

## 2. Fetch (`github.rs`)

```rust
pub fn fetch_review_comments(owner: &str, repo: &str, pr: u64) -> Result<Vec<ReviewComment>>
```

`gh_api(&["--paginate", "repos/{owner}/{repo}/pulls/{pr}/comments"])`. Unlike
`fetch_comments` this takes a resolved `(owner, repo, pr)` directly — it is
only called once a `pr_number` is recorded, so no issue-arg resolution is
needed.

## 3. Seen cache (`seen.rs`)

Extend `SeenRecord`:

```rust
// Inline review comment id -> content hash of its body.
#[serde(default)]
pub review_comments: BTreeMap<u64, String>,
```

`#[serde(default)]` keeps existing seen-records parsing cleanly.

## 4. Digest assembly (`main.rs`)

In `work_on`, when `digest_pr` is true, also `fetch_review_comments` for the
recorded `pr_number`. Diff each against `record.review_comments` by content
hash exactly as conversation comments are diffed, building review-comment
views for the renderer. Apply the same own-session-token filter for symmetry,
even though ghwf never authors inline comments today.

When saving the updated record: replace `review_comments` with the fetched
set when we fetched it, otherwise carry the previous map over unchanged (the
issue-thread phases never fetch inline comments and must not clobber the map).

## 5. Render (`render.rs`)

- Add a `ReviewCommentView` mirroring `CommentView` plus a `location: String`.
- `render_work_on` gains a `new_review: &[ReviewCommentView]` parameter:
  - The "No new activity" early-return triggers only when *all three* of body
    change, conversation comments, and review comments are empty.
  - After the conversation-comments section, render "New inline review
    comments since you last ran `ghwf work-on`:", each entry as
    `**login** at <time> said on `path:line`<tag>:` followed by the
    blockquoted body, `<hr>`-separated like conversation comments, with the
    same `(updated)` tag for changed ones.

## 6. Tests

- `ReviewComment::location()` fallback chain: `line` set → `original_line`
  only → neither.
- `render_work_on` with review comments: section renders with location
  annotation; "no new activity" only when all inputs are empty; conversation
  and review sections compose.
- `SeenRecord` back-compat: a record JSON without `review_comments`
  deserializes with an empty map.

Build order: 1 → 2 → 3 → 4 → 5, with 6 alongside 4–5.

## Out of scope / punted

- Review summary bodies (`pulls/{n}/reviews`).
- Resolved/outdated-thread state (REST doesn't expose resolution; GraphQL
  only).
- ghwf authoring inline review comments.
