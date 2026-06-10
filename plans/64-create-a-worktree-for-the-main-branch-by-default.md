# Plan for #64: Create a worktree for the main branch by default

## Goal

After `ghwf clone`, automatically create a worktree for the repo's default
branch under `worktrees/`, named after that branch (e.g. `worktrees/main`).
Today `ghwf clone` leaves `worktrees/` empty.

## Why this fits

- The single-branch bare clone already creates exactly one local branch — the
  default branch (`refs/heads/main`). So the worktree just needs to check out
  that existing branch.
- `prep.rs::update_default_worktree` already fast-forwards "the worktree that
  has the default branch checked out" on each fetch. Today that's a no-op after
  a fresh clone because no such worktree exists. Creating one by default gives
  that machinery something to keep current, and gives the user a convenient spot
  to inspect the default branch.

## Decisions (from pre-plan hand-off, approved)

1. **No opt-out flag.** "By default" is the standard behaviour, not a hint at a
   `--no-main-worktree` flag. Keeps `ghwf clone` flag-free; a flag can be added
   later if wanted.
2. **Best-effort.** If creating the default-branch worktree fails, warn rather
   than fail the whole clone — the core layout (bare repo + config) is already
   in place by then.

## Changes

### 1. `src/git.rs` — new helper for an existing branch

`add_worktree` uses `-b <branch>` to create a *new* branch, which would fail on
the already-existing default branch. Add a sibling helper that checks out an
existing branch:

```rust
/// Create a worktree at `path` checked out on the existing local `branch`
/// (unlike [`add_worktree`], which creates a new branch with `-b`).
pub fn add_worktree_for_branch(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    git(repo, &["worktree", "add", path, branch]).map(|_| ())
}
```

Add a unit test in `git.rs`'s test module: init a repo, add a worktree for the
existing `main` branch, assert the path exists and that
`branch_worktree(repo, "main")` reports it.

### 2. `src/clone.rs` — create the worktree during `populate`

In `populate`, after `setup_conventional_remote(&bare)` and after the
`worktrees` directory is created, read the default branch and add its worktree:

```rust
std::fs::create_dir(container.join("worktrees"))
    .context("failed to create the worktrees directory")?;
create_default_worktree(container, &bare)?;
```

New helper, best-effort (warn, don't fail):

```rust
/// Best-effort: check out the default branch into `worktrees/<default>`, so a
/// fresh clone has a ready place to view and update the default branch (and so
/// `update_default_worktree` has a checkout to keep current). A failure here is
/// a warning, not a clone failure — the bare repo and config are already in
/// place.
fn create_default_worktree(container: &Path, bare: &Path) -> Result<()> {
    let default = git::default_remote_branch(bare)?;
    let worktree = container.join("worktrees").join(&default);
    git::add_worktree_for_branch(bare, &worktree, &default)?;
    Ok(())
}
```

Wrap the call so failure only warns:

```rust
if let Err(err) = create_default_worktree(container, &bare) {
    eprintln!("warning: failed to create the default-branch worktree: {err:#}");
}
```

(Return type of `populate` stays `Result<()>`; only the genuine layout steps can
fail it.)

### 3. `src/clone.rs` — update `report`

The `report` function describes the created layout. Add a line for the new
worktree and adjust the `worktrees/` description. It needs the default branch
name; thread it through from `run`/`populate` (e.g. have `populate` return the
default branch name, or re-read it in `run`). Simplest: have `populate` return
`Result<String>` with the default branch, and pass it to `report`.

Updated layout description, e.g.:

```
- `worktrees/` — per-issue worktrees are created here
  - `main/` — a checkout of the default branch, ready to use
```

If the worktree wasn't created (the best-effort step warned), `report` should
not claim it exists. Track whether it was created (e.g. `populate` returns
`Option<String>` — `Some(default)` when the worktree was made, `None` when it
was skipped) and have `report` include the worktree line only when present.

### 4. `README.md` — document the new layout

Update the `ghwf clone` section (around the layout tree near line 203):

```
├── repo.git/          # bare repo, remote configured like a normal clone's
├── ghwf.toml          # generated config (essentials only)
└── worktrees/         # per-issue worktrees land here
    └── main/          # checkout of the default branch, created by clone
```

Adjust surrounding prose so it mentions the default-branch worktree.

## Tests

- `git.rs`: new test for `add_worktree_for_branch` (worktree created on the
  existing branch; `branch_worktree` finds it).
- `clone.rs`: extend `populate_builds_a_working_layout` (or add a focused test)
  to assert `worktrees/main` exists, is a non-bare checkout
  (`rev-parse --is-bare-repository` → `false`), and has the default branch
  checked out (`branch_worktree(&bare, "main")` points at it). The existing
  `fixture_origin` has `main` + `extra`; the single-branch clone keeps only
  `main` locally, so the default-branch worktree will be `main`.

## Out of scope

- An opt-out flag (`--no-main-worktree`) — deferred per decision 1.
- Changing how `prep.rs` creates per-issue worktrees.
