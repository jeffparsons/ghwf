# Plan: `ghwf clone` — set up the preferred layout (#40)

## Summary

A new `ghwf clone <repo> [directory]` subcommand that clones a GitHub repo
into ghwf's preferred layout:

```
<repo-name>/            # container dir under cwd ([directory] overrides)
├── <repo-name>.git/    # bare repo, remote configured like a normal clone
├── ghwf.toml           # generated, essentials only
└── worktrees/          # created empty
```

`<repo>` is `owner/repo` shorthand or a full GitHub URL (HTTPS or SSH). An
optional `--reference <path>` borrows objects from an existing local clone so
big repos don't re-download everything; the clone is always dissociated
afterwards, so the new repo never depends on the old one (the old clone is
exactly what a migrating user is likely to delete).

No in-place conversion of an existing clone — decided in the issue thread.

## 1. CLI surface (`main.rs`)

New variant in `Commands`, alongside the others:

```rust
/// Clone a GitHub repo into ghwf's preferred layout: a container
/// directory holding the bare repo, a generated `ghwf.toml`, and an
/// empty worktrees directory.
Clone {
    /// The repo to clone: `owner/repo` or a full GitHub URL
    /// (HTTPS or SSH).
    repo: String,
    /// Directory to create (the container). Defaults to the repo name
    /// under the current directory.
    directory: Option<PathBuf>,
    /// Borrow objects from this existing local clone instead of
    /// fetching them from the network (passed to `git clone
    /// --reference`); the new repo is dissociated from it afterwards.
    #[arg(long)]
    reference: Option<PathBuf>,
},
```

Dispatch: `Commands::Clone { repo, directory, reference } =>
clone::run(&repo, directory.as_deref(), reference.as_deref())`. Unlike most
subcommands it takes no issue argument and needs no session — it runs before
any ghwf.toml exists.

## 2. New module `src/clone.rs`

`pub fn run(repo: &str, directory: Option<&Path>, reference: Option<&Path>)
-> Result<()>`, in these steps:

### 2.1 Resolve the clone URL and repo name

- If the argument looks like a URL (contains `://` or starts with `git@`),
  use it verbatim. Reuse `github::parse_remote_url` (currently private —
  make it `pub(crate)`) to validate it's a GitHub remote and extract
  `owner/name`; its tests already cover the SSH and HTTPS shapes.
- Otherwise treat it as `owner/repo` shorthand and build the URL with the
  user's preferred protocol, mirroring what `gh repo clone` would do: read
  `gh config get git_protocol` (default `https` when unset/errored) and
  construct `git@github.com:owner/repo.git` or
  `https://github.com/owner/repo.git`. The construction itself is a pure
  function (`shorthand_url(protocol, owner, repo)`) for testability.
- The repo name (last path segment, `.git` suffix stripped) names the
  default container dir and the bare repo dir.

Plain `git clone` rather than `gh repo clone`: it keeps the clone step
testable against local fixture repos (no GitHub round-trip), and avoids
gh's fork-upstream magic. The protocol lookup is the only `gh` touch.

### 2.2 Create the container directory

- Target: `directory` when given, else `./<name>`.
- An existing *empty* directory is fine (matching `git clone`'s rule); an
  existing non-empty directory or file is an error before anything runs.
- Remember whether we created it: if any later step fails, best-effort
  remove a directory *we* created (never a pre-existing one), so a failed
  run leaves nothing half-built.

### 2.3 Clone the bare repo

```
git clone --bare --single-branch <url> <container>/<name>.git \
    [--reference <canonicalized path> --dissociate]
```

- `--single-branch` keeps the bare repo's `refs/heads/*` down to just the
  default branch. A plain `--bare` clone mirrors *every* remote branch into
  `refs/heads/*`, where they sit frozen forever (fetch `--prune` only tends
  `refs/remotes/*`) and pollute `git::list_local_branches` — which
  `collect-garbage` reads (`src/collect_garbage.rs`).
- `--reference` is canonicalized first (git resolves it against the
  command's cwd; an absolute path keeps the error messages and the
  temporary alternates file unambiguous). `--dissociate` always accompanies
  it — see Summary.
- Run via a new helper in `src/git.rs` (the existing `git()` helper is
  `-C <dir>`-shaped and module-private; add e.g.
  `pub fn clone_bare(url: &str, dest: &Path, reference: Option<&Path>) ->
  Result<()>` next to it, running with `-C <container>` or absolute paths).

### 2.4 Make the remote behave like a normal clone's

A bare clone is the odd one out: `--single-branch` leaves
`remote.origin.fetch` as `+refs/heads/<default>:refs/heads/<default>` and
creates no remote-tracking refs — but ghwf requires them:
`prep.rs:34-41` runs `git fetch --prune origin` and creates worktrees from
`origin/<default>`, and `collect-garbage` reads
`refs/remotes/origin/*`. So, in the bare repo:

```
git config remote.origin.fetch '+refs/heads/*:refs/remotes/origin/*'
git fetch --prune origin            # populates refs/remotes/origin/*
git remote set-head origin --auto   # creates refs/remotes/origin/HEAD
```

After this, `origin/main` etc. resolve exactly as in a working-copy clone,
and every later `git fetch` keeps them fresh.

### 2.5 Write `ghwf.toml` and `worktrees/`

`<container>/ghwf.toml`, essentials only (per the issue thread):

```toml
main_repo = "<name>.git"
worktrees_dir = "worktrees"
```

Then create `<container>/worktrees/`. Round-trip the generated text through
`toml::from_str::<config::Config>` in a unit test so the file can never
drift from what `config::find` accepts.

### 2.6 Report and point at the wizard

Print the created layout (container path, bare repo, config, worktrees
dir) and next steps:

- `cd <container>`
- `ghwf config init` — the interactive wizard (#36) — for the optional
  extras: priority labels, PR instructions, workflow status labels.
- `ghwf work-on <n>` / `ghwf next` to start working.

(#36 is in flight; if it lands after this, the pointer is forward-looking
prose either way and needs no code coupling.)

## 3. Tests

House style: pure-function unit tests plus real-git integration tests using
`git::tests::{scratch, run_git, init_repo}` fixtures.

- **URL/name resolution** (`clone.rs` unit tests): `owner/repo` shorthand →
  HTTPS and SSH forms via `shorthand_url`; full URLs pass through verbatim;
  repo name derivation strips `.git` and takes the last segment for both
  arg shapes; garbage (`no-slash`, empty segments) errors.
- **Generated config parses**: the ghwf.toml text deserializes as
  `config::Config` with the expected `main_repo`/`worktrees_dir`.
- **End-to-end against a local origin** (real git, no network): build a
  fixture repo with a second branch, run the post-URL-resolution pipeline
  (clone + remote setup + file generation) into a scratch container, then
  assert:
  - the bare repo exists at `<name>.git` and is bare;
  - `refs/remotes/origin/main` and the second branch resolve;
  - `refs/heads/*` holds only the default branch (`--single-branch` did its
    job);
  - `remote.origin.fetch` is the conventional refspec;
  - `git worktree add -b x <path> origin/main` succeeds from the bare repo
    — the operation prep-and-plan actually performs;
  - `ghwf.toml` and `worktrees/` exist.
- **`--reference`**: clone a fixture origin with `--reference` to a second
  local clone of it; assert success and that `objects/info/alternates` does
  not exist afterwards (dissociated).
- **Target collision**: existing empty dir succeeds; non-empty dir errors
  without touching its contents; a failed clone (bogus URL) removes the
  container dir it created but leaves a pre-existing empty one in place.

## 4. README

Add a "Setting up a project" section near the top of the configuration
docs: one `ghwf clone owner/repo` invocation, the resulting tree, a note on
`--reference` for big repos, and the `ghwf config init` follow-up. Trim the
existing config section's manual-setup framing to "what `ghwf clone`
generates / what you'd write by hand for other layouts" — other layouts
remain fully supported; this command is just the opinionated default.

## Out of scope (noted for follow-ups)

- In-place conversion of an existing clone (explicitly declined in the
  issue thread).
- Non-GitHub remotes (ghwf is GitHub-specific throughout).
- A `--no-dissociate` escape hatch for `--reference`; add later if a
  use-case appears.
