# Fix #136: `convert` leaves a broken default-branch worktree

## Problem

`ghwf convert` builds the entire new layout — the bare repo **and** the
default-branch worktree (`worktrees/<default>`) — inside a scratch sibling
directory `<name>.ghwf-converting`, then swaps it into the original path with two
`rename`s (`swap_into_place` in `src/convert.rs`).

When `git worktree add` creates `worktrees/<default>`, git records **absolute**
paths in its administrative files:

- the bare repo's `worktrees/<default>/gitdir`, pointing at the worktree's `.git`;
- the worktree's `.git` file, pointing back at the bare repo's worktree admin dir.

Both bake in the scratch location. The rename moves the files but doesn't rewrite
those paths, so after the swap every pointer still references
`<name>.ghwf-converting/...`, which no longer exists:

```
fatal: not a git repository: .../ghwf.ghwf-converting/ghwf.git/worktrees/main
git worktree list   # worktree shown at the stale .ghwf-converting path, "prunable"
```

`ghwf clone` is unaffected because it builds directly in the final location (no
rename), so the absolute paths are already correct. The bug is specific to
convert's build-in-temp-then-swap design.

The existing convert tests missed it because they only assert the worktree's
*files* exist on disk (which survive the rename) — they never run a git
operation inside the worktree.

## Fix

After the swap succeeds, repair the worktree's path pointers with
`git worktree repair <new-worktree-path>`, run from the bare repo. Verified
end-to-end: passing the worktree's new absolute path repairs both directions of
the pointer, after which `git worktree list` and `git status` inside the worktree
work correctly.

This is only needed when the default-branch worktree was actually created.
`clone::populate` already returns `Option<String>` — the default branch name, or
`None` when the worktree step was skipped (e.g. it warned and continued). When it
is `None`, there is no worktree to repair.

### Changes

1. **`src/git.rs`** — add a helper:

   ```rust
   /// Repair the administrative files of the worktree at `path` so its links to
   /// `repo` (and back) are correct after the worktree or the repo has moved on
   /// disk. `git worktree add` records absolute paths, so a layout built in one
   /// directory and then renamed needs this to relink.
   pub fn repair_worktree(repo: &Path, path: &Path) -> Result<()> {
       let path = path.to_str().context("worktree path is not valid UTF-8")?;
       git(repo, &["worktree", "repair", path]).map(|_| ())
   }
   ```

2. **`src/convert.rs`** — in `build_and_swap`, after `swap_into_place(plan)?`
   succeeds, repair the default worktree when one was created:

   ```rust
   swap_into_place(plan)?;
   if let Some(default) = &default {
       // `git worktree add` baked the scratch path into the worktree's admin
       // files; the swap moved them but not the paths they record, so relink
       // them to the worktree's final location.
       let bare = plan.top.join(format!("{}.git", plan.name));
       let worktree = plan.top.join("worktrees").join(default);
       git::repair_worktree(&bare, &worktree)?;
   }
   Ok(default)
   ```

   The repair runs only after the swap has fully succeeded, so it never
   interferes with the rollback path. (If `repair` itself somehow failed, the
   layout is already swapped into place — a repair error is surfaced to the user,
   who can re-run `git worktree repair` manually; this matches how other
   post-swap steps would behave.)

## Tests

1. **`src/convert.rs` — strengthen `converts_a_working_clone`** (or add a focused
   test): after `run`, assert that git operations work *inside* the default
   worktree and that it is no longer prunable. Concretely:

   - `git -C worktrees/main rev-parse --is-inside-work-tree` returns `true`
     (today this fails with "not a git repository"), and
   - `git worktree list --porcelain` from the bare repo reports the worktree at
     its final path with no `prunable` line.

   This is the regression guard — it fails before the fix and passes after.

2. **`src/git.rs`** — a small unit test for `repair_worktree` is optional; the
   convert-level test exercises it end-to-end through the real swap, which is the
   scenario that actually matters. I'll add a git-level test only if it adds
   coverage the convert test doesn't already give.

## Out of scope

- No change to `ghwf clone` (it builds in place and is unaffected).
- No change to the swap/rollback logic itself — the layout-building and atomic
  swap are correct; only the post-swap worktree relink is missing.
