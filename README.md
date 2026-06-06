# ghwf: GitHub WorkFlow

`ghwf` drives a GitHub issue through a fixed sequence of phases, one
`ghwf work-on <issue>` invocation at a time. Run inside a Claude Code session,
each run reports the current phase, surfaces what's new since you last looked,
and tells Claude what to do next; run outside one, it launches (or resumes) the
issue's Claude session instead. You advance between phases by commenting a
phase-specific approval command — `/approve-pre-plan` (alias `/approve-preplan`),
`/approve-plan`, or `/approve-implementation` — on the issue or, once the draft
PR exists, on its conversation thread.

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
4. **review** — on `/approve-implementation`, ghwf flips the draft PR to
   ready-for-review and the work awaits a human.

A command that doesn't match the current phase (or the retired generic
`/proceed`) is consumed without advancing anything, and `work-on` reports what
went wrong and which command applies.

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

## Running `work-on` outside Claude

Run outside a Claude Code session, `ghwf work-on <issue>` acts as a launcher
rather than printing the phase banner. It narrates each step as it:

1. finds the issue's worktree, creating it (and its branch) immediately if it
   doesn't exist yet — even in pre-plan — so the session it starts is anchored
   there and stays resumable across every phase (`--no-branch` opts out and
   launches Claude in the current directory instead); then
2. starts an interactive `claude` in the worktree, resuming the worktree's
   recorded session (`claude --resume <id>`) when its transcript still exists,
   else starting fresh.

For a fresh session there's nothing to resume and nothing queued: ghwf reminds
you to run `/work-on <issue>` once Claude is up. It deliberately passes no
prompt — programmatic use (`-p`/`--print`) is billed as API traffic rather than
your subscription.

## The relaunch constraint

A Claude Code session's working directory is fixed when `claude` launches; ghwf
can't move a running session into a different directory. So once an issue has its
own worktree, the work must happen in a Claude session launched *inside* that
worktree.

When you run a phase that needs the worktree from the wrong directory, ghwf
hard-errors rather than operating across directories, and tells you to exit
Claude and run `ghwf work-on <issue>` from outside — the launcher above, which
switches to the worktree and resumes the issue's session.

`ghwf worktree-path <issue>` prints the absolute worktree path for an issue, for
use in scripts and the slash command.
