# Plan: Only automatically select issues from allow-listed users (#93)

Follow-up to #92, same philosophy: on a public repo, the general public can open
issues, and we don't want ghwf to *automatically* start working on them. So the
allow-list that already gates comments and 👍 reactions (#92) should also gate
which issues automatic selection (`ghwf next` / `next --wait` / `forever`) will
pick up.

## Approach

An issue is auto-selectable only if its **author** is accepted by the existing
`AccessList`: the authenticated operator (always), a login in `allowed_users`,
or a repo collaborator (`OWNER` / `MEMBER` / `COLLABORATOR` association). The
repo-wide open-issues listing returns both `user.login` and `author_association`
per issue, so the collaborator check is free — exactly like `accepts_comment`,
with no collaborator-list fetch.

The gate is **unconditional** (reuses `allowed_users`, no new config option):
on a private repo every author is a collaborator so nothing changes; on a public
repo, strangers' issues stop being auto-picked.

It applies to **automatic selection only**. The gate lives in `next.rs`'s
`select`, reached solely by `ghwf next` / `next --wait` / `forever`. An explicit
`ghwf work-on <n>` goes through `launch::prepare` and never calls `select`, so
naming an issue by hand stays ungated — deliberately working a stranger's issue
still works.

Only **fresh** issues are gated. A resumable issue (work already underway) is
resumed regardless of author — it passed the gate, or was started deliberately,
when it began. This slots in beside the existing blocked/tracking freshness
gates. Skipped issues are **reported** (a new `skipped_unlisted_author` bucket),
consistent with #92's "report rather than drop silently."

## Changes

### `src/models.rs` — author fields on `IssueListing`

Add to `IssueListing`:

- `user: User` (the issue author) — `#[serde(default)]` so the existing tests
  and sub-issue children that omit it still parse; `User::login` defaults to an
  empty string, which no allow-list rule accepts.
- `author_association: String` — `#[serde(default)]`, same defaulting as
  `Comment::author_association`.

The listing endpoint (`repos/{owner}/{repo}/issues`) and the sub-issues endpoint
both return these, so no fetch changes are needed. The `issue(...)` test helper
in `next.rs` builds `IssueListing` literally, so it will need the two new fields
(default `User { login: "me" }` or similar — see Tests).

### `src/access.rs` — `accepts_issue`

Add a method mirroring `accepts_comment` (the association-based collaborator
check is identical):

```rust
/// Accept an issue for automatic selection by its author login and GitHub
/// `author_association`. Pure: the association classifies collaborators with
/// no API call, exactly as for comments.
pub fn accepts_issue(&self, login: &str, association: &str) -> bool {
    self.is_self_or_allowed(login) || COLLABORATOR_ASSOCIATIONS.contains(&association)
}
```

(If preferred, `accepts_comment` and `accepts_issue` could share one private
helper, since they're identical today; keeping them as two named entry points
documents intent and lets them diverge later. Decide during implementation —
likely a thin `accepts_issue` delegating to the shared logic.)

Add a unit test that an allow-listed / operator / collaborator author is
accepted and a stranger (`NONE`/`""`) is rejected.

### `src/next.rs` — gate fresh candidates in `select`

`select` (and the `claim_pick` wrapper, `pick`, `wait_for_pick_excluding`,
`run_forever`) gains access to the resolved `AccessList`. Plumb an
acceptance predicate into `select`/`claim_pick` the same way `excluded` and
`status` are injected (a closure `accepts_author: impl Fn(&IssueListing) -> bool`),
so the selection logic stays testable without network/filesystem.

In `select`, in the `IssueStatus::Fresh` arm — after the blocked/tracking
checks, or before them; order doesn't matter much, but place it so the most
specific reason is reported (an unlisted-author issue need not also be reported
as blocked) — skip a fresh issue whose author isn't accepted, pushing its number
onto a new `skipped_unlisted_author: Vec<u64>` field on `Selection`. Resumable
issues bypass this (they're pushed as candidates before the freshness gates,
unchanged).

Decide precedence with the other fresh-skip buckets. Proposed:
`unlisted_author → blocked → tracking` (don't bother reporting that a stranger's
issue is also blocked). Mirror the existing `blocked`/`tracking` precedence
tests with one for author-vs-blocked.

Wire the predicate at the two real call sites (`pick` and
`wait_for_pick_excluding`): resolve an `AccessList` from the config's
`allowed_users` once (`AccessList::resolve`), and pass
`|issue| access.accepts_issue(&issue.user.login, &issue.author_association)`.
`resolve` makes one `gh api user` call; both call sites already call
`github::authenticated_user()`, so consider resolving the `AccessList` and
reading `me` from it to avoid a duplicate call (or just accept the extra call —
it's once per `pick`, negligible). Confirm `config::find()` already yields
`allowed_users`; if not, extend the destructure there.

### `src/next.rs` — report the skip in `announce_pick`

Add a reporting loop alongside the existing `skipped_live` / `skipped_blocked` /
`skipped_tracking` loops:

```rust
for number in &selection.skipped_unlisted_author {
    println!(
        "Skipping #{number} — its author isn't allow-listed for automatic \
         selection (work it explicitly with `ghwf work-on {number}` if intended)."
    );
}
```

Keep the message pointing the operator at the manual escape hatch.

### `README.md` — extend the `allowed_users` doc

The `allowed_users` annotated example currently says it governs comments and
👍 reactions. Add a sentence that it *also* gates which issues automatic
selection (`ghwf next` / `forever`) will pick up: only issues authored by you,
an `allowed_users` login, or a repo collaborator are auto-selected; others are
skipped (and can still be worked explicitly with `ghwf work-on <n>`).

No `config ls`/`info`/`example` or `config init` changes: no new field is
introduced (we reuse `allowed_users`), so the config-schema guard and wizard are
untouched.

## Tests

- `access.rs`: `accepts_issue` accepts operator / allow-listed / collaborator
  associations and rejects strangers (mirror `collaborator_associations_accepted_for_comments`).
- `next.rs`:
  - Update the `issue(...)` helper to populate the new `user` /
    `author_association` fields. Make the default author one the test predicate
    accepts (e.g. login `"me"`, or association `OWNER`) so all existing
    selection tests keep passing unchanged.
  - The `pick`/`select` test helpers gain an `accepts_author` argument; give the
    existing helpers an accept-all default so current tests are unaffected.
  - New test: a fresh issue from a non-accepted author is skipped and reported
    in `skipped_unlisted_author`, and a lower-priority accepted issue wins.
  - New test: a *resumable* issue from a non-accepted author is still selected
    (the gate is fresh-only).
  - New test: precedence — an unlisted-author issue that is also blocked is
    reported only under `skipped_unlisted_author` (per chosen precedence).
- `cargo test` and `cargo clippy` clean.

## Out of scope / notes

- Manual `ghwf work-on <n>` stays ungated by design.
- Tracking issues are already skipped by `select` (`skipped_tracking`), so the
  tracking-redirect leaf path (`resolve_workable`, reached from the manual/launch
  side) is unaffected; no author gating is added there.
- The `assigned_to`/`only_assigned_to_me` filters are independent and unchanged.
