# Plan — concise status stubs on the less obvious thread (#21)

Today a status update is rendered once and the identical full body is posted
to both the issue thread and (once a PR exists) the PR conversation thread
(`main.rs` ~200–208). The duplication is noisy: a human watching both threads
reads the same multi-paragraph comment twice. This plan posts the full status
to the "obvious" thread for the workflow's current position and a one-line
stub — linking to the full comment — to the other thread, so nothing is
missed but the noise is gone.

Design decisions locked on the issue thread (all proposals accepted;
remaining ambiguity resolved here):

1. **Primary thread is chosen by phase**, the phase *after* this run's
   transitions: pre-plan and prep-and-plan → the **issue** is primary
   (discussion lives there; the PR is at most a freshly opened draft);
   implement and review → the **PR** is primary (that's where code lands and
   review happens). When no PR exists yet, the issue gets the full body and
   there is no stub — unchanged from today.
2. **Stub content** is a single line wrapped in the usual `**ghwf:**` header
   and hidden status marker (so digests and directive scanning keep ignoring
   it):
   - with transitions: `Phase advanced: {first.from} → {last.to} — full
     update: {url}` (first→last collapses the rare multi-transition run);
   - without transitions (intro or note-only runs): `Status update posted on
     the {issue|PR}: {url}`.
   No "next approval command" line — the link carries that.
3. **Failure fallback:** posting stays best-effort, but if the full post to
   the primary thread fails, the secondary thread gets the *full* body
   instead of a stub (there is nothing to link to, and nothing should be
   lost). A failed stub post just warns, as today.

## 1. Stub rendering (`render.rs`)

New pure function alongside `render_status_comment`:

```rust
/// One-line pointer posted to the secondary conversation thread, linking to
/// the full status update on the primary one. `primary_noun` is "issue" or
/// "PR"; `full_url` is the posted comment's html_url.
pub fn render_status_stub(
    transitions: &[Transition],
    primary_noun: &str,
    full_url: &str,
) -> String
```

- With transitions: `Phase advanced: {from} → {to} — full update: {url}`,
  where `from` is the first transition's `from` and `to` the last's `to`.
- Without: `Status update posted on the {primary_noun}: {url}`.
- Callers wrap it with the existing `build_status_comment_body`, so the
  header and `<!-- ghwf:v1 status -->` marker come along for free.

Also a small helper for the phase → primary mapping, so it's testable and the
posting code reads declaratively:

```rust
/// Which conversation thread gets the full status update in this phase.
/// The other thread gets a one-line stub linking to it.
pub fn status_primary_is_pr(phase: Phase) -> bool
```

`false` for `PrePlan` / `PrepAndPlan`, `true` for `Implement` / `Review`.

## 2. Posting (`main.rs`)

Rework the post block at ~`main.rs:198–217`:

- No PR recorded → exactly today's behaviour: full body to the issue.
- PR recorded → order the two subjects as (primary, secondary) per
  `status_primary_is_pr(phase)` — `phase` here is already the post-transition
  phase used to render the status. Then:
  1. Post the full body to the primary thread via the existing `post_status`.
  2. On success, build the stub from the returned comment's `html_url` and
     post it to the secondary thread.
  3. On failure (`None`), post the full body to the secondary thread instead
     (decision 3) — the existing stderr warning from `post_status` already
     covers the failed primary.
- Feed-lag self-calibration (`issue_state.last_posted`) records the newest
  own post: the secondary-thread comment when it posted, else the primary.
  This matches today's intent (the code currently prefers the later, PR-side
  post) and keeps `wait`'s baseline at the most recent own comment.

`render_status_comment` itself is untouched: its `pr_url` paragraph (review
phase) is part of the full body regardless of which thread hosts it.

## 3. Tests

- `render_status_stub`: transition form names `from → to` and contains the
  URL; multi-transition run collapses to first-from → last-to; no-transition
  form names the primary noun and URL; output wrapped by
  `build_status_comment_body` still carries the status marker (and so stays
  `hidden_from_digest`).
- `status_primary_is_pr`: the four-phase mapping.
- Existing `render_status_comment` / banner tests are unaffected.

The posting order and fallback live in `main.rs` glue around network calls;
they follow directly from the two tested helpers and stay untested, like the
current post block.

## Out of scope / punted

- Choosing the primary thread by where the triggering directive was posted —
  undefined for directive-less updates; phase-based mapping is predictable.
- Stubbing or deduplicating Claude-authored comments (`create-issue-comment`)
  — this plan covers ghwf status machinery only.
- Editing/pinning a rolling status comment instead of posting new ones.
