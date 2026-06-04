# ghwf: GitHub WorkFlow

`ghwf` drives a GitHub issue through a fixed sequence of phases, one
`ghwf work-on <issue>` invocation at a time. Each run reports the current phase,
surfaces what's new since you last looked, and tells Claude what to do next. You
advance between phases by commenting `/proceed` on the issue.

## Phases

1. **pre-plan** — discuss on the issue to resolve open questions. Claude posts
   questions and a summary with `ghwf create-issue-comment <issue>`; no branch or
   worktree yet.
2. **prep-and-plan** — ghwf creates the branch + worktree (off `origin/<default>`),
   Claude writes `plans/<n>-<slug>.md`, and ghwf commits it, pushes, and opens a
   draft PR.
3. **implement** — Claude codes in the worktree, committing and pushing as it goes
   (the draft PR updates). `work-on` surfaces new activity on the PR conversation
   thread so review feedback is easy to follow.
4. **review** — on `/proceed` from implement, ghwf flips the draft PR to
   ready-for-review and the work awaits a human.

Pass `--no-branch` to skip the branch/worktree/PR entirely and just write the plan
file — handy for trivial tasks or when you're already on a feature branch.

## Configuration

ghwf needs a `ghwf.toml`, found by walking up from the current directory:

```toml
# Path to the main git repo (omit or "." if the config sits at the repo root).
main_repo = "repo.git"
# Directory under which ghwf creates per-issue worktrees.
worktrees_dir = "worktrees"
```

## The `/work-on` slash command

Wrap ghwf in a custom Claude Code slash command so a single `/work-on <issue>`
drives the workflow. Put this in `.claude/commands/work-on.md`:

```markdown
---
description: Drive ghwf on a GitHub issue.
---
Run `ghwf work-on $ARGUMENTS` and follow the phase banner exactly:
- Never enter Claude Code plan mode; write any plan as a file where ghwf tells you.
- In pre-plan, post questions and your final summary with
  `ghwf create-issue-comment $ARGUMENTS`.
- If ghwf hard-errors that the work belongs in a different worktree, relay its
  relaunch command to the user and stop — do not try to work around it.
```

## The relaunch constraint

A Claude Code session's working directory is fixed when `claude` launches; ghwf
can't move a running session into a different directory. So once an issue has its
own worktree, the work must happen in a Claude session launched *inside* that
worktree.

When you run a phase that needs the worktree from the wrong directory, ghwf
hard-errors with a copy-pasteable relaunch command rather than operating across
directories, for example:

```
cd worktrees/issue_1_basic_workflow && claude
```

`ghwf worktree-path <issue>` prints the absolute worktree path for an issue, for
use in scripts and the slash command. (A future `ghwf resume <issue>` will fold
the `cd … && claude` relaunch into ghwf itself.)
