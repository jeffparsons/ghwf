# Plan — finish the ghwf workflow (#1)

ghwf already drives **pre-plan** and **prep-and-plan** end to end. This plan covers
what's left to complete the "Basic workflow": getting Claude into the right
worktree, documenting the entry point, and the **implement** phase. Written by
dog-fooding ghwf's own prep-and-plan on this issue.

## 1. Worktree switching — hard error + relaunch (buildable now)

ghwf cannot change Claude Code's session working directory (it's fixed at `claude`
launch), and only the user can run `/add-dir` (which grants access but doesn't
switch cwd). So when a command needs the issue's worktree and Claude isn't running
**inside** it, ghwf should **hard-error** rather than limp along via absolute paths.

- **Detect:** compare `std::env::current_dir()` (canonicalized) against the
  persisted `PrepState.worktree_path`; error if cwd is not inside it.
- **Message:** name the worktree and give a copy-pasteable command to run *outside
  Claude*, anchored at the `ghwf.toml` directory, e.g.:
  > This issue's work happens in `worktrees/issue_1_basic_workflow`.
  > Exit Claude and run from the project root (where `ghwf.toml` lives):
  > `cd worktrees/issue_1_basic_workflow && claude`
- **When:** only once a worktree exists — prep-and-plan after creation, and the
  implement phase. Pre-plan and `--no-branch` are unaffected.
- **Helper:** add `ghwf worktree-path <issue>` that prints the absolute worktree
  path (for scripts and the `/work-on` slash command).
- **Future:** the raw `cd … && claude` is a placeholder — the relaunch should
  eventually be a ghwf call itself (e.g. `ghwf resume <issue>` that cds to the
  worktree and launches Claude). Tracked in
  https://github.com/jeffparsons/ghwf/issues/2; emit the raw command for now but
  shape the message so swapping it for the ghwf call later is trivial.

## 2. README — `/work-on` slash command (buildable now)

Add a concise README section documenting a custom `/work-on` slash command that
wraps ghwf, roughly:

```
---
description: Drive ghwf on a GitHub issue.
---
Run `ghwf work-on $ARGUMENTS` and follow the phase banner exactly:
- Never enter Claude Code plan mode; write any plan as a file where ghwf tells you.
- In pre-plan, post questions with `ghwf create-issue-comment $ARGUMENTS`.
- If ghwf hard-errors that the work belongs in a different worktree, relay its
  relaunch command to the user and stop.
```

Briefly explain the relaunch constraint (sessions are directory-bound; switching
worktrees means exiting and relaunching `claude` in the target).

## 3. Implement phase (proposed design — revisit if needed)

On `/proceed` from prep-and-plan, the issue enters `implement`. The worktree and
draft PR already exist; Claude does the coding. ghwf's role stays light:

- **Worktree guard** (section 1) applies.
- **Surface PR feedback:** `work-on` in implement shows new activity on the **PR**
  thread (via the persisted `pr_number`), reusing the per-session seen-cache so
  only new/changed PR comments appear — the implement-phase analog of pre-plan's
  issue digest. (Inline review comments via `pulls/{n}/comments` are a stretch
  goal; start with the PR conversation thread, which shares the issues comments
  endpoint.)
- **Banner:** "Implement per `plans/<n>-<slug>.md`. Commit and push as you go.
  Address the PR feedback above. When the work is complete and ready for human
  review, comment `/proceed`."

**Add a terminal `Review` phase** (`PrePlan → PrepAndPlan → Implement → Review`):

- On `/proceed` from Implement, ghwf flips the draft PR to ready-for-review
  (`gh pr ready <number>`); the banner reports it's awaiting human review.
- Resolves the open questions: phase-after-implement = `Review`; draft→ready
  timing = entering `Review`; completion signal = `/proceed`.
- `Phase::next()` extends: `Implement → Some(Review)`, `Review → None`.
- Naming: `Review` reads a little oddly in the imperative (the *human* reviews) —
  fine to rename (e.g. `request-review`).

Suggested build order: section 1 (worktree guard + `worktree-path`) → section 2
(README) → section 3 (PR-feedback digest, then `Review` phase + draft→ready).

## Already done (out of scope here)

pre-plan; prep-and-plan (worktree off `origin/<default>`, plan file, commit, push,
draft PR); `--no-branch`; `ghwf.toml` as the repo source of truth; per-session
seen-cache; `/proceed` phase advancement.

## Handoff — context for a fresh session in this worktree

You are picking up mid-stream from an interactive session that built ghwf up to
this point. **This is the implement phase for issue #1: build the three sections
above.** Key context (a fresh session launched from this worktree is a *different
project path*, so it won't auto-recall ghwf's saved memories — hence this):

**Where you are**
- Layout: project root holds `ghwf.toml` (`main_repo = "repo.git"`,
  `worktrees_dir = "worktrees"`), a **bare** `repo.git`, and `worktrees/` with
  `main` and this `issue_1_basic_workflow` worktree (branch `issue_1_basic_workflow`
  off `origin/main`). A **draft PR** for this branch tracks the work.
- Per-issue workflow state lives in `~/Library/Application Support/ghwf/issues/
  jeffparsons/ghwf/1.json` (independent of cwd). Run `ghwf work-on 1` to orient;
  once issue #1 is `/proceed`'d to `implement`, you'll get the implement banner
  (currently a stub — building it is part of this plan).

**Codebase map** (`src/`)
- `main.rs` — clap CLI + dispatch; `work_on`, `create_issue_comment`, `advance_phase`.
- `github.rs` — `gh` wrappers; repo resolution (`config_repo`, `parse_remote_url`,
  `issue_endpoint`), `default_branch`, `find_pr`, `create_draft_pr`.
- `git.rs` — `git -C` wrappers (`fetch`, `add_worktree`, `remote_url`, commit/push,
  clean/tracked/pushed probes).
- `config.rs` — `ghwf.toml` discovery (`find`, `warn_if_absent`, `require`).
- `prep.rs` — the prep-and-plan state machine (worktree → plan → commit → push → PR).
- `state.rs` — `Phase`, `IssueState`, `PrepState`, naming (`branch_and_slug`),
  `is_proceed_directive`.
- `render.rs` — phase banners, the new/changed digest, comment marker build/parse.
- `seen.rs` — per-session seen cache; `store.rs` — data dir, salt, session token,
  `content_hash`; `models.rs` — `Issue`, `Comment`, `User`.

**Principles & conventions to hold**
- ghwf must stay **blanket-allowable**: narrow, explicit subcommands; never a
  generic `gh api` passthrough.
- `ghwf.toml` is the **source of truth** for the target repo; a URL for a different
  repo is a hard error.
- Name things in the **imperative mood** (e.g. `Implement`, not `Implementation`).
- Comments: own line before the code; full sentences terminated, fragments not.
- Commit messages: concise; first line ≤50 chars, never >72.
- Never override `HOME` in shell/test commands (it breaks rustup).
- `git`/`gh` cwd detection is unsafe under the home repo; rely on `ghwf.toml` +
  explicit `git -C`. `GIT_CEILING_DIRECTORIES` is set as a backstop.

**Workflow**
- Build/test from this worktree: `cargo build && cargo clippy && cargo test`.
- Implement sections 1→2→3; commit + push to this branch as you go (the draft PR
  updates). When done and ready for review, comment `/proceed` on issue #1.
- The earlier design discussion (requirements for all of this) is on issue #1's
  comment thread — `ghwf work-on 1` will re-surface it for a fresh session.
