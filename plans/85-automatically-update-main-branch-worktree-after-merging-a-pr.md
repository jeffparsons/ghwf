# Plan: Automatically update main branch worktree after merging a PR

## Goal

When ghwf detects that a worktree's PR has just been merged, automatically
fast-forward the worktree that has the default (main) branch checked out — but
only if that worktree is clean and at-or-strictly-behind `origin/<default>`.
This keeps a long-lived main-branch worktree current without anyone having to
work on it directly. Especially useful when developing ghwf itself.

## Background / what already exists

The exact "clean + at-or-strictly-behind" update mechanism already lives in
`prep::update_default_worktree()` (`src/prep.rs:69`):

- Finds the worktree with `<default>` checked out via `git::branch_worktree()`;
  no such worktree → nothing to do.
- Already at the fetched tip (`git::remote_branch_matches()`) → nothing to do.
- Tree not clean (`git::is_tree_clean()`) → skip with a note.
- Otherwise `git::merge_ff_only(worktree, "origin/<default>")`; a worktree with
  local commits can't fast-forward, so it's skipped with a note.

This is a precise match for the issue: "clean" = `is_tree_clean`, and
"at or strictly behind" = a `--ff-only` merge succeeds. Every skip/failure is a
harmless stderr note, never an error — callers can't be broken by it.

Today this helper is only called during worktree *creation*
(`prep::ensure_worktree`, `src/prep.rs:41`), after a fetch.

## The gap

Merge detection happens in `work_on` (`src/main.rs:460-498`). The PR object is
fetched from the GitHub API, `issue_state.pr_outcome` is recomputed, and
`new_conclusion` (`src/main.rs:466`) holds the outcome *only on the run where it
first transitions*. When the outcome becomes `Merged`, the phase is stamped
`Finished` (`src/main.rs:496-498`).

Nothing updates the main worktree there. Also, this detection path is
API-driven and does **not** fetch, so local `origin/<default>` is stale at that
point and must be fetched before the helper can do anything useful.

(Note: `collect-garbage` is the *other* place merges are handled and it already
fetches — see "Out of scope" below.)

## Approach

Reuse the existing helper; add a fetch + call on the fresh-merge transition.

1. **Trigger on fresh detection only.** Use `new_conclusion == Some(PrOutcome::Merged)`
   as the gate, not `issue_state.pr_outcome == Some(Merged)`. `new_conclusion`
   is non-`None` only on the run where the outcome transitions, so we fetch +
   update exactly once per merge rather than on every later `work-on` over an
   already-finished issue.

2. **Require config.** The update only makes sense when there's a configured
   `main_repo` telling us where the worktrees live. Use `config::find()?` (the
   `located` value already computed at `src/main.rs:506`) and do nothing when
   there's no config. This keeps the no-config single-repo path untouched.

3. **Fetch, then update.** When the gate fires and config is present:
   - `git::fetch(&main_repo)` so `origin/<default>` reflects the just-merged PR.
   - Resolve the default branch with `github::default_branch(&code_owner, &code_repo)`.
   - Call `prep::update_default_worktree(&main_repo, &default)`.

   `main_repo` comes from `located.main_repo_path()`.

4. **Placement.** Add a small block after `located` is resolved
   (`src/main.rs:506`), gated on the fresh-merge condition. The existing
   phase-stamping block at `src/main.rs:496-498` stays as-is. Extract a small
   helper in `main.rs` (e.g. `fn update_main_worktree_after_merge(located, code_owner, code_repo) -> Result<()>`)
   to keep `work_on` readable, or inline it if it stays short.

## Files to change

- `src/main.rs` — add the fetch + `update_default_worktree` call on the
  fresh-merge transition, gated on having a config. (`prep::update_default_worktree`
  is already `pub`, so no signature changes needed.)

No changes to `src/prep.rs`, `src/git.rs`, or `src/config.rs` are expected — the
helper and git primitives already exist. No new config key, so the CLAUDE.md
"adding a config option" checklist doesn't apply.

## Edge cases (all handled by the existing helper)

- No main-branch worktree exists → no-op.
- Main worktree dirty → skipped with a note.
- Main worktree has local commits ahead / diverged → `--ff-only` fails, skipped
  with a note (this is the "strictly behind" guard).
- Main worktree already up to date → no-op via `remote_branch_matches`.
- No config / single-repo mode → skipped (no `main_repo` to locate worktrees).

## Out of scope

- `collect-garbage` (`src/collect_garbage.rs`) already fetches and handles
  merged-branch cleanup independently; leaving its behaviour unchanged unless
  the user wants the auto-update wired in there too.
- No new config option to enable/disable the behaviour — it's safe-by-default
  (only ever fast-forwards a clean, behind worktree).

## Testing / verification

- `cargo build` and `cargo clippy` clean.
- Manual: with a main-branch worktree checked out clean and behind, merge a
  feature PR and run `ghwf work-on <n>` on the merged issue; confirm the main
  worktree fast-forwards and prints the "Fast-forwarded …" line.
- Manual negatives: dirty main worktree → note, no update; no main worktree →
  silent no-op.
