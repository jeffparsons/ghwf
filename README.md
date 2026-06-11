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
loop, and the session ends on its own. A merged issue moves to a terminal
**finished** state, so (when workflow labels are configured) its phase and
attention labels come off and it carries a single `ghwf:finished` label —
making it obvious at a glance that ghwf regards the work as done. Closing the
PR without merging halts the workflow the same way (with distinct wording, so
Claude surfaces it) but is not "finished": it keeps its phase label as a record
of how far the work got, and reopening the PR resumes the workflow on the next
`work-on`.

A command that doesn't match the current phase (or the retired generic
`/proceed`) is consumed without advancing anything, and `work-on` reports what
went wrong and which command applies.

Claude never raises an interactive prompt to ask you something — those can't be
answered from a phone or asynchronously, which defeats the point. When it needs
an answer to proceed in any phase, it posts the question with
`ghwf hand-off <issue> --question`: a hand-off that flips the issue to
"needs you" (the `waiting-on-user` label) and waits, but — unlike an
end-of-phase hand-off — carries no approval prompt and stays in the current
phase, so a 👍 won't advance anything. (`ghwf create-issue-comment` remains the
non-blocking way to post a note or status that needs no reply.)

When the answer is a choice among discrete options, Claude uses
`ghwf ask <issue> --option "..." --option "..."` (the question on stdin) instead
of prose. ghwf renders each option as a GitHub checkbox tagged with a hidden id,
appends a final "Submit my answers" checkbox, and flips the issue to "needs
you". You tick whatever applies — it's multi-select — and only ticking the
submit box wakes Claude; ghwf then reads back your selections and rewrites the
submit line to `_Answers submitted at …_` so it can't fire twice. You're never
trapped by the menu: a plain prose reply wakes Claude too (signalling the
question was answered but not fully resolved), and Claude is encouraged to
include an "other / none of these" option for exactly that.

Both comment-posting commands — `ghwf create-issue-comment` and `ghwf hand-off`
— take a repeatable `--attach <path>` to attach a local file (a screenshot, a
diagram, a log). GitHub has no token-authenticated API for the inline-attachment
CDN its web UI uses, so each file is instead committed into the repo on a
dedicated `ghwf-attachments` branch (its own orphan history, so it never touches
your code branches or a PR diff) and referenced from the comment. Images on a
**public** repo embed inline; on a **private** repo — where blob links are
auth-gated and GitHub's image proxy can't fetch them — every attachment, images
included, renders as a clickable link instead.

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

PRs and issues assigned to someone else are never picked. An issue with a
**live session** — a launcher process is running it right now — is skipped and
listed, so two workers never collide on one issue. But an issue that was started
and then *stopped* (the session exited, or its launcher crashed, before the
workflow concluded) is **resumed** rather than skipped: ghwf picks it up again
and re-enters its worktree. Concluded issues are left alone. (You can still
resume any stopped issue by hand with `ghwf work-on <n>`.)

Liveness is tracked with a short-lived *lease* a launcher holds while it runs a
session and refreshes on a heartbeat, kept in a sibling of the issue's state
file. A crashed launcher's lease goes stale (its process is gone), so its issue
becomes resumable automatically instead of staying locked — which matters most
for worker pools, where a launch that errors for some transient reason must not
take an issue out of circulation.

`next` also respects GitHub's issue relationships, again listing what it passes
over:

- **Currently-blocked issues** — those with at least one *open* "blocked by"
  dependency — are skipped until the blocker closes. (A blocker that's already
  closed no longer counts.) When the last open blocker closes, the issue
  becomes pickable automatically — `next --wait` even wakes on the change.
- **Tracking issues** — those with sub-issues — are never worked directly. Their
  sub-issues are ordinary open issues, so they're picked on their own merits;
  running `ghwf work-on <tracking-issue>` redirects to one of its workable
  sub-issues (the same ordering above, skipping blocked ones, descending through
  nested tracking issues, and resuming one already in progress when there is
  one).

A fresh pick is **claimed** before work starts: ghwf records the issue's state
file atomically, reserving it against any other `next` run on the machine, and
assigns the issue to you on GitHub so the pickup is visible (e.g. from your
phone). A resumed pick is single-flighted instead by the session lease — the
launcher that wins the lease runs it, and any other worker that selected the
same stopped issue backs off. Either way two runs can never work one issue at
once.

## A pool of single-use workers

`ghwf next --wait` blocks until an eligible issue appears instead of erroring
when there is none. Open several terminals, run `ghwf next --wait` in each, and
you have a pool of single-use workers: as you file (or open) issues — even from
your phone, via GitHub — each idle worker claims the next one to come along,
creates its worktree, and launches a Claude session that starts working
immediately. The atomic claim guarantees exactly one worker per issue; the
others keep waiting.

Each worker polls the open-issues listing cheaply (conditional requests with
backoff, capping at one request per minute while idle), so a handful of workers
sit comfortably inside GitHub's rate limit. Pass `--timeout <secs>` to give up
after a while (exit code 2, like `wait`); omit it to wait indefinitely.
`--no-branch` passes through as with plain `next`.

A plain `ghwf next --wait` worker is single-use: it works one issue and then
that terminal stays in the Claude session until you quit it. `ghwf forever`
makes the worker self-renewing instead. ghwf spawns Claude as a child
and supervises it: when the issue's workflow concludes (its PR is merged or
closed, or the issue is closed), ghwf brings the session down and claims the next
eligible issue, waiting when the queue is empty — so one terminal works the queue
indefinitely. To stop a `forever` worker, quit a session before its workflow
concludes (the usual Ctrl-C-twice or `/exit`); ghwf reads that as you stepping in
and stops the loop rather than picking the next issue. (`ghwf next --forever`
remains as a hidden alias for `ghwf forever` during a transitional period.)

A `forever` worker also rides out a launch that fails for a transient reason
(say a network blip while fetching the issue or creating the worktree): it logs
the failure, leaves the issue pickable rather than locking it, and moves on to
the next pick instead of stopping. A bare reservation with nothing behind it is
released; an issue that already has a worktree is simply resumed on a later
round.

This supervisor model is also why an ordinary `ghwf work-on`/`ghwf next` launch
now keeps a thin ghwf process alive alongside Claude rather than replacing
itself with it. The difference is invisible in normal use: a stray Ctrl-C still
reaches Claude (which handles its own exit gesture), and quitting Claude returns
you to the shell exactly as before — ghwf just exits with the session's status
code once the child does.

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

## Proxied GitHub commands

The Claude integration only pre-approves `Bash(ghwf:*)`, so a session never gets
to run `gh` directly. ghwf proxies the GitHub operations Claude needs during the
implement and review phases, so it can act on the PR without a permission prompt:

- `ghwf show-pr [issue]` — print the PR's current title, state, and body, the
  read path for revising it.
- `ghwf update-pr [issue] [--title <title>]` — set the PR body (read from stdin)
  and/or its title. To change only the title, pass an empty body, e.g.
  `ghwf update-pr 49 --title "…" </dev/null`.
- `ghwf pr-checks [issue] [--log-failed]` — summarise the PR's CI checks
  (wrapping `gh pr checks`); with `--log-failed`, also dump the failing-job logs
  for the PR's head commit.
- `ghwf reply-review-comment [issue] --id <comment-id>` — reply (body from stdin)
  to an inline review comment thread; the comment ids are the ones `work-on`
  surfaces.
- `ghwf create-issue --title "<title>" [issue] [--label <name>]… [--no-block]` —
  file a follow-up issue (body from stdin) for a deferral or discovery. By
  default it's marked blocked by the originating issue (the optional `[issue]`,
  else inferred like the other commands): the `blocked_label` is set atomically
  in the create payload so a worker can't grab the follow-up before it's marked,
  the native GitHub `blocked_by` dependency is set right after, and then the
  temporary label is removed again — the dependency is the durable, UI-visible
  truth (the label sticks around only if that dependency call fails).
  `--no-block` files a standalone issue; `--label` attaches extra labels. The
  new issue is created unassigned and prints as JSON.

The PR commands each resolve the issue argument the same way the other commands
do, and error clearly when the issue has no PR yet.

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
    └── main/          # checkout of the default branch, created by clone
```

The bare repo keeps the container free of a working copy that would shadow
the per-issue worktrees, while its remote is set up to behave exactly like a
normal clone's (`origin/<default>` resolves and stays fresh on fetch). The
default branch is checked out into `worktrees/<default>` so you have a ready
place to inspect it; ghwf keeps that checkout fast-forwarded as it fetches.

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
interactively for other layouts. To discover the options without leaving the
terminal, `ghwf config ls` lists them (drill into a table with, e.g., `ghwf
config ls labels`), `ghwf config info <key>` prints one option's full docs, and
`ghwf config example` writes a fully-filled, annotated `ghwf.toml` to stdout.
The annotated example below shows what it manages:

```toml
# Path to the main git repo (omit or "." if the config sits at the repo root).
main_repo = "repo.git"
# Directory under which ghwf creates per-issue worktrees.
worktrees_dir = "worktrees"
# Repos whose issues may be worked on even though the code, worktree, and PR
# live in this repo (optional; the configured repo is always allowed). Useful
# when issues are tracked in a separate repo from the code. Each entry is either
# a plain "owner/repo" string, or a table with an optional `branch_prefix`. A
# foreign-repo issue's branch is prefixed (to avoid colliding with a
# same-numbered issue in the main repo): the prefix defaults to the repo name,
# `branch_prefix = "docs"` overrides it, and `branch_prefix = ""` disables it.
# Note: GitHub's `Closes #N` auto-close doesn't work across repos, so a foreign
# issue won't auto-close when its PR merges (the PR still links it by URL); and
# `ghwf next` only discovers issues in the configured repo.
issue_repos = ["StileEducation/documentation", { repo = "StileEducation/wiki", branch_prefix = "wiki" }]
# Labels marking an issue as urgent, most urgent first (optional; used by
# `ghwf next`).
priority_labels = ["urgent", "soon"]
# When true, `ghwf next` only considers issues already assigned to you, ignoring
# unassigned ones (optional; default false). Suits teams that allocate work by
# discussion or a manager rather than picking off the list.
only_assigned_to_me = true
# Label `ghwf create-issue` applies to a follow-up to mark it blocked by the
# issue it was filed from (optional; defaults to `blocked`). A transient
# creation-race guard: set in the create payload so the follow-up carries it
# from the moment it exists, then removed again once the native GitHub
# `blocked_by` dependency is set right after (kept only if that call fails).
blocked_label = "blocked"
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
# When true, ghwf rewrites the plan commit out of the branch's history once you
# approve the implementation (mark the draft PR ready for review), then
# force-pushes the branch — for repos that don't want Claude's plans committed
# (optional; default false). It rebases out the single commit that added
# `plans/<n>-<slug>.md`. Skipped with a warning when it can't be done safely
# (dirty worktree, merge commits on the branch, the plan modified by a later
# commit, a rejected push), and a no-op in --no-branch mode.
delete_plan_on_approval = true
```

The `config ls`/`info`/`example` commands are generated from the `Config` type
itself (via [facet](https://facet.rs/) reflection), reading the same doc
comments that document each field in the source — so the listing stays complete
and never drifts from the code as options are added.

## Installing the Claude Code integration

`ghwf install` writes ghwf's user-global Claude Code pieces, so a single
`/work-on <issue>` in any session drives the workflow:

- **The `/work-on` skill**, at `<claude_dir>/skills/work-on/SKILL.md` (where
  `<claude_dir>` is `$CLAUDE_CONFIG_DIR` or `~/.claude`). It tells Claude to
  run `ghwf work-on`, follow the phase banner, keep the `wait`/`work-on` loop
  going until the workflow completes or you tell it to stop, and never raise an
  interactive prompt — questions go to the thread via `ghwf hand-off --question`
  instead.
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

The launched session starts itself: ghwf passes `/work-on` as the initial
prompt, so the workflow advances without anyone typing — which is what makes
the flow drivable from a phone. This stays interactive and subscription-billed;
only `-p`/`--print` (headless) bills separately, and ghwf never uses it.

The launcher exports `GHWF_ISSUE` (the issue's URL) to the `claude` it starts,
and records `--no-branch` in the issue's state, so ghwf commands inside the
session need neither the issue argument nor the flag repeated.

Every launch also fetches origin (worktree creation always did; an existing
worktree now triggers its own fetch) and then opportunistically fast-forwards
the worktree that has the repo's default branch checked out, so the local
`main` checkout implicitly stays fresh. The update only happens when that
worktree has no changes to tracked files, and any failure is just a warning —
it never blocks the launch.

### Choosing the model per issue

An issue can pick the Claude model its session runs on with a single line in the
issue body, on its own:

```
Model: opus
```

The key is matched case-insensitively and the value is passed straight through
to `claude --model`, so both aliases (`fable`, `opus`, `sonnet`) and full model
names (`claude-fable-5`) work. Omit the line to use Claude's default. The flag
is session-scoped — it never changes your default for other sessions.

The model is read from the body when the launcher starts the session, so editing
the line takes effect on the next launch, not mid-session. If the body has more
than one `Model:` line, or one with no value, ghwf can't tell which you meant: it
refuses to start, comments the problem on the issue, and flips it to "needs you"
so you can fix the body and relaunch. An invalid model name only Claude can
reject surfaces when the session starts.

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
