# Plan — #46: Configuration option to delete plans after implementation is approved

## Goal

Add an opt-in config flag, `delete_plan_on_approval`, that makes ghwf erase the
plan commit from a branch's history the moment the implementation is approved
(the draft PR is marked ready for review). For repositories whose owners don't
want Claude's plans committed, this leaves no trace of `plans/<n>-<slug>.md` in
the branch that gets reviewed and merged.

The flag is **off by default**, so existing repos are unaffected.

## Background — how things work today

- In branch mode, `prep::run` (`src/prep.rs`) writes the plan to
  `plans/<n>-<slug>.md` and commits it with `git::commit_file`, which stages and
  commits **only that one path** (`git commit -m <msg> -- <relpath>`). So the
  plan lives in a single, standalone commit — the first commit on the branch,
  message `Add plan for #N: <title>`.
- "Implementation is approved" corresponds to the **implement → review**
  transition. There is no approval *command* for the implement phase: the user
  marking the draft PR ready for review is what advances it. This is detected by
  `advance_on_pr_ready` (`src/main.rs`), which pushes a `Trigger::PrReady`
  transition and sets `issue_state.phase = Review`.
- Config is parsed in `src/config.rs` (`Config` struct, `serde`), offered in the
  `ghwf config init` wizard (`src/init.rs`), and documented in the README's
  annotated `ghwf.toml` (per `CLAUDE.md`'s "Adding a config option" note).
- Git is driven through thin helpers in `src/git.rs` (each shells out to `git -C
  <dir> …`). There is no rebase or force-push helper yet.

## Design decisions (settled with the issue owner)

1. **Name / default:** `delete_plan_on_approval`, boolean, default `false`.
2. **Mechanism:** rebase the plan commit out — find the commit that *added* the
   plan file and drop it with `git rebase --onto <plan_commit>^ <plan_commit>`,
   then force-push the branch with `--force-with-lease`.
3. **Merge safety:** be extra careful if the branch already has merge commits.
   If the replayed range contains any merge commit, **do not rebase** — skip and
   warn (a plain rebase would flatten/drop merges).
4. **Failure behaviour:** skip with a warning (never hard-fail the workflow) for
   every precondition that doesn't hold. The plan is simply left in place.
5. **`--no-branch`:** no-op (ghwf manages no commits there).

## Implementation

### 1. Config key (`src/config.rs`)

Add to `Config`:

```rust
/// When true, ghwf rewrites the plan commit out of the branch's history once
/// the implementation is approved (the draft PR is marked ready for review),
/// then force-pushes the branch. For repos that don't want Claude's plans
/// committed. Default false. A no-op in `--no-branch` mode, and skipped (with a
/// warning) when the rewrite can't be done safely.
#[serde(default)]
pub delete_plan_on_approval: bool,
```

Tests: a `delete_plan_on_approval_parses` test (key `= true` parses to `true`)
and confirmation it defaults to `false` when absent (extend the existing
"pre-existing configs keep loading" assertions).

### 2. Git helpers (`src/git.rs`)

Add small, individually-tested helpers (names indicative):

- `plan_commit_that_added(dir, relpath) -> Result<Option<String>>`
  — `git log --diff-filter=A --format=%H -- <relpath>`; return the **last**
  (oldest) line, the commit that introduced the path. `None` when the path was
  never added on this history.
- `range_has_merges(dir, range) -> Result<bool>`
  — `git rev-list --merges <range>` is non-empty. Used with the range
  `<plan_commit>^..HEAD` (everything from the plan's base to the tip).
- `path_touched_in_range(dir, range, relpath) -> Result<bool>`
  — `git rev-list <range> -- <relpath>` is non-empty. Used with
  `<plan_commit>..HEAD` to detect a later commit that also modified the plan
  file (which would make the drop unsafe / conflict).
- `rebase_onto(dir, onto, upstream) -> Result<()>`
  — `git rebase --onto <onto> <upstream>`. On error, the caller aborts.
- `rebase_abort(dir) -> Result<()>` — `git rebase --abort` (best-effort cleanup
  after a failed rebase).
- `force_push_with_lease(dir, branch) -> Result<()>`
  — `git push --force-with-lease origin <branch>`.

`rev_parse_ok` already exists for resolving `<plan_commit>^`.

Tests (real-git scratch repos, mirroring the existing `git::tests` style):
- linear branch: `plan_commit_that_added` finds the add; `rebase_onto
  <plan>^ <plan>` drops it and the plan file is gone from `HEAD` while later
  commits survive.
- `range_has_merges` true when a merge commit is present, false otherwise.
- `path_touched_in_range` true when a later commit edits the plan path.

### 3. Removal orchestration (new `src/plan_cleanup.rs`, wired from `src/main.rs`)

Add a function that performs the whole guarded operation and reports what it
did, so it's unit-testable in isolation:

```rust
/// Outcome of an attempted plan-commit removal, for reporting/tests.
pub enum Removal {
    Removed,
    Skipped(String), // human-readable reason
}

pub fn remove_plan_commit(worktree: &Path, branch: &str, plan_rel: &str) -> Result<Removal>
```

Steps (each failed precondition returns `Removal::Skipped(reason)`, never an
`Err` unless something truly unexpected happens):

1. Tree must be clean — `git::is_tree_clean(worktree)`. If dirty, skip.
2. `plan_commit = git::plan_commit_that_added(worktree, plan_rel)`; if `None`,
   skip (plan never committed, or already gone).
3. Resolve `plan_commit^` via `git::rev_parse_ok`; if `None` (plan commit is a
   root commit with no parent), skip — `--onto` needs a base.
4. `range_has_merges(worktree, "<plan_commit>^..HEAD")` → if true, skip
   ("branch contains merge commits").
5. `path_touched_in_range(worktree, "<plan_commit>..HEAD", plan_rel)` → if true,
   skip ("plan file modified by a later commit").
6. `rebase_onto(worktree, "<plan_commit>^", plan_commit)`. On `Err`, call
   `rebase_abort` (best-effort) and skip ("rebase failed: …").
7. `force_push_with_lease(worktree, branch)`. On `Err`, skip with a warning that
   the local branch was rewritten but the push was rejected (the remote/PR still
   carries the plan; nothing else is touched).

Wire-up in `work_on` (`src/main.rs`): the config is already read once for
`pr_instructions` — fold that into a single `config::find()?` and pull
`delete_plan_on_approval` from it too. Immediately after the existing
`advance_on_pr_ready(...)` call, when **all** of these hold:

- `delete_plan_on_approval` is set,
- `outcome.transitions` contains a `Trigger::PrReady` (the flip happened *this*
  run — so removal is attempted exactly once, since the phase is already Review
  on later runs and the transition won't re-fire),
- prep is branch mode with a recorded `worktree_path` and `branch`,

call `plan_cleanup::remove_plan_commit(...)`. Compute `plan_rel` the same way
the phase bodies do: `let (_, slug) = state::branch_and_slug(number,
&issue_data.title); let plan_rel = format!("plans/{number}-{slug}.md");`.

On `Removal::Skipped(reason)`, `eprintln!` a `warning:` line (consistent with
ghwf's other best-effort git steps); on `Removed`, a brief `println!`/note. Keep
this **purely a stderr/stdout side effect** — do not change phase, attention, or
the posted status comment. (Force-pushing the branch does not disturb the
recorded `pr_number`; the review banner and digest run as usual afterward.)

### 4. `config init` wizard (`src/init.rs`)

Following the `permission_mode` pattern:
- a `Confirm` prompt (default `false`), e.g. "Delete the plan commit from
  history once implementation is approved? (force-pushes the branch)";
- a `set_delete_plan_on_approval(doc, value)` writer using `insert_with_comment`
  with a one/two-line explanatory comment, only offered when the key is absent;
- only write/`doc_changed` when the user opts in (true). A round-trip test like
  `permission_mode_round_trips`.

### 5. README (`README.md`)

Add an annotated entry to the `ghwf.toml` example block, in the
"## Configuration" section, e.g.:

```toml
# When true, ghwf rewrites the plan commit out of the branch's history once you
# approve the implementation (mark the draft PR ready for review), then
# force-pushes the branch — for repos that don't want Claude's plans committed
# (optional; default false). It rebases out the single commit that added
# plans/<n>-<slug>.md. Skipped with a warning when it can't be done safely
# (dirty worktree, merge commits on the branch, the plan modified by a later
# commit, …), and a no-op in --no-branch mode.
delete_plan_on_approval = true
```

## Edge cases / notes

- **One-shot.** Removal is attempted only on the run where the draft→ready flip
  is first observed. If it's skipped (e.g. a dirty worktree at that instant), it
  is not retried later — acceptable for a best-effort cleanup, and called out in
  the warning.
- **Merge commits** on the branch (e.g. `origin/main` merged in) cause a skip,
  per the owner's caution — a linear replay is the only path we attempt.
- **Force-push rejection** leaves the local branch rewritten but the remote
  unchanged; warned about, nothing else altered.
- **`--no-branch`** and **concluded PRs** never reach the removal path (no
  branch-mode prep / the transition can't fire).

## Testing summary

- `src/git.rs`: unit tests for each new helper against scratch repos.
- `src/plan_cleanup.rs`: tests for `remove_plan_commit` covering removed /
  dirty-tree / no-plan-commit / merge-present / plan-modified-later, using
  scratch repos with a fake `origin` so the force-push path exercises.
- `src/config.rs`: parse + default tests for the new key.
- `src/init.rs`: round-trip test for the new setter.
- `cargo test` and `cargo clippy` clean; comments follow the repo's terminator
  conventions.
