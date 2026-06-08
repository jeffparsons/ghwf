# ghwf: GitHub WorkFlow

`ghwf` drives a GitHub issue through a fixed sequence of phases, one
`ghwf work-on <issue>` invocation at a time. Run inside a Claude Code session,
each run reports the current phase, surfaces what's new since you last looked,
and tells Claude what to do next; run outside one, it launches (or resumes) the
issue's Claude session instead. You advance between phases by commenting a
phase-specific approval command — `/approve-pre-plan` (alias `/approve-preplan`),
`/approve-plan`, or `/approve-implementation` — on the issue or, once the draft
PR exists, on its conversation thread. Reacting 👍 to a ghwf-posted comment that
prompts for an approval is equivalent to posting that command.

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

Merging the PR completes the workflow automatically: `wait` wakes on the
merge, `work-on` posts a final status update and tells Claude to stop the
loop, and the session ends on its own. Closing the PR without merging halts
the workflow the same way (with distinct wording, so Claude surfaces it);
reopening the PR resumes it on the next `work-on`.

A command that doesn't match the current phase (or the retired generic
`/proceed`) is consumed without advancing anything, and `work-on` reports what
went wrong and which command applies.

Pass `--no-branch` to skip the branch/worktree/PR entirely and just write the plan
file — handy for trivial tasks or when you're already on a feature branch. The
mode is recorded in the issue's state on first use (including by the
outside-Claude launcher), so later `work-on` runs don't need the flag repeated.

The issue argument is optional everywhere it appears: when omitted, ghwf falls
back to `$GHWF_ISSUE` (set on sessions started by the launcher below), then to
the issue whose recorded worktree contains the current directory. An explicit
argument always wins.

## Picking an issue automatically

`ghwf next` chooses the next issue to work on and then proceeds exactly as
`ghwf work-on <picked>` would (including `--no-branch` pass-through). It picks
from the repo's open issues, preferring, in order:

1. issues assigned to you (the authenticated `gh` user);
2. issues carrying a priority label, ranked by the configured
   `priority_labels` list — earlier in the list wins;
3. the lowest issue number.

PRs and issues assigned to someone else are never picked. Issues a ghwf
session has already started are skipped too (ghwf can't tell whether another
session is still working them); `next` lists the ones it passed over — resume
those explicitly with `ghwf work-on <n>`.

## Collecting garbage

`ghwf collect-garbage` (alias `gc`) cleans up after merged PRs. After a
`git fetch --prune`, it examines every local and `origin/` branch other than
the default branch, and for each one whose PR has been merged:

- **deletes the branch** — local and remote sides judged independently — but
  only when that side's tip is exactly the head commit GitHub merged, so
  nothing added or rewritten since the merge is ever lost. The check works
  for merge commits and squash/rebase merges alike, and also verifies the
  merge actually landed on the default branch;
- **removes the branch's worktree** first, but only when its working tree is
  fully clean (no changes to tracked files *and* no untracked files), and
  never the main worktree or the one the command is running inside;
- **removes the issue's ghwf state file** once nothing of the branch remains.

Anything suspicious — a merged branch carrying different/extra content than
what was merged, a worktree with working-tree changes — is warned about and
left alone. Nothing is ever force-deleted. Branches with an open PR, or with
no merged PR at all, are skipped silently. Pass `--dry-run` to see what would
be collected without touching anything.

## Waiting for approval and feedback

`ghwf wait <issue>` blocks until something new happens on the issue or its PR,
so an agent can hand off and sleep instead of being re-prompted by hand. The
contract:

- **exit 0** — new activity arrived (a comment, an edit, a state change, the
  PR getting merged or closed); run `ghwf work-on <issue>` to process it.
  ghwf's own status updates and the current session's comments never count as
  activity.
- **exit 2** — the timeout elapsed with nothing new (`--timeout <secs>`,
  default 540 — just under a 10-minute shell command timeout); run
  `ghwf wait <issue>` again.
- **exit 1** — an error.

Polling is cheap: conditional requests (`If-None-Match` with ETags recorded in
the issue's local state) against the endpoints `work-on` reads, starting at
5 s and backing off to 60 s while idle. Once quiet at the cap, `wait` switches
to watching the repo events feed — one rate-limit-free request per cycle —
after verifying the feed currently shows ghwf's own latest post (it can lag),
with a full direct sweep every ~5 min as the backstop.

## Setting up a project

`ghwf clone` clones a GitHub repo into ghwf's preferred layout in one step:

```
$ ghwf clone owner/repo        # or a full HTTPS/SSH URL
```

creates, under the current directory:

```
repo/                  # container directory (override with a second argument)
├── repo.git/          # bare repo, remote configured like a normal clone's
├── ghwf.toml          # generated, essentials only
└── worktrees/         # per-issue worktrees land here
```

The bare repo keeps the container free of a working copy that would shadow
the per-issue worktrees, while its remote is set up to behave exactly like a
normal clone's (`origin/<default>` resolves and stays fresh on fetch).

For big repos, `--reference <path>` borrows objects from an existing local
clone instead of fetching them over the network. The new repo is dissociated
from the reference afterwards, so it's safe to delete the old clone — the
typical migration move.

`ghwf clone` generates only the config essentials; run `ghwf config init`
afterwards for the optional extras (priority labels, PR instructions,
workflow status labels). Other layouts remain fully supported — the command
is just the opinionated default; any layout you can describe in a `ghwf.toml`
works.

## Configuration

ghwf needs a `ghwf.toml`, found by walking up from the current directory.
`ghwf clone` generates one; `ghwf config init` sets one up (or extends one)
interactively for other layouts. The annotated example below shows what it
manages:

```toml
# Path to the main git repo (omit or "." if the config sits at the repo root).
main_repo = "repo.git"
# Directory under which ghwf creates per-issue worktrees.
worktrees_dir = "worktrees"
# Labels marking an issue as urgent, most urgent first (optional; used by
# `ghwf next`).
priority_labels = ["urgent", "soon"]
# Markdown file of instructions for writing PR titles and bodies (optional;
# defaults to `pull-request.md` next to this config). When the file exists,
# the implement- and review-phase instructions point Claude at it and tell it
# to finish each round of work by checking whether the PR title/body need
# updating; when it doesn't, a brief generic instruction applies instead. The
# file is free-form prose and may itself refer to repo-versioned templates.
pr_instructions = "pull-request.md"
# Permission mode for the Claude sessions ghwf launches, passed through as
# `claude --permission-mode <value>` (optional; omit for Claude's default
# prompting behaviour). "auto" is recommended for unattended use — without
# it, sessions quickly stall on permission prompts.
permission_mode = "auto"
```

## Installing the Claude Code integration

`ghwf install` writes ghwf's user-global Claude Code pieces, so a single
`/work-on <issue>` in any session drives the workflow:

- **The `/work-on` skill**, at `<claude_dir>/skills/work-on/SKILL.md` (where
  `<claude_dir>` is `$CLAUDE_CONFIG_DIR` or `~/.claude`). It tells Claude to
  run `ghwf work-on`, follow the phase banner, and keep the `wait`/`work-on`
  loop going until the workflow completes or you tell it to stop.
- **A Stop hook** in `<claude_dir>/settings.json`, pointing at
  `ghwf claude-stop-hook`.

Re-run `ghwf install` after upgrading ghwf to refresh both. The skill file
carries a marker identifying it as ghwf-written; if a file without the marker
is in the way, `install` refuses to touch it unless you pass `--force`. The
settings merge is surgical (only our `hooks.Stop` entry is ever added) and
idempotent, and anything unexpected about the file is an error, never an
overwrite.

### How the Stop hook keeps a session working

Claude Code runs the hook whenever Claude tries to finish responding. The hook
consults only ghwf's local state: if the session is bound to an issue (it ran
`work-on` in that issue's worktree) whose workflow is still active, the hook
blocks the stop and tells Claude to resume the `wait`/`work-on` loop. It lets
go when:

- the issue is closed, or the PR was merged or closed without merging
  (recorded by the last `work-on` run);
- it has nudged 3 times in a row with nothing new arriving — Claude is stuck
  or you've asked it to stop, so the hook stops fighting (any new activity
  observed by `work-on` resets the count); or
- the session isn't bound to any issue (including all `--no-branch` work) —
  the hook stays out of the way of every other Claude session.

The hook never touches the network and fails open: any error means the stop
is allowed.

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

The launcher exports `GHWF_ISSUE` (the issue's URL) to the `claude` it starts,
and records `--no-branch` in the issue's state, so ghwf commands inside the
session need neither the issue argument nor the flag repeated.

Every launch also fetches origin (worktree creation always did; an existing
worktree now triggers its own fetch) and then opportunistically fast-forwards
the worktree that has the repo's default branch checked out, so the local
`main` checkout implicitly stays fresh. The update only happens when that
worktree has no changes to tracked files, and any failure is just a warning —
it never blocks the launch.

For a fresh session there's nothing to resume and nothing queued: ghwf reminds
you to run `/work-on` (no argument needed) once Claude is up. It deliberately
passes no prompt — programmatic use (`-p`/`--print`) is billed as API traffic
rather than your subscription.

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
