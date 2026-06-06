# Plan — outside-Claude `work-on` as a launcher (#2)

Run *outside* a Claude session, `ghwf work-on <issue>` currently prints the phase
banner and persists nothing. This plan turns it into a **thin launcher**: ensure
the issue's worktree exists, then start (or resume) an interactive Claude session
anchored in it, narrating each step. All real phase work (plan/commit/push/PR/
banner/digest) keeps happening *inside* the session.

Design decisions were locked on the issue thread:

1. **Thin launcher** — outside-Claude `work-on` only ensures the worktree and
   launches Claude.
2. **Create the worktree ASAP** — by default the launcher creates the
   branch/worktree immediately, *including during pre-plan*, so the session is
   anchored in the worktree from the first launch and stays resumable across all
   phases. `--no-branch` opts out (launch in the current directory).
3. **Track the session id; resume by id** — every in-Claude `work-on` run whose
   cwd is inside the issue's worktree records `CLAUDE_CODE_SESSION_ID` as the
   worktree's session (last wins). The launcher resumes with
   `claude --resume <id>` when the tracked session's transcript still exists,
   else starts a fresh `claude` and reminds the user to run `/work-on <issue>`.
4. **Launch directory** — the worktree in branch mode; the current directory for
   `--no-branch`.
5. **Reworded wrong-worktree hard-error** — the in-Claude guard error points at
   running `ghwf work-on <issue>` from outside Claude, replacing the raw
   `cd … && claude`.
6. Never pass `-p`/`--print` or a prompt (interactive only — subscription
   pricing); `exec`-replace into `claude`; no separate `resume` subcommand.

## 1. Shared worktree creation (`prep.rs`)

Extract step 1 of `prep::run` (fetch, resolve default branch, `git worktree add`,
record `branch`/`worktree_path` in `PrepState`) into a shared function, e.g.:

```rust
/// Ensure the issue's branch + worktree exist, creating PrepState if needed.
/// Records branch and worktree_path; saves nothing (callers persist state).
pub fn ensure_worktree(issue: &Issue, owner: &str, repo: &str, state: &mut IssueState)
    -> Result<(PathBuf, String)>
```

- Initializes `state.prep` (with `no_branch: false`) when absent, so the launcher
  can call it during pre-plan. `prep::run` keeps its own first-entry init for the
  `--no-branch` case and then delegates to this for branch mode.
- Behavior is unchanged for the in-session path: keyed off `prep.branch.is_none()`,
  requires `config::require()`, creates off `origin/<default>`.
- Tweak the `--no-branch`-conflict warning wording in `prep::run`: prep state may
  now have been created during pre-plan by the launcher, so say "already started
  in branch mode" without claiming prep-and-plan started it.

## 2. Session tracking (`state.rs`, `main.rs`)

- Rename `PrepState.session_id` → `worktree_session_id`, semantics: "the Claude
  session most recently seen running `work-on` inside this issue's worktree".
  Nothing reads the old field, and serde tolerates the rename in existing state
  files (unknown field ignored; missing `Option` → `None`).
- Stop recording the session at PrepState creation in `prep::run`; instead, in
  `main.rs::work_on`, whenever `CLAUDE_CODE_SESSION_ID` is set and the cwd is
  inside the recorded `worktree_path` (any phase), set `worktree_session_id`
  before `state::save`. Expose the existing canonicalizing containment check from
  `worktree.rs` (e.g. `worktree::cwd_is_inside(&Path) -> bool`) for this and for
  the guard.

## 3. The launcher (`launch.rs`, dispatch in `main.rs`)

At the top of `work_on`, before any GitHub fetch: if `CLAUDE_CODE_SESSION_ID` is
unset/empty, hand off to a new `launch::run(issue_arg, no_branch)`. The flow:

1. Print why we're in launcher mode: no Claude session detected, so ghwf will
   prepare the worktree and start Claude in it.
2. Resolve the issue via `github::resolve_issue_ref` (no network for a bare
   number + config) and `state::load`.
3. **`--no-branch`** (flag, or `prep.no_branch` recorded in state): print that no
   worktree is used, and exec a fresh `claude` in the current directory with the
   `/work-on <n>` reminder. No session tracking without a worktree.
4. **Branch mode:**
   - Worktree already recorded → say so. If the directory no longer exists on
     disk, hard-error with a short explanation (recovery is manual; out of scope).
   - No worktree yet → `github::fetch_issue` (we need the title) and
     `prep::ensure_worktree`, printing that the worktree is created now — even in
     pre-plan — so the session is anchored and resumable from the start. Then
     `state::save`.
5. Decide resume: if `worktree_session_id` is set and its transcript exists at
   `<claude_dir>/projects/<munge(worktree)>/<id>.jsonl`, resume by id; otherwise
   start fresh and print the `/work-on <n>` reminder (we deliberately pass no
   prompt). `claude_dir` is `$CLAUDE_CONFIG_DIR` when set, else `~/.claude`
   (home via the `directories` crate already in deps). `munge` replaces every
   non-alphanumeric char of the canonicalized absolute path with `-` (verified
   against real `~/.claude/projects/` entries, e.g. `/Users/…/repo.git` →
   `-Users-…-repo-git`). A tracked id whose transcript is gone falls back to
   fresh, with a note.
6. Print the final action ("Launching `claude --resume <id>` in `<worktree>`" /
   "Starting a fresh Claude session in `<worktree>`"), flush stdout,
   `std::env::set_current_dir(dir)`, then exec `claude` (args: nothing, or
   `--resume <id>`). Unix: `CommandExt::exec` so quitting Claude returns to the
   user's shell; non-unix fallback: spawn, wait, exit with the child's status.

The launcher does *not* process `/proceed`, render banners, or touch the seen
cache — phase advancement happens on the in-session `work-on` run.

## 4. Reworded wrong-worktree error (`worktree.rs`)

`ensure_inside` gains the issue number; `relaunch_message` becomes: name the
worktree and cwd as today, then instruct —

> Exit Claude, then from the project root (where `ghwf.toml` lives, `<dir>`) run:
>
>     ghwf work-on <n>
>
> ghwf will switch to the worktree and resume this issue's Claude session (or
> start one there).

Drop the "future `ghwf resume <issue>`" sentence and the now-stale comment on
`relaunch_message` referencing issue #2 as future work.

## 5. README

- Rewrite "The relaunch constraint": the constraint stands (a session's cwd is
  fixed), but the remedy is now `ghwf work-on <issue>` run outside Claude —
  describe the launcher (worktree find-or-create, resume-by-id, fresh-session
  `/work-on` reminder, `--no-branch` → current dir). Remove the "future
  `ghwf resume`" parenthetical.
- Note in the phases intro that running `work-on` outside Claude launches/resumes
  the issue's session rather than printing the banner.

## 6. Tests

- `munge` unit tests against the verified real-world examples (leading `/`, `.`,
  `_` all → `-`).
- Transcript-path construction + resume decision: factor the check so the claude
  dir is a parameter, and test Some/None against a temp directory layout.
- Keep `worktree::is_inside` tests; adjust for any rename.

Build order: 1 → 2 → 3 (the bulk) → 4 → 5, with 6 alongside 3.

## Out of scope / punted

- Recovering a recorded-but-deleted worktree directory (hard error with guidance).
- Resuming sessions for `--no-branch` work (no worktree to key the session to).
- An allowlist of alternative repos, inline PR review comments, etc. (tracked
  elsewhere).
