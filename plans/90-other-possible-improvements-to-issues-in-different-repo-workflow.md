# Plan: harden in-session command targeting (#90, items #2 + #3)

## Background

Issue #90 collects five suggested footgun-reducers for the workflow where the
ghwf issue lives in a *different* repo than the code being worked on. PR #89
("Anchor bare issue numbers to the bound issue's repo", `64ba7fa`) already
landed two of them:

- **#1** — every in-session command (`work-on`, `hand-off`, `wait`,
  `create-issue-comment`, `ask`, `create-issue`) already takes an *optional*
  issue arg and falls back to `$GHWF_ISSUE`, then to worktree state
  (`resolve_issue_arg`/`infer_issue_arg`, `src/main.rs:389-432`).
- **#5** — the instruction bodies and the `/work-on` skill no longer hand-build
  a URL or show a bare number.

#89 also closed the *bare-number* misroute: `qualify_bare_number`
(`src/main.rs:441-445`) anchors a bare number to the bound issue's repo. But it
deliberately leaves an **explicit URL** untouched (`resolve_issue_arg`,
`src/main.rs:390-400`) — the URL still wins and is never compared to the bound
issue. So the report's actual failure (a full URL pointing at the *wrong* repo
posting silently) is still open.

The user selected **#2 + #3** for this issue (#4 declined).

- **#2** — when an explicit target disagrees with the bound issue, hard-error
  instead of posting.
- **#3** — echo the resolved target before each mutation so a misroute is
  visible to model and human.

## Design

### The validation key (#2)

The check is **"does the resolved target belong to the same workflow as the
bound issue?"** — not a naive "number == bound number", because `hand-off`/`ask`
legitimately accept the *PR* thread, which `state::find_workflow_issue`
(`src/state.rs:446`) maps back to the workflow issue. So:

- **Target workflow issue**: the `(owner, repo, number)` that
  `resolve_issue_ref` + `find_workflow_issue` already produce in `hand-off`
  (`src/main.rs:1743-1751`) and `ask` (`src/main.rs:1888-1895`). For
  `create-issue-comment` (which today does no `find_workflow_issue`) we compute
  it the same way for the check.
- **Bound issue**: `infer_issue_arg()` returns the bound issue *URL*
  (`$GHWF_ISSUE` or the worktree's `/issues/N`). The bound is always the issue
  (never a PR), so it parses purely to `(owner, repo, number)` — no network.

Comparing these two is correct under PR/issue duality: an omitted arg resolves
to the bound issue (passes trivially); the bound issue's PR URL maps back to the
bound issue (passes); an unrelated issue/PR — the dependabot-PR case in the
report — maps to a different workflow or to none, and is rejected.

### Helpers (both pure → unit-testable, sit beside `qualify_bare_number`)

```rust
/// Parse a ghwf issue URL into (owner, repo, number). Pure: bound issues are
/// always issue URLs produced by ghwf, so no network is needed.
fn parse_issue_url(url: &str) -> Option<(String, String, u64)>
```
Reuse `github::parse_owner_repo` for owner/repo and parse the trailing
`/issues/<n>` (or `/pull/<n>`) segment for the number.

```rust
/// Bail if a resolved target doesn't belong to the bound issue's workflow.
/// `target_workflow` is the workflow issue the target maps to, or None when the
/// target is not a tracked workflow at all. No-op when `bound` is None
/// (nothing to validate against) or unparseable (stay lenient, matching
/// qualify_bare_number's passthrough philosophy).
fn ensure_target_matches_bound(
    target_workflow: Option<(&str, &str, u64)>,
    bound: Option<&str>,
) -> Result<()>
```
On mismatch it bails with both sides named, e.g.:
> explicit target `dev-environment#15827` does not match this session's issue
> `StileEducation/documentation#15827`; in-session commands act on the bound
> issue — omit the argument, or pass the bound issue's URL/number.

### Echo (#3)

```rust
/// Print the resolved target to stderr before a mutation, so a misroute is
/// visible. Best-effort: a failed title/state fetch downgrades to
/// `→ owner/repo#number`.
fn echo_target(owner: &str, repo: &str, number: u64, repo_ctx: Option<&github::RepoRef>)
```
Does a best-effort `github::fetch_issue` for `title` + `state` (the `Issue`
struct carries both, `src/models.rs:11`) and prints
`→ owner/repo#number "title" (OPEN)` to **stderr** (so it never pollutes the
JSON that `create-issue-comment` writes to stdout). The line-formatting is split
into a pure `format_target_line(owner, repo, number, title_state: Option<(&str, &str)>) -> String`
so it can be unit-tested without a network call.

The echo shows the **workflow issue** identity (`owner/repo#number`), which is
the "am I on the right workflow?" signal the report wanted — exactly what would
have caught `dev-environment#15827 "Bump sorbet…" (CLOSED)`.

## Changes by file

### `src/main.rs`

1. Add `parse_issue_url`, `ensure_target_matches_bound`, `echo_target`, and
   `format_target_line` near `qualify_bare_number` (~line 445).

2. **`hand_off`** (`~1742-1751`): after `find_workflow_issue` resolves the
   workflow issue, call `ensure_target_matches_bound(Some((&owner, &repo,
   number)), infer_issue_arg()?.as_deref())?`. Then `echo_target(&owner, &repo,
   number, repo_ctx.as_ref())` immediately before the post (`~1810`).

3. **`ask`** (`~1888-1895`): same two calls, same positions (echo before the
   post at `~1926`).

4. **`create_issue_comment`** (`~1576-1582`): after `resolve_issue_ref`, when
   `infer_issue_arg()?` is `Some`, compute the target workflow via
   `find_workflow_issue(&owner, &repo, number)` and call
   `ensure_target_matches_bound(...)`; when it's `None`, skip (preserves
   standalone, non-session use). Then `echo_target(...)` before
   `post_issue_comment`. Posting still targets the originally-resolved thread —
   validation gates it, it doesn't rewrite it.

`work-on`, `wait`, and the read-only PR commands (`show-pr`, `update-pr`,
`pr-checks`, `reply-review-comment`) are **out of scope**: the report's #2/#3 are
about *posting* workflow comments, and those PR commands legitimately take an
explicit PR URL whose number differs from the issue — guarding them uniformly
would need extra care and isn't what was asked. (Noted as a deliberate boundary,
not an oversight.)

### Tests (`src/main.rs` test module, alongside the `qualify_bare_number` tests)

- `ensure_target_matches_bound`:
  - matching `(owner, repo, number)` → `Ok`
  - different repo → `Err`
  - same repo, different number → `Err`
  - target not a tracked workflow (`None`) with a bound issue → `Err`
  - `bound: None` → `Ok` (nothing to validate)
  - unparseable bound → `Ok` (lenient passthrough)
- `parse_issue_url`: `/issues/N` and `/pull/N` forms, and a malformed URL → `None`.
- `format_target_line`: with and without title/state.

## Out of scope / notes

- **#4** (refuse posting to a target lacking the `<!-- ghwf:v1 … -->` marker or
  to a closed issue/PR) was declined for this issue.
- No new config field, so the CLAUDE.md "Adding a config option" checklist does
  not apply.
- Behavioural change worth a line in the README's safety/targeting notes if one
  exists; will check during implementation and add a short note if so.

## Risks

- **Over-strict `create-issue-comment`**: with a bound issue set, an explicit
  target outside that workflow is now rejected. This is intended (the skill files
  follow-ups with `create-issue`, not `create-issue-comment`), and standalone use
  with no bound issue is unaffected.
- **Extra fetch for the echo**: one best-effort `gh api` per mutation. Mutations
  are infrequent and the fetch is non-blocking (failure downgrades the line), so
  the cost is negligible.
