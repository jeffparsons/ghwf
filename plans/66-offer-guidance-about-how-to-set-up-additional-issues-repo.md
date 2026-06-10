# Plan: allow an `issue_repos` allowlist (work on issues from other repos)

## Problem

`ghwf` operates on exactly one repo, derived from `main_repo`'s git remote
(`config_repo()` in `src/github.rs`). `issue_endpoint()` (`src/github.rs:463`)
hard-rejects any issue URL pointing at a different repo — the error the user
hit when running `ghwf work-on` on a `StileEducation/documentation` issue from a
`dev-environment` checkout. There's already a `TODO` next to that check about
allowing an allowlist.

We want issues to live in a **different** repo while the worktree, branch, and
PR still live in the configured `main_repo`. Concretely: issue in
`StileEducation/documentation`, code + PR in `StileEducation/dev-environment`.

## Core design: two repo roles

Today a single `(owner, repo)` pair is used for *everything*. The feature
splits it into two roles, derived fresh each run (no new persisted state):

- **Issue repo** — where the issue lives. Derived from the issue's `html_url`
  (already done: `parse_owner_repo` in `work_on`). May be a foreign,
  allowlisted repo. Used for: fetching the issue + its comments, posting
  comments / hand-off / ask, reaction & options watches, issue-thread events,
  and label sync **on the issue**.
- **Code repo** — where the work happens. The configured `main_repo`'s
  `(owner, repo)` (i.e. `config_repo()`); falls back to the issue repo when
  there is no config (preserving today's single-repo behaviour). Used for:
  worktree/branch creation, `default_branch`, push, PR find/create/fetch, PR
  URL, PR-thread comments & events, PR checks, review comments, and label sync
  **on the PR**.

When the two coincide (the common case — issue in the main repo, or no config)
behaviour is byte-for-byte what it is today.

## Step 1 — Config (`src/config.rs`)

Add to `Config`:

```rust
/// Repos whose issues may be worked on even though the code, worktree, and PR
/// live in `main_repo`. The configured repo is always allowed; this lists
/// *additional* issue-only repos. Empty by default. Each entry is either a
/// plain `"owner/repo"` string or a table with an optional `branch_prefix`.
#[serde(default)]
pub issue_repos: Vec<IssueRepo>,
```

`IssueRepo` accepts both forms via an untagged enum (string | table):

```rust
#[derive(Deserialize)]
#[serde(untagged)]
pub enum IssueRepo {
    /// `"owner/repo"` — prefix defaults to the repo name.
    Plain(String),
    /// `{ repo = "owner/repo", branch_prefix = "docs" }`. `branch_prefix`
    /// omitted → repo name; `""` → no prefix (collision risk accepted).
    Detailed { repo: String, branch_prefix: Option<String> },
}
```

`IssueRepo` exposes:
- `repo_ref() -> Result<RepoRef>` — parse/validate `"owner/repo"` (exactly one
  `/`, non-empty halves); a malformed entry is a hard config error.
- `branch_prefix() -> Option<&str>` carrying the *explicit* override only, so
  branch naming (Step 4) can distinguish "unset → use repo name" from
  `Some("")` → no prefix.

A `Config::issue_repo_refs() -> Result<Vec<RepoRef>>` helper (for the allowlist)
and a per-repo prefix lookup feed Steps 2 and 4.

Tests (mirror the existing `priority_labels` pair):
- plain-string entries parse; the detailed table form parses with and without
  `branch_prefix`; the two forms can be mixed in one list.
- absent key → empty vec (old configs keep loading).
- a malformed `"owner/repo"` entry errors.

## Step 2 — Resolution context: `IssueScope` (`src/github.rs`)

The allowlist must reach `issue_endpoint`, which is only reachable through
`fetch_issue` / `fetch_comments` / `post_issue_comment` / `resolve_issue_ref`.
Threading a second slice argument through all of them is noisy, so replace the
existing `config_repo: Option<&RepoRef>` parameter on those functions with a
single context value:

```rust
/// What an issue argument may resolve against.
pub struct IssueScope {
    /// Repo bare numbers resolve against (the configured `main_repo`); `None`
    /// with no config.
    pub primary: Option<RepoRef>,
    /// Additional repos whose issue *URLs* are accepted (`issue_repos`).
    pub allowed: Vec<RepoRef>,
}

impl IssueScope {
    /// True when `(owner, repo)` is the primary repo or any allowed repo
    /// (case-insensitive).
    pub fn is_allowed(&self, owner: &str, repo: &str) -> bool { /* … */ }
}

/// Build the scope from a discovered `ghwf.toml`: primary = config_repo(),
/// allowed = `Config::issue_repo_refs()`. Malformed `"owner/repo"` entries are
/// a hard config error (fail fast rather than silently dropping an allowlist
/// entry the user is relying on).
pub fn issue_scope() -> Result<IssueScope>;
```

- `issue_endpoint(arg, scope)`: bare numbers resolve against `scope.primary`
  (unchanged). For a URL, accept when `scope.is_allowed(owner, repo)`; otherwise
  keep rejecting — see Step 6 for the new message. Delete the `TODO`.
- `resolve_issue_ref`, `fetch_issue`, `fetch_comments`, `post_issue_comment`
  take `&IssueScope` instead of `Option<&RepoRef>`.
- Each call site currently does `let repo_ctx = github::config_repo()?;` then
  passes `repo_ctx.as_ref()`. Swap to `let scope = github::issue_scope()?;` and
  pass `&scope`. Call sites: `main.rs` (worktree_path, work_on, resolve_pr,
  create_issue_comment, hand_off, ask, create_issue paths), `launch.rs`,
  `wait.rs`. Keep `config_repo()` itself — it's still how we get the code repo.

## Step 3 — Thread the code repo through the workflow

In `work_on` (`src/main.rs:375`):

```rust
let scope = github::issue_scope()?;
let issue_data = github::fetch_issue(issue, &scope)?;
let (issue_owner, issue_repo) = github::parse_owner_repo(&issue_data.html_url)?;
// Code/PR repo: the configured main repo, or the issue repo when unconfigured.
let (code_owner, code_repo) = scope.primary.clone()
    .unwrap_or_else(|| (issue_owner.clone(), issue_repo.clone()));
```

Then split the existing uses of `(owner, repo)`:
- **Issue repo** (`issue_owner`/`issue_repo`): `fetch_comments`, issue-thread
  `collect_prompt_thumbs`, and the issue side of `labels::sync`.
- **Code repo** (`code_owner`/`code_repo`): `fetch_pr`, the PR URL strings
  (lines ~470), PR-thread comment fetch + `collect_prompt_thumbs`,
  `prep::run`, `implement::run`, `implement::review`, and the PR side of
  `labels::sync`.

Update the PR-side helpers to take the code repo:
- `prep::run(issue, code_owner, code_repo, number, …)` — `ensure_worktree`,
  `default_branch`, `find_pr`, `create_draft_pr` all then operate on the code
  repo (correct: the worktree is built from `main_repo`'s `origin/<default>`).
  The PR body keeps linking the issue by `issue.html_url` (already cross-repo
  safe).
- `ensure_worktree(issue, code_owner, code_repo, state)` (`src/prep.rs:16`) —
  fixes the latent bug where `default_branch(owner, repo)` used the issue repo
  while the worktree came from `main_repo`.
- `implement::run` / `implement::review` (`src/implement.rs`) — code repo (they
  only touch the PR: URL, checks, review comments).
- `launch.rs`: `resolve_issue_ref` via the scope; `resolve_workable` and
  `$GHWF_ISSUE` stay on the **issue** repo (sub-issues live with the parent);
  `ensure_worktree` / `refresh_main_repo` take the **code** repo.

## Step 4 — Branch / worktree naming (`src/state.rs:16`)

Two same-numbered issues in different repos must not collide on
`issue_<n>_<slug>` (one branch, one worktree dir = `worktrees_dir/<branch>`).
Qualify the branch only for foreign-repo issues; leave main-repo issues exactly
as today so existing worktrees are undisturbed. **The prefix is per-repo
configurable** (the user's choice — option 1):

- main-repo issue: `issue_<number>_<words>` (never prefixed).
- foreign-repo issue: `<prefix>_issue_<number>_<words>`, where `<prefix>` comes
  from the matching `issue_repos` entry:
  - entry has no `branch_prefix` (or is the plain string form) → prefix = the
    issue repo's **name**, sanitised like a slug word (e.g. `documentation`);
  - `branch_prefix = "docs"` → prefix = `docs` (also sanitised);
  - `branch_prefix = ""` → **no prefix**, i.e. bare `issue_<n>_<words>` (the
    user has opted into the collision risk for that repo).

`branch_and_slug` gains the resolved prefix (an `Option<&str>`: `None` for
main-repo / no-prefix, `Some(p)` otherwise). It returns the qualified branch but
the **unqualified** slug (the plan filename `plans/<number>-<slug>.md` lives
inside the per-branch worktree, so it can't collide across issues). The caller
resolves the prefix from config before calling; update both call sites in
`prep.rs` (`:32`, `:122`) so the branch used for the worktree and the branch
used elsewhere agree. Owner is omitted from the default prefix (within one org
the repo name disambiguates); a user who needs more can set `branch_prefix`
explicitly.

## Step 5 — Label sync (`src/labels.rs`)

`sync` currently loops `once(number).chain(pr_number)` with a single
`(owner, repo)`, so it would try to label the PR in the issue's repo. Change
`sync` to take the issue repo **and** the code repo (e.g.
`sync(issue_repo: &RepoRef, code_repo: &RepoRef, number, pr_number, state)`):
- issue thread → `sync_thread` on the **issue** repo
- PR thread (when `pr_number`) → `sync_thread` on the **code** repo

`sync_thread`, `desired_labels` are unchanged. Update the four `labels::sync`
call sites in `main.rs` (`:487`, `:873`, `:1619`, `:1720`) to pass both repos.

Label existence in the foreign repo: sync is best-effort (every failure is a
stderr warning), so a foreign repo lacking the `ghwf:*` labels just gets no
issue labels — no crash. To make setup complete, extend `configure_at`
(`src/labels.rs:171`, used by `ghwf config labels` and the init wizard) to also
create the default labels in each `issue_repos` entry, reporting per repo. The
`[labels]` section it writes is unchanged.

## Step 6 — The guidance (the issue's headline ask)

Rewrite the `issue_endpoint` rejection so it names the knob and shows the fix:

```
issue URL points at {owner}/{repo}, but ghwf.toml configures {cfg}.
To work on issues from {owner}/{repo} while the code, worktree, and PR stay in
{cfg}, add it to `issue_repos` in ghwf.toml:

    issue_repos = ["{owner}/{repo}"]
```

(`{cfg}` = `cfg_owner/cfg_repo`.) Remove the `TODO`.

## Step 7 — `wait` (`src/wait.rs`)

`run` builds issue-thread endpoints, reaction/options watches, and an events
feed all under one `(owner, repo)`. Split by role:
- issue-thread comments, reaction watches, options watches, and the issue
  repo's events feed → **issue** repo
- PR-thread comments and the **code** repo's events feed → **code** repo

When the repos differ this means polling two `events` feeds (issue repo + code
repo); when they coincide, collapse to one (today's behaviour) to avoid a
redundant request. `poll_endpoints` / `reaction_endpoints` / `options_endpoints`
take the repo they belong to. Verify `last_posted` / status-comment hiding
still keys correctly per thread.

## Step 8 — Wizard + README (required by `CLAUDE.md`)

- **`src/init.rs`**: add an optional-extras prompt (guarded by
  `!doc.contains_key("issue_repos")`), mirroring `priority_labels`: confirm,
  then read a comma-separated `owner/repo` list, validate each entry has exactly
  one `/` with non-empty halves, and write via a `set_issue_repos` helper
  (toml-edit array of plain strings). The wizard writes only the plain-string
  form; mention in the prompt/output that `branch_prefix` can be set by hand
  (documented in the README). Add a parse/round-trip test like
  `parse_priority_labels`.
- **`README.md`**: add an annotated `issue_repos` block to the `ghwf.toml`
  example (after `worktrees_dir`) showing **both** the plain-string and
  `{ repo = …, branch_prefix = … }` forms and what `branch_prefix` / `""` do,
  plus a short prose note in *Configuration* explaining the issue-repo vs
  code-repo split and the auto-close limitation below.

## Known limitations (documented, not solved here)

- **No cross-repo auto-close.** GitHub's `Closes #N` doesn't fire across repos,
  so a `documentation` issue won't auto-close when the `dev-environment` PR
  merges. The PR body links the issue by full URL, so the reference exists;
  closing is manual. Note in the README.
- **`ghwf next`** still discovers issues only in the configured repo; foreign
  issues are worked by passing their URL explicitly. Out of scope.
- **`ghwf create-issue`** files follow-ups in the code repo; its `blocked_by`
  link back to a foreign issue may not apply cross-repo. Out of scope; note if
  trivial.

## Testing

- `config.rs`: `issue_repos` parses both plain-string and table forms (mixed),
  with/without `branch_prefix`; defaults empty; malformed entry errors.
- `github.rs`: `IssueScope::is_allowed` (primary, allowed, case-insensitive,
  neither); `issue_endpoint` accepts an allowlisted URL, accepts the primary
  URL, resolves a bare number against primary, and rejects a non-allowlisted
  URL with the new message.
- `state.rs`: `branch_and_slug` — no prefix for the main repo; default
  (repo-name) prefix; explicit `branch_prefix`; and `Some("")`/no-prefix
  produces the bare `issue_<n>_<slug>` form.
- `init.rs`: `issue_repos` parse/round-trip.
- Keep all existing `labels.rs` tests green after the signature change.
- `cargo test` + `cargo clippy` clean; manual smoke: `ghwf work-on <foreign
  issue URL>` with the repo allowlisted creates a worktree/branch/PR in the main
  repo and posts to the foreign issue.

## Out of scope

PR-repo persistence in state (we re-derive the code repo from config each run,
matching how the issue repo is re-derived from `html_url`); cross-repo
`next`/auto-close/`blocked_by`.
