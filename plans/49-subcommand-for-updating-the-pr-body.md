# Plan: Proxy the `gh` commands Claude needs (#49)

## Goal

`ghwf install` only pre-approves `Bash(ghwf:*)`, so a Claude session driving the
workflow can't touch the `gh` CLI without tripping a permission prompt. Today
that bites when Claude wants to rewrite the placeholder PR body
(`Plan for #N` + issue link, `src/prep.rs:162`) into a real description, but the
broader intent (from the issue discussion) is: **Claude should never need raw
`gh` for normal workflow work.** This plan adds `ghwf`-proxied subcommands
covering the realistic needs surfaced in the issue:

1. **Read** the PR's current title/body/state — the precursor to revising it.
2. **Write** the PR body and/or title.
3. **CI / check status**, including failing-job logs.
4. **Reply** to inline review comments.

All follow existing conventions: kebab-case subcommands, an optional `[issue]`
argument resolved the same way as elsewhere, bodies read from stdin, and every
GitHub call routed through the helpers in `src/github.rs`.

## Subcommand surface

| Command | Purpose | Input |
| --- | --- | --- |
| `ghwf show-pr [issue]` | Print the PR's number, title, state/draft, URL, and body. | — |
| `ghwf update-pr [issue] [--title T]` | Set the body (stdin) and/or title. | body on stdin |
| `ghwf pr-checks [issue] [--log-failed]` | Summarise CI check status; optionally dump failing-job logs. | — |
| `ghwf reply-review-comment [issue] --id N` | Reply to an inline review-comment thread. | body on stdin |

Resolving a review *thread* (mark resolved) is intentionally **deferred**: it
needs a GraphQL `resolveReviewThread` mutation against the thread node id (the
REST comment id won't do), which is a meaningfully different mechanism. Noted as
a follow-up, called out in the hand-off.

## Implementation

### 1. Shared PR resolution helper (`src/main.rs`)

The new commands all need `(owner, repo, pr_number)` and must fail cleanly when
there's no PR yet (pre-plan / `--no-branch`). Factor the resolution that
`hand_off` already does (`src/main.rs:1095`-ish) into a helper:

```rust
/// Resolve the issue arg to the PR backing its workflow, erroring clearly when
/// no PR exists yet.
fn resolve_pr(issue: &str) -> Result<(String, String, u64)> // (owner, repo, pr_number)
```

It runs `github::config_repo()`, `github::resolve_issue_ref`, then
`state::find_workflow_issue` to map a PR-thread arg back to the workflow issue,
reads `issue_state.prep.and_then(|p| p.pr_number)`, and bails with a helpful
message ("no PR for issue #N yet — it's created in prep-and-plan") when absent.
`hand_off` keeps its own richer flow (it needs phase/attention state); this
helper serves the four new read/write commands.

### 2. PullRequest model (`src/models.rs:27`)

Add the fields the read path needs (the `gh api .../pulls/{n}` response already
carries them):

```rust
pub title: String,
#[serde(default)]
pub body: Option<String>,   // GitHub sends null for an empty body
pub head: Head,             // { r#ref: String, sha: String } via #[serde(rename = "ref")]
```

`head.ref` (the branch) and `head.sha` feed the CI commands. Existing
`fetch_pr` needs no change — it already deserialises the full object.

### 3. `show-pr` (`src/github.rs` + `src/main.rs`)

`fetch_pr` already returns everything. Add `fn show_pr(issue)` in main that calls
`resolve_pr`, `github::fetch_pr`, and prints a plain-text block Claude can read:

```
#<n> <title>  (<state>, draft|ready)
<html_url>

<body or "(no body)">
```

Format via a small pure helper `render::pr_overview(&PullRequest) -> String` so
it's unit-testable.

### 4. `update-pr` (`src/github.rs` + `src/main.rs`)

Add to `github.rs`:

```rust
pub fn update_pr(owner, repo, pr, title: Option<&str>, body: Option<&str>) -> Result<()> {
    // PATCH repos/{owner}/{repo}/pulls/{pr} with only the provided fields,
    // via gh_api_stdin (JSON on stdin — no shell escaping), mirroring
    // post_issue_comment / add_issue_labels.
}
```

`fn update_pr(issue, title)` in main reads stdin (as `create_issue_comment`
does), trims, and treats empty stdin as "no body change". It errors if neither a
non-empty body nor `--title` was supplied. The PR body is written **verbatim**
(no "Claude says" header — it's the PR description, not a conversation comment),
respecting the global no-hard-wrap PR-body convention by leaving the text as
given. Prints a one-line confirmation with the PR URL.

Note: to change *only* the title, the caller passes empty stdin
(`ghwf update-pr 49 --title "…" </dev/null`); documented in the command's help
and the README.

### 5. `pr-checks` (`src/github.rs` + `src/main.rs`)

CI status combines check-runs and legacy commit statuses; `gh pr checks` already
does that aggregation and is what a human/Claude would run. ghwf calling `gh` is
fine (the allow-list concern is only about *Claude* calling it), so wrap the
porcelain rather than reimplementing the aggregation over REST:

- Status: run `gh pr checks <pr> -R owner/repo` and print its output. `gh`
  exits non-zero when checks are failing/pending — treat that as a normal,
  reportable state (print the table, don't surface it as a ghwf error); only a
  genuine invocation failure (e.g. gh missing) is an error. A dedicated helper
  `github::pr_checks(owner, repo, pr) -> Result<ChecksReport>` captures stdout +
  the failing/pending signal.
- `--log-failed`: fetch the PR head branch via `fetch_pr().head.ref`, list its
  recent workflow runs (`gh run list -b <branch> -R owner/repo --json
  databaseId,conclusion,status,workflowName -L <small N>`), and for each failed
  run run `gh run view <id> --log-failed -R owner/repo`, printing each under a
  header. If nothing failed, say so.

Keep the CI helpers in `github.rs` next to the other `gh` wrappers; add a small
`gh_capture` that returns `(status, stdout, stderr)` without bailing on
non-zero, since `pr-checks` needs the non-zero exit as data.

### 6. `reply-review-comment` (`src/github.rs` + `src/main.rs`)

```rust
pub fn reply_review_comment(owner, repo, pr, comment_id, body) -> Result<ReviewComment> {
    // POST repos/{owner}/{repo}/pulls/{pr}/comments/{comment_id}/replies
    // body as JSON on stdin via gh_api_stdin.
}
```

`fn reply_review_comment(issue, id)` in main reads stdin, errors on empty, wraps
the text with `render::build_comment_body` (so the reply carries the "Claude
says" header + session tag, matching conversation comments and keeping
authorship clear), posts the reply, and prints the created-comment JSON like
`create_issue_comment` does. The inline-comment ids needed here are already
surfaced in `work-on` output (`ReviewCommentView`), so Claude has the id to
target.

### 7. CLI wiring (`src/main.rs`)

Add four variants to `enum Commands` (`src/main.rs:39`) with doc comments
matching the house style, and their match arms in `main` (`src/main.rs:175`-ish).
Each takes `issue: Option<String>` resolved via `resolve_issue_arg`, plus:
`update-pr` → `--title: Option<String>`; `pr-checks` → `--log-failed: bool`;
`reply-review-comment` → `--id: u64` (required).

### 8. Skill + docs

- **Skill** (`src/install.rs` `SKILL_CONTENT`): add a short bullet pointing Claude
  at these subcommands for PR body/title, CI status, and review replies, so it
  reaches for `ghwf` instead of `gh` (the skill only allow-lists `Bash(ghwf:*)`,
  which already covers them — no permission change needed). This is the behaviour
  nudge that makes the proxy actually get used.
- **README**: add a "## Proxied GitHub commands" section documenting the four
  subcommands and the rationale (Claude never needs raw `gh`), including the
  `</dev/null` title-only note. The project `CLAUDE.md` doc rule is specific to
  *config options*, so it doesn't apply here, but the README is the natural home
  for the command reference.

## Testing

Most new code is thin `gh` I/O that isn't unit-testable, but the pure pieces get
tests (mirroring the existing `#[cfg(test)]` blocks in `github.rs`/`main.rs`):

- `render::pr_overview` formatting: with a body, with an empty/`None` body, and
  draft vs ready state.
- `update_pr` payload construction: body-only, title-only, both — assert the JSON
  contains exactly the provided fields and omits the others.
- `update-pr` validation: neither body nor title supplied → error.
- `parse_owner_repo`/resolution reuse is already covered; no new tests there.

Manual smoke test once the draft PR for this very issue exists: `show-pr`,
`update-pr` to give this PR a real description, `pr-checks` after a push, and a
`reply-review-comment` against any inline comment.

## Out of scope / follow-ups

- Resolving review threads (GraphQL `resolveReviewThread`) — different mechanism;
  file as a follow-up if it's wanted.
- Anything beyond the four needs identified in the issue (e.g. creating reviews,
  merging) — not part of the normal Claude workflow loop.
