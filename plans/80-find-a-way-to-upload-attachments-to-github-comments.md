# Upload attachments to GitHub comments

## Goal

Let a Claude session attach local files (screenshots, diagrams, logs,
diffs) to the comments ghwf posts, via a repeatable `--attach <PATH>`
argument on the comment-posting commands. Each file is uploaded into the
repo and referenced from the comment body — an image inline where the
repo's visibility allows it, otherwise as a clickable link.

## Background

GitHub has **no official API** for uploading attachments to comments
(confirmed mid-2026). The web UI's drag-and-drop hits a private endpoint
(`/upload/policies/assets`) that only accepts a browser `user_session`
cookie — a PAT gets HTTP 422 — so `gh`'s token auth (all ghwf uses)
can't drive it. The user picked the robust, token-only alternative
(issue #80 options question): **commit the file into the repo via the
Git Data API and embed a link.**

Caveat that shapes the design: on a **private** repo (this repo is
private), `raw`/`blob?raw=true` links are auth-gated, so GitHub's image
proxy won't render them inline — they show as clickable links. On a
**public** repo, `?raw=true` blob links render inline. So image vs link
rendering is chosen per repo visibility.

Today both comment-posting paths build the body the same way:
`render::build_comment_body(user_body, token)` (`src/render.rs:96`)
prefixes the "**Claude says:**" header and appends the hidden session
marker last. `create_issue_comment` (`src/main.rs:1448`) reads stdin,
resolves the repo, posts via `github::post_issue_comment`. `hand_off`
(`src/main.rs:1616`) does the same but appends the phase advance prompt
to the user body before building, and posts to the phase's primary
thread (issue or PR). All GitHub calls go through `gh api` wrappers in
`src/github.rs` (`gh_api`, `gh_api_stdin`).

## Design decisions

- **Where the bytes go.** A dedicated `ghwf-attachments` branch with its
  own orphan history (first commit has no parents, tree holds only
  attachments), so attachments never appear in the working branch, the
  PR diff, or anyone's normal checkout, and the branch stays tiny.
- **Which repo.** The repo that hosts the thread the comment body is
  posted to, so the link lives alongside the conversation and is
  accessible to anyone reading it. For `create-issue-comment` that's the
  issue's repo; for `hand-off` it's the repo of the *primary* thread
  (the PR's code repo when the primary thread is the PR, else the issue
  repo). The secondary-thread stub carries no attachments.
- **One commit per comment.** All `--attach` files for a single comment
  are batched into one Git Data commit (one tree with N blob entries).
- **Path naming.** `attachments/<issue>/<hash8>-<safe-name>`, where
  `hash8` is the first 8 hex chars of the file's SHA-256 and `safe-name`
  is the basename with anything outside `[A-Za-z0-9._-]` replaced by `_`.
  This is deterministic: re-attaching identical content lands on the same
  path (idempotent), and the hash prevents collisions between different
  files that share a name.
- **Rendering.** Per file: an image (`png/jpg/jpeg/gif/webp/svg/bmp/apng`
  by extension) in a **public** repo → inline `![name](blobUrl?raw=true)`;
  everything else (non-images, and images in private repos) →
  `[name](blobUrl)`. `blobUrl` is
  `https://github.com/<owner>/<repo>/blob/ghwf-attachments/<path>`.
- **Failure ordering.** Validate and upload *before* posting, so a failed
  upload never leaves a comment with broken links.
- **No new config key.** The branch name is a constant
  (`ghwf-attachments`); nothing is added to `Config`, so the CLAUDE.md
  "adding a config option" checklist doesn't apply.

## Changes

### 1. `Cargo.toml` — add base64

The Git blobs endpoint takes `{ "content": <base64>, "encoding": "base64" }`.
Add `base64 = "0.22"`. (`sha2` is already a dependency, used for hashing.)

### 2. `src/github.rs` — Git Data API primitives

Add thin `gh api` wrappers, mirroring the existing helper style (build an
endpoint, send JSON on stdin via `gh_api_stdin`, parse the field needed):

- `fn create_blob(owner, repo, content_base64: &str) -> Result<String>` —
  `POST repos/{o}/{r}/git/blobs` with `{content, encoding:"base64"}`,
  returns the blob SHA.
- `fn get_branch_tip(owner, repo, branch) -> Result<Option<(String, String)>>` —
  `GET repos/{o}/{r}/git/ref/heads/{branch}`; returns
  `Some((commit_sha, tree_sha))` (fetching the commit for its tree via
  `git/commits/{sha}`), or `None` when the ref 404s (branch absent). Use
  `gh_capture` so a 404 is data, not an error.
- `fn create_tree(owner, repo, base_tree: Option<&str>, entries: &[TreeEntry]) -> Result<String>` —
  `POST repos/{o}/{r}/git/trees`; `entries` carry `{path, mode:"100644",
  type:"blob", sha}`. Returns the tree SHA.
- `fn create_commit(owner, repo, message, tree, parents: &[String]) -> Result<String>` —
  `POST repos/{o}/{r}/git/commits`. Returns the commit SHA.
- `fn create_ref(owner, repo, branch, sha) -> Result<()>` —
  `POST repos/{o}/{r}/git/refs` with `{ref:"refs/heads/{branch}", sha}`.
- `fn update_ref(owner, repo, branch, sha) -> Result<bool>` —
  `PATCH repos/{o}/{r}/git/refs/heads/{branch}` with `{sha, force:false}`;
  returns `false` on a non-fast-forward rejection (422) so the caller can
  retry, `true` on success, bail on other errors.
- `fn repo_is_private(owner, repo) -> Result<bool>` — wraps
  `gh repo view <owner>/<repo> --json isPrivate --jq .isPrivate`.

A small `TreeEntry` struct (or inline `serde_json::json!`) for the tree
payload.

### 3. `src/attach.rs` — new module (orchestration + pure helpers)

Register `mod attach;` in `src/main.rs`.

Public entry point:

```rust
/// Upload `paths` into `(owner, repo)` on the attachments branch and
/// return the markdown trailer to append to a comment body, or `None`
/// when `paths` is empty.
pub fn upload(owner: &str, repo: &str, issue: u64, paths: &[PathBuf]) -> Result<Option<String>>
```

Steps:

1. Return `Ok(None)` if `paths` is empty.
2. For each path: read the bytes (clear error if missing / not a regular
   file), enforce a soft size cap (e.g. bail above 25 MB — base64 in JSON
   is heavy and the blobs API has limits), compute `repo_path` via the
   naming scheme, base64-encode. Dedupe entries that resolve to the same
   `repo_path` (identical content+name).
3. Upload as one commit: create a blob per file, read the branch tip
   (`get_branch_tip`), `create_tree` (with `base_tree` = tip's tree when
   present, else none), `create_commit` (parent = tip commit when present,
   else none — the orphan case), then `update_ref` if the branch existed
   or `create_ref` if it didn't. On a non-fast-forward `update_ref`
   (concurrent poster), re-read the tip and rebuild once before bailing.
4. Resolve visibility once via `github::repo_is_private`, build each
   file's markdown line, and assemble the trailer:

   ```
   ---

   **Attachments:**

   - ![diagram.png](https://github.com/o/r/blob/ghwf-attachments/attachments/80/ab12cd34-diagram.png?raw=true)
   - [server.log](https://github.com/o/r/blob/ghwf-attachments/attachments/80/0f9e8d7c-server.log)
   ```

Pure helpers (each unit-tested — see §6):

- `sanitize_filename(name: &str) -> String`
- `repo_path(issue: u64, content: &[u8], name: &str) -> String`
- `is_image(name: &str) -> bool`
- `attachment_markdown(name: &str, url: &str, image: bool, private: bool) -> String`
- `build_trailer(lines: &[String]) -> String`

### 4. `src/main.rs` — wire `--attach` into the two commands

- `CreateIssueComment` variant: add
  `#[arg(long = "attach")] attach: Vec<PathBuf>` with a doc comment
  ("Attach a local file (repeatable); uploaded to the repo and linked
  from the comment."). Thread it through the dispatch arm
  (`src/main.rs:313`) into `create_issue_comment`.
- `HandOff` variant: add the same `attach: Vec<PathBuf>` field; thread
  through the dispatch arm (`src/main.rs:320`) into `hand_off`.
- `create_issue_comment(issue, attach)`: after resolving the repo,
  resolve `(owner, repo, number)` via `github::resolve_issue_ref` and call
  `attach::upload(&owner, &repo, number, &attach)?`; append the returned
  trailer to `user_body` before `build_comment_body`.
- `hand_off(issue, question, attach)`: compute the upload target to match
  the *primary* thread — `github::code_repo(&(owner,repo))` when
  `primary_is_pr`, else `(owner, repo)` — call `attach::upload(...)`, and
  fold the trailer into the user body *before* the existing prompt-append
  logic so the advance prompt stays last.

Empty `attach` ⇒ `upload` returns `None` ⇒ behaviour is byte-for-byte
unchanged from today.

### 5. Documentation

- `README.md` — where `create-issue-comment` and `hand-off` are
  introduced (Phases section, ~lines 16 & 45–48), note the `--attach
  <path>` flag, that files are committed to a `ghwf-attachments` branch,
  and the private-repo caveat (images link rather than render inline).
- `src/install.rs` — the `/work-on` skill body references
  `create-issue-comment` and `hand-off`; add a short mention that either
  accepts `--attach <path>` so future sessions know the capability
  exists. Skim `src/render.rs` phase banners for the same command names
  and mention `--attach` if it reads naturally there.

### 6. Tests (`src/attach.rs` unit tests)

Pure-function coverage (network paths stay untested, consistent with the
codebase):

- `sanitize_filename`: spaces/slashes/colons → `_`; safe chars kept.
- `repo_path`: deterministic for given content+name; differing content
  changes the hash prefix; includes the issue number.
- `is_image`: true for the image extensions (case-insensitive), false for
  `.log`/`.txt`/`.patch`/no extension.
- `attachment_markdown`: image+public → `![…](…?raw=true)`;
  image+private → `[…](…)` (no `?raw=true`, no `!`); non-image → `[…](…)`
  regardless of visibility.
- `build_trailer`: produces the `---` / `**Attachments:**` / bulleted
  structure and is omitted (caller passes `None`) when there are no files.

## Out of scope

- The cookie-based `user-attachments` upload flow (true inline rendering
  on private repos) — rejected in the options question as too fragile for
  an unattended tool.
- `reply-review-comment` and `ask` gaining `--attach` — can follow later
  if wanted; this issue targets the comment-posting commands.
