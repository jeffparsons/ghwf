# Plan: Only accept comments and emoji-reactions from allow-listed users

Issue: #92

## Problem

ghwf treats GitHub issue/PR activity as workflow input without checking
*who* authored it — only that the comment isn't ghwf/Claude-authored
(`render::extract_marker`). Two input channels drive the workflow:

- **Typed approval directives** (`/approve-pre-plan`, `/approve-plan`,
  `/approve-implementation`) parsed from comments, and
- **👍 reactions** on ghwf's approval-prompt comments,

both consumed in `advance_phase` (`src/main.rs`). Separately, ghwf
**surfaces new comments into the Claude session** (`collect_new_comments`
→ `render_work_on`) and **wakes `wait`** on new comments/reactions.

Now that the repo is public, *any* GitHub user can comment `/approve-plan`
(or 👍 a prompt) to drive the workflow, or inject prose into the Claude
session via a comment. The native checkbox paths (`ghwf ask` submissions,
marking the PR ready-for-review) are already gated by GitHub write-access,
so the gap is specifically **comments and reactions** — exactly what the
issue names.

## Decision (agreed in pre-plan)

A comment or reaction is **accepted** iff its author is:

1. the **authenticated `gh` user** (always — keeps the default, empty-config
   case working for the person running ghwf), **or**
2. listed in a new **`allowed_users`** config option (GitHub logins,
   matched case-insensitively), **or**
3. a **repo collaborator** — i.e. carries an `OWNER` / `MEMBER` /
   `COLLABORATOR` association.

Everyone else's comments and reactions are ignored — for *both*
phase-advancing input (directives + 👍) *and* the comments surfaced into
the Claude session. Ignored input is reported with a brief note (not
dropped silently), so a teammate's silence isn't a mystery.

### Comments vs. reactions: how the collaborator rule is evaluated

GitHub's **comment** payloads carry `author_association` (already on
`models::Comment` / `models::ReviewComment`), which is set server-side and
cannot be spoofed — so rule 3 is a free, no-API-call check for comments.

GitHub's **reactions** API returns **no** `author_association`
(`models::Reaction` has only `user`, `content`, `id`, `created_at`). To
honour the collaborator rule for 👍, ghwf must resolve collaborator
status another way: fetch the repo's collaborator logins via the API and
test membership. This is done **lazily and memoised** — only when a 👍
from a user not already covered by rule 1/2 actually needs adjudicating
(the common case has no pending unknown 👍, so no extra call).

Minor accepted asymmetry: the collaborators-list endpoint returns users
with repo access (owner, direct/outside collaborators, team/org members
*with access*), which is a close-but-not-identical proxy for the
comment-side `MEMBER` association (an org member with *no* repo access is
`MEMBER` on a comment but absent from the collaborators list). This edge
case — an access-less org member reacting 👍 — is rare; such a user can
still use the typed `/approve-*` comment (which carries the association),
or be added to `allowed_users`. Documented in the field doc comment.

## Design

### New module: `src/access.rs`

A resolved acceptance policy, constructed once per process and threaded
into the ingestion points. Network I/O (collaborator fetches) is done up
front / lazily and memoised, so the hot decision method is pure and
unit-testable.

```rust
pub struct AccessList {
    me: String,                 // authenticated user login
    allowed: Vec<String>,       // allowed_users, lowercased
    // Per-repo collaborator login sets (lowercased), fetched lazily.
    collaborators: HashMap<RepoRef, HashSet<String>>,
}

impl AccessList {
    /// Resolve from config + the authenticated user. One `gh api user`
    /// call. No collaborator fetch yet (lazy).
    pub fn resolve(allowed_users: &[String]) -> Result<Self>;

    /// Pre-fetch and memoise the collaborator set for `repo` (idempotent).
    /// Used before the pure decision pass when a 👍 needs adjudicating.
    pub fn ensure_collaborators(&mut self, repo: &RepoRef) -> Result<()>;

    /// Accept a comment by author + association (no I/O).
    pub fn accepts_comment(&self, login: &str, association: &str) -> bool;

    /// Accept a reaction by author, against the (already-ensured)
    /// collaborator set for `repo` (no I/O; false if not yet fetched).
    pub fn accepts_reaction(&self, repo: &RepoRef, login: &str) -> bool;
}
```

- `me` / `allowed` matching is case-insensitive (`eq_ignore_ascii_case`,
  as `next.rs` already does for logins).
- Association acceptance set: `{"OWNER", "MEMBER", "COLLABORATOR"}`.
- Unit tests construct `AccessList` directly (explicit `me`, `allowed`,
  `collaborators`) — no network.

### `github::fetch_collaborators` (`src/github.rs`)

```rust
/// Logins of the repo's collaborators (users with access), paginated.
pub fn fetch_collaborators(owner: &str, repo: &str) -> Result<Vec<String>>;
```

Endpoint `repos/{owner}/{repo}/collaborators` with `--paginate`, parsing
`User.login`. (Requires the authenticated user to have push access, which
the ghwf operator has.) Reuses the existing `gh_api` + `User` patterns.

### Enforcement in `work-on` (`src/main.rs`)

`config::find()` currently runs *after* `advance_phase` (line ~633).
Resolve the `AccessList` near the top of `work_on` (config is needed for
`allowed_users`; fall back to an empty list when no `ghwf.toml`), so it's
available to every ingestion point below.

1. **`advance_phase`** gains an `&AccessList` parameter and stays pure
   (no network — collaborator sets are pre-fetched, see step 4):
   - `ApprovalEvent::Directive`: if
     `!access.accepts_comment(login, &comment.author_association)`,
     **consume** the comment id (so an allow-list change can't make an old
     directive fire retroactively) and `continue` — no transition, no
     note here (the ignored *comment* is reported by the digest layer in
     step 2, avoiding a double-note).
   - `ApprovalEvent::Thumb`: if `!access.accepts_reaction(repo, login)`,
     **consume** the reaction id and push an *ignored-reaction* note
     (reactions have no other surfacing path). Once-only via
     `consumed_reactions`.
   - `AdvanceOutcome` gains an `ignored_reactions: Vec<IgnoredInput>`
     (login + source thread) rendered in the banner.
   - `ApprovalEvent::Directive` needs the thread's `RepoRef` for the
     reaction-repo lookup symmetry; the merge already tags `source`
     ("issue"/"PR"), map that to the issue vs. code repo.

2. **`collect_new_comments`** gains `&AccessList`: partition *newly-seen*
   comments into accepted (→ `CommentView`s, surfaced as today) and
   rejected (→ a list of ignored logins). Because the seen-record
   (`seen::save`) already records *all* fetched comments every run,
   rejected comments are "new" exactly once, so the ignored note is
   emitted once. The same filter covers a non-accepted `/approve-*`
   comment uniformly (it's just another non-accepted comment to the
   digest). Inline **review comments** (built at main.rs ~993) get the
   same `accepts_comment` filter.

3. **`render_work_on` / banner**: add a concise ignored-input section,
   e.g. `Ignored a comment from @someone (not allow-listed).` and
   `Ignored a 👍 from @someone (not allow-listed).` Keep it short; list
   the logins.

4. **Lazy collaborator pre-fetch**: before `advance_phase`, scan
   `prompt_thumbs` for 👍 whose author is unconsumed and not in
   `me ∪ allowed`; for each such reaction's repo, call
   `access.ensure_collaborators(repo)`. Usually a no-op (no pending
   unknown 👍 ⇒ zero extra API calls).

### Wake-gate filtering in `wait` (`src/wait.rs`)

So non-accepted activity doesn't *wake the Claude session* at all (each
wake costs a Claude turn), apply the same policy to wake reasons. Resolve
one `AccessList` at `wait` startup (reused for the whole invocation) and
pre-fetch collaborators for the issue and code repos once (reactions can
arrive any time, so eager here rather than lazy):

- `comment_reasons` (direct-poll comments): skip non-accepted authors
  (`accepts_comment` — the issue/PR comments API returns
  `author_association`).
- `EndpointKind::Reactions`: skip 👍 from non-accepted authors
  (`accepts_reaction`).
- `EndpointKind::ReviewComments`: skip non-accepted authors.
- Feed mode (`FeedComment` / `feed_wake_reasons`): add `author_association`
  to the `FeedComment` deserialize struct (the events API's
  `IssueCommentEvent` payload includes it) and apply `accepts_comment`.
  (Reactions never appear in the events feed — that's why reaction watches
  poll separately — so no reaction handling is needed in feed mode.)

This is the same `AccessList` type as `work-on`; `wait` is the wake gate,
`work-on` remains the authoritative enforcement.

### Config wiring (`Adding a config option` checklist, CLAUDE.md)

1. **`src/config.rs`**: add `allowed_users: Vec<String>` to `Config` with a
   `///` doc comment (covering: logins, case-insensitive, authenticated
   user always implicitly included, repo collaborators auto-accepted, the
   reaction/`MEMBER` asymmetry note) and `#[serde(default)]`.
2. **`src/init.rs`**: a wizard prompt (gated on
   `!doc.contains_key("allowed_users")`) — Confirm "Allow additional users
   to drive the workflow via comments/👍? (you and repo collaborators are
   always accepted)", then a comma-separated logins Text prompt; add
   `set_allowed_users` + a `parse_allowed_users` helper mirroring
   `parse_priority_labels`/`parse_issue_repos`.
3. **README**: add `allowed_users` to the annotated `ghwf.toml` example
   with an explanatory comment.
4. **`src/config_schema.rs`**: destructure `allowed_users` in
   `example_covers_every_field` (compile-time guard), emit it in
   `render_example` (e.g. `["octocat"]`) via `insert`, and add a
   `config_comment` entry. The `example_covers_every_top_level_option`
   test (reflection-driven) then passes automatically.

## Tests

- **`access.rs`** (pure): authenticated user accepted regardless of
  association; `allowed_users` case-insensitive; `OWNER`/`MEMBER`/
  `COLLABORATOR` accepted via `accepts_comment`; `NONE`/`CONTRIBUTOR`
  rejected; `accepts_reaction` true only for me/allowed/collaborator-set
  members; reaction with un-fetched collaborator set ⇒ rejected.
- **`advance_phase`** (extend existing tests in main.rs): a `/approve-*`
  from a `NONE`-association non-listed author does not advance and is
  consumed; a 👍 from a non-accepted user does not advance and yields an
  ignored-reaction note; accepted authors still advance (existing tests
  pass with an accept-all `AccessList`).
- **`collect_new_comments`**: non-accepted author's comment is excluded
  from the views and reported as ignored; accepted author's surfaces as
  today.
- **`config_schema`**: existing round-trip / coverage tests extended for
  the new field (mostly automatic).
- **`wait`**: `comment_reasons` / reaction / review filtering skip
  non-accepted authors (construct an `AccessList` with a known accept-set).

## Non-goals / out of scope

- `ghwf ask` checkbox submissions and PR ready-for-review: already gated by
  GitHub write-access; unchanged.
- `ghwf next` issue selection and outbound commands (`hand-off`,
  `create-issue-comment`, etc.): not ingestion of third-party input.
- Org-membership-without-repo-access exactness for reactions (documented
  asymmetry above); not worth an extra membership API per 👍.

## Rollout note

Default behaviour changes: previously *anyone* could drive a public repo's
workflow; now only the operator + collaborators + `allowed_users` can. This
is the intended fix and is safe for the common single-operator case (the
authenticated user is always accepted).
