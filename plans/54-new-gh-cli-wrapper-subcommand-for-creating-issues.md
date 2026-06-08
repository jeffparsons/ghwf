# Plan: `ghwf create-issue` for follow-ups/deferrals (#54)

## Goal

When planning or implementing, a session often wants to spin off a follow-up
issue ("defer X", "discovered Y") without dropping it. Today that means reaching
for raw `gh`, which a workflow session can't do — only `Bash(ghwf:*)` is
allow-listed. Add a `ghwf create-issue` subcommand that creates a GitHub issue,
and advertise it to Claude so deferrals get filed instead of lost.

Per the issue discussion, the follow-up must be marked **blocked by** the
originating issue, **by default**, and the race against `next`-style workers
must be closed *fully*.

## Key constraints discovered (from the issue discussion)

- **No atomic "create-already-blocked" call.** GitHub's issues-create endpoint
  has no dependency field. The dependency can only be declared from the
  *blocked_by* side via `POST .../issues/{n}/dependencies/blocked_by` on the new
  issue (which must already exist); the *blocks* direction is read-only (GET
  only). So creating the dependency is unavoidably a second call after creation.
- **The native dependency can't be the race guard.** Between "create issue" and
  "set blocked_by" there is a window where the issue exists without the
  dependency. The local "already-started" state (`next.rs` `started_fn`) is
  per-machine, so it can't guard a cross-machine worker pool either.
- **The one signal that *can* be set atomically at creation is a label** (the
  create payload accepts `labels`). So a **blocked label, included in the create
  payload**, is the race-free guard: the issue is never visible without it. The
  native `blocked_by` dependency is set immediately afterwards as the
  human-facing truth in the GitHub UI.

## Scope boundary (confirmed with the user)

**In scope (this issue):** `create-issue`, which sets the blocked label
atomically at creation and then the native `blocked_by` dependency.

**Deferred to a separate issue (the user is tackling it):** teaching `ghwf next`
to *skip* issues carrying the blocked label, and the label's unblock lifecycle
(clearing it when the blocker closes). This issue only plants the guard at
birth; that issue consumes it.

## Subcommand surface

| Command | Purpose | Input |
| --- | --- | --- |
| `ghwf create-issue --title "<title>" [issue] [--label L]… [--no-block]` | Create a follow-up issue, blocked by the originating issue by default. | body on stdin |

- `--title <title>` (required): the new issue's title.
- Body: read from stdin (matching `create-issue-comment` / `update-pr` /
  `hand-off`). An empty body is allowed (issues can have empty bodies), so —
  unlike the comment commands — empty stdin is **not** an error.
- `[issue]` (optional positional): the *originating* issue, resolved the usual
  way (explicit arg → `$GHWF_ISSUE` → worktree). It is the blocker.
- `--label <name>` (repeatable): extra labels to attach, in addition to the
  blocked label.
- `--no-block`: create a standalone issue — no blocked label, no dependency.

Output: the created issue as JSON (number + html_url), like
`create-issue-comment` prints the created comment.

## Blocking semantics

Let *block* = `!no_block && originating issue resolves`.

- **Originating-issue resolution is best-effort when inferred, strict when
  explicit.** If `[issue]` is given explicitly but can't be resolved → hard
  error (the user named a specific blocker). If it's only inferred (env /
  worktree) and nothing resolves → **warn and skip blocking**, still creating
  the issue. With `--no-block`, skip resolution entirely.
- When *block*:
  1. Ensure the blocked label exists in the repo (create it if missing, as
     `labels` setup does), so the create payload's label actually sticks
     (GitHub silently drops unknown labels on issue creation).
  2. Create the issue with `labels = [blocked_label, ...--label]` in the
     payload → atomic guard.
  3. `POST .../issues/{new}/dependencies/blocked_by` with
     `{ "issue_id": <originating issue's **database id**> }` (the API wants the
     id, not the number).
- When not *block*: create with `labels = [...--label]` (possibly empty); no
  dependency.

The new issue is created **unassigned, with no workflow/phase labels**, so it
reads as fresh work.

## Implementation

### 1. Config: the blocked-label name (`src/config.rs`)

Add a standalone key with a default, so the guard works even when the optional
`[labels]` (phase/attention) feature is off:

```rust
/// Label applied to follow-up issues created by `ghwf create-issue` to mark
/// them blocked, included atomically in the create payload so a worker never
/// sees the issue unguarded. Defaults to "blocked".
#[serde(default = "default_blocked_label")]
pub blocked_label: String,
```

with `fn default_blocked_label() -> String { "blocked".into() }`. Add unit tests
mirroring the existing ones: explicit value parses; absent key defaults to
`"blocked"` (pre-existing configs keep loading).

Per the repo's "adding a config option" rule, also:
- **`src/init.rs`**: offer it in the wizard (a `set_blocked_label` writer +
  an `!doc.contains_key("blocked_label")` prompt, default `"blocked"`), with a
  round-trip test.
- **README**: document `blocked_label` in the annotated `ghwf.toml` example.

### 2. Issue model: expose the database id (`src/models.rs`)

`Issue` currently has no `id`. Add `pub id: u64;` (the REST database id). It is
needed for the `blocked_by` payload (originating issue's id) and is present on
both the single-issue fetch and the create response, so one model serves both.

### 3. GitHub helpers (`src/github.rs`)

Two new functions, following the `gh_api_stdin` POST pattern used by
`post_issue_comment` / `add_issue_labels`:

```rust
/// Create an issue, returning the created issue (number, id, html_url).
pub fn create_issue(
    owner: &str, repo: &str, title: &str, body: &str, labels: &[&str],
) -> Result<Issue>;
// POST repos/{owner}/{repo}/issues with { title, body, labels }.

/// Declare that `issue_number` is blocked by the issue with database id `blocker_id`.
pub fn add_blocked_by(
    owner: &str, repo: &str, issue_number: u64, blocker_id: u64,
) -> Result<()>;
// POST repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocked_by
//   with { issue_id: blocker_id }.
```

Reuse the existing `create_label` / `list_repo_labels` for the
ensure-label-exists step (a `ensure_label` helper, or inline: list, create with
a sensible default colour/description if absent — pick a colour distinct from
the phase labels, e.g. a muted grey, description "Blocked by another issue").

### 4. Command wiring (`src/main.rs`)

Add the `CreateIssue` variant to `Commands` (kebab-cased to `create-issue` by
clap) with `title: String` (`--title`, required), `issue: Option<String>`,
`label: Vec<String>` (`--label`), `no_block: bool` (`--no-block`); dispatch in
the match.

`fn create_issue(title, issue_arg, labels, no_block) -> Result<()>`:

1. Read body from stdin (empty allowed).
2. `repo_ctx = github::config_repo()?`; resolve the target repo
   (`github::repo_or_cwd()` — the new issue's home).
3. Resolve the originating issue *best-effort* (a softer variant of
   `resolve_issue_arg` that returns `Option` rather than bailing when nothing is
   inferable; an **explicitly passed** unresolvable arg still errors). When
   resolved, fetch it (`github::fetch_issue`) to get its database id and confirm
   the repo.
4. Decide `block` and assemble the label list (blocked label first when
   blocking, then `--label`s; de-duplicate).
5. When blocking: ensure the blocked label exists. Create the issue with the
   payload labels. Then `add_blocked_by(...)`. If the dependency POST fails
   *after* creation, don't lose the issue — warn (`eprintln!`) that the issue
   was created but couldn't be marked blocked, and still print it. (The label
   guard is already in place, so the worker-pool race stays closed even if the
   native dep didn't land.)
6. Print the created issue as JSON (add a small `render::issue_json` or reuse a
   serde serialization of `Issue`).

Resolution helper note: factor the inference half of `resolve_issue_arg` so both
it and the new best-effort variant share the env/worktree lookup.

### 5. Advertise to Claude

- **`src/install.rs` `SKILL_CONTENT`**: add a bullet, e.g. — "When you decide to
  defer work or discover something out of scope, **file it** with `ghwf
  create-issue --title \"…\"` (body on stdin) instead of dropping it; by default
  it's marked blocked by the issue you're working." Add an `install.rs` test
  asserting `SKILL_CONTENT.contains("create-issue")` (mirrors the existing
  skill-content assertions).
- **The installed `/work-on` SKILL.md is also checked into this repo's
  `~/.claude` copy**; `ghwf install` rewrites it, so only `SKILL_CONTENT` needs
  editing here.
- **README**: document `ghwf create-issue` under the proxied-commands section
  (alongside `show-pr` / `update-pr`), and the `blocked_label` config key.

## Testing

Unit tests (in-module, matching the codebase's style — there is no `tests/`
dir):
- `config.rs`: `blocked_label` parses; defaults to `"blocked"` when absent.
- `init.rs`: `set_blocked_label` round-trips through the typed `Config`.
- `github.rs`: the request-payload builders are the testable seam — if
  `create_issue` / `add_blocked_by` build their JSON via small pure helpers
  (as `pr_update_payload` does), unit-test those (title/body/labels shape;
  `issue_id` shape). Label de-duplication / "blocked label first" ordering as a
  pure helper test.
- `install.rs`: `SKILL_CONTENT` mentions `create-issue`.

The end-to-end `gh` calls themselves aren't unit-tested (consistent with the
rest of `github.rs`); manual verification against a scratch issue covers them.

## Manual verification

- `echo "body" | ghwf create-issue --title "Follow-up" 54` → new issue created,
  carries the blocked label from birth, shows "blocked by #54" in the GitHub UI,
  prints JSON.
- `--no-block` → plain issue, no label, no dependency.
- `--label foo` → blocked label + `foo`.
- Inferred-but-absent originating issue (run outside a worktree, no `$GHWF_ISSUE`)
  → warns and creates an unblocked issue rather than erroring.
- Explicit unresolvable `[issue]` → clean error.

## Out of scope / follow-ups

- Making `ghwf next` skip blocked-label issues, and clearing the label when the
  blocker closes — the user's separate issue. Call this out in the hand-off so
  the dependency between the two is explicit.
