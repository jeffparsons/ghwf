# Plan — implicitly keep the default-branch worktree fresh (#24)

ghwf already fetches origin when it creates an issue worktree
(`prep::ensure_worktree`), but nothing ever advances the local checkout of the
default branch — in a bare-repo layout like this repo's (`repo.git` +
`worktrees/main`) the `main` worktree just goes stale. Goal: every time ghwf
fetches, opportunistically fast-forward whichever worktree has the default
branch checked out.

Decisions locked on the issue thread:

1. **Target** — the worktree (main or linked; git allows at most one per
   branch) that has the repo's default branch checked out, found via
   `git worktree list --porcelain`. None checked out → skip silently.
2. **When** — wherever a fetch happens: worktree creation in
   `ensure_worktree` (covers both the outside-Claude launcher and in-Claude
   prep-and-plan), plus a new fetch in the launcher's existing-worktree path,
   so every outside-Claude launch refreshes things.
3. **How** — only if the worktree has no staged or unstaged changes to
   tracked files (untracked files don't block), `git merge --ff-only
   origin/<default>` inside it. Dirty or non-fast-forwardable → warn and
   skip.
4. **Best-effort** — no failure in this step may break worktree creation or
   the Claude launch; everything degrades to a stderr warning.

## 1. Git helpers (`git.rs`)

- `branch_worktree(repo: &Path, branch: &str) -> Result<Option<PathBuf>>` —
  run `git worktree list --porcelain` and return the path of the worktree
  with `branch refs/heads/<branch>` checked out. Factor the porcelain parsing
  into a pure `fn parse_worktree_list(output: &str, branch: &str) ->
  Option<PathBuf>` so it can be unit-tested without git: blocks are separated
  by blank lines; take the `worktree <path>` line of the block whose
  `branch` line matches; bare and detached blocks have no `branch` line and
  never match.
- `is_tree_clean(dir: &Path) -> Result<bool>` — true when
  `git status --porcelain --untracked-files=no` prints nothing (tracked
  changes only, per decision 3; the existing `is_clean` is per-path and
  includes untracked, so it stays as is).
- `merge_ff_only(dir: &Path, ref_: &str) -> Result<()>` — `git merge
  --ff-only <ref_>`.

## 2. Shared update step (`prep.rs`)

```rust
/// Best-effort: fast-forward the worktree that has `default` checked out
/// to `origin/<default>`. Never fails — every skip or failure is at most
/// a stderr warning, so callers can't be broken by it.
pub fn update_default_worktree(main_repo: &Path, default: &str)
```

- `branch_worktree` finds the checkout; `None` → return silently.
- Dirty (`!is_tree_clean`) → `eprintln!` a one-line notice and return.
- `merge_ff_only(&wt, &format!("origin/{default}"))`: on success print a
  one-line narration (`Fast-forwarded `<path>` to origin/<default>.`) only
  when the merge actually moved the tip (compare `rev-parse HEAD` before and
  after, or detect git's "Already up to date." — simplest is comparing the
  tip to `origin/<default>` first and returning silently when they already
  match); on error (diverged) warn and return.
- Each helper error is caught here and downgraded to a warning.

Call it from `ensure_worktree` right after `github::default_branch` resolves
(the fetch has just happened, and `default` is in hand).

## 3. Launcher fetch for existing worktrees (`launch.rs`)

In `run`'s `Some(path)` arm (worktree already exists), before launching
Claude: locate the config with `config::find()` — the launcher works without
one in this path today, so `None` (or a config error) skips with a warning
rather than becoming a new hard requirement. With a config in hand:

- narrate the step (the launcher narrates everything it does),
- `git::fetch(&main_repo)` — failure warns and skips the update (offline
  launches must still work),
- `github::default_branch(&owner, &repo)` — likewise warn-and-skip on error,
- `prep::update_default_worktree(&main_repo, &default)`.

The create-worktree arm needs no change — `ensure_worktree` now does the
update internally.

## 4. Tests

- `git.rs`: unit tests for `parse_worktree_list` — bare block skipped,
  detached block skipped, match on the requested branch only, no match →
  `None`. Plus scratch-repo tests (real `git` in a temp dir, in the style of
  `launch.rs`'s scratch helper) covering `is_tree_clean` (clean / tracked
  modification / untracked-only) and `merge_ff_only` (fast-forwards;
  errors on divergence).
- `prep.rs`: scratch-repo test for `update_default_worktree` — builds an
  "origin" repo plus a clone with a `main` worktree, advances origin,
  fetches, and asserts the worktree moves to the new tip; asserts a dirty
  worktree is left untouched.

## Out of scope / punted

- Updating any branch other than the repo's default.
- Rebasing or merging a dirty/diverged checkout — we only ever fast-forward
  a clean one.
- Adding a fetch to in-Claude `work-on` runs that don't already fetch
  (phases after the worktree exists); only the launcher gains a new fetch.
