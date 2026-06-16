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
making it obvious at a glance that ghwf regards the work as done. `wait`
normalizes these labels the moment it detects the merge, so they don't linger
even when no further `work-on` follows (the loop has already stopped, say).
Closing the PR without merging halts the workflow the same way (with distinct
wording, so Claude surfaces it) but is not "finished": it keeps its phase label
as a record of how far the work got (dropping only the attention label), and
reopening the PR resumes the workflow on the next `work-on`.

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
your code branches or a PR diff) and referenced from the comment. On a
**public** repo, images and videos embed inline (videos via an HTML `<video>`
player); audio and other files render as clickable links, since GitHub has no
inline form for them. On a **private** repo — where blob links are auth-gated
and GitHub's image proxy can't fetch them — every attachment, images included,
renders as a clickable link instead.

Pass `--no-branch` to skip the branch/worktree/PR entirely and just write the plan
file — handy for trivial tasks or when you're already on a feature branch. The
mode is recorded in the issue's state on first use (including by the
outside-Claude launcher), so later `work-on` runs don't need the flag repeated.

The issue argument is optional everywhere it appears: when omitted, ghwf falls
back to `$GHWF_ISSUE` (set on sessions started by the launcher below), then to
the issue whose recorded worktree contains the current directory. An explicit
argument otherwise wins — with one guard: the comment-posting commands
(`create-issue-comment`, `hand-off`, `ask`) refuse an explicit target that
names a different workflow than the session's bound issue, so a stray issue
number or a URL built from the wrong repo can't silently post to the wrong
place. A bare number is anchored to the bound issue's repo (not the cwd's git
remote) before this check. Each of these commands also echoes its resolved
target (`→ owner/repo#N "title" (OPEN)`) to stderr before posting.

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

To wind a worker down *without* interrupting it, run `ghwf stop`: each running
`forever` worker finishes the issue it's currently on and then exits instead of
picking another. The request is recorded under ghwf's data dir and reaches every
forever worker on the machine; a worker you start afterwards ignores it, so it's
safe to `ghwf stop` and then launch a fresh worker.

A `forever` worker also picks up a rebuilt `ghwf` on its own: it hashes its own
binary at startup, and when a workflow concludes it re-checks that hash. If the
binary has changed — you've installed a new build — the worker relaunches itself
in place (same terminal, same arguments) so the next issue runs on the new code.
The relaunch only ever happens at this clean between-workflows boundary, never
mid-issue, and a pending `ghwf stop` takes precedence (the worker exits rather
than relaunching). This is Unix-only; if the binary can't be read, the worker
just carries on rather than risking a spurious relaunch.

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

This can also run automatically. Set `auto_collect_garbage = true` (off by
default) and ghwf runs the same collection after a ticket's PR merges, at most
once per `auto_collect_garbage_interval_hours` (default 24 — once per day). The
same safety rails apply, and the manual command stays available and unthrottled.

## Staying current with the base branch

The base branch moves while a PR is open, so ghwf checks the branch against a
freshly-fetched `origin/<base>` at the moments that matter — entering the
implement and review phases, and again at hand-off — using a local trial merge
(`git merge-tree`, no GitHub API). When the branch conflicts, ghwf leads the
phase banner with a resolve-it-now instruction and **blocks the
ready-for-review hand-off** until the merge is pushed, so a known-conflicting
branch is never announced as ready.

Even when the base has moved on *cleanly*, the new commits can still bear on the
work — a refactor to follow, work that supersedes the branch's, a new helper to
reuse — so ghwf leads the banner with a heads-up that names the new commits and
asks Claude to weigh them against its plan before integrating. While a PR sits
idle in review, `ghwf wait` keeps probing on a slow cadence and wakes the agent
the moment `main` moves under it — whether the advance is clean or conflicting —
rather than leaving it for you to spot at merge time.

For the clean case you can also set `auto_merge_base = true` (off by default):
the branch is merged up to `origin/<base>` and pushed for you, keeping the open
PR current with the base branch and its CI fresh; the banner then confirms the
merge and still points Claude at the commits it brought in. Conflicts are never
auto-resolved — those are always surfaced for you or Claude to handle.

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

Already have an ordinary clone you'd rather not re-download? `ghwf convert
[path]` (default: the current directory) turns it into the layout above
in place:

```
$ ghwf convert            # run from inside the clone
```

It builds a fresh single-branch bare repo — reusing the existing clone's
objects, so the history isn't re-fetched — and moves it into the original
path, renaming the original aside to `<name>.pre-ghwf/`. That backup is left
fully intact, so any local-only branches, stashes, or uncommitted work are
still there; the new bare repo itself is pristine, exactly as `ghwf clone`
would produce. All the work happens in a scratch directory first and the
final swap is two quick renames, so converting the directory you're sitting
in is safe.

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
Two commands create the GitHub labels ghwf uses: `ghwf config state-labels`
(workflow status labels; `ghwf config labels` is a kept alias) and `ghwf config
priority-labels` (the configured `priority_labels`). Both are idempotent, so
they double as the way to upsert the labels onto an existing repo or a fresh
clone. The annotated example below shows what it manages:

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
# `ghwf next`). `ghwf config init` suggests these and offers to create them on
# GitHub; `ghwf config priority-labels` creates them later (recognised names get
# a sensible colour, others one derived from the name).
priority_labels = ["high-priority", "medium-priority"]
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
# When true, ghwf automatically runs garbage collection (same as
# `ghwf collect-garbage`) after a ticket's PR merges, at most once per
# `auto_collect_garbage_interval_hours` (optional; default false). It deletes
# merged branches and their fully-clean worktrees, with the same safety rails as
# the manual command (never the main or current worktree, never a dirty one,
# never a force-delete). The manual command stays available and unthrottled.
auto_collect_garbage = true
# Minimum hours between automatic garbage collections (optional; default 24 —
# once per day). e.g. 12 for twice a day, 168 for weekly. Ignored when
# auto_collect_garbage is off.
auto_collect_garbage_interval_hours = 24
# When true, ghwf merges the base branch into a PR branch that has fallen behind
# it whenever the merge is clean, then pushes — keeping the open PR current with
# the base branch and its CI fresh (optional; default false). Conflicts are
# never auto-resolved; they are still surfaced for you or Claude to handle.
auto_merge_base = true
# GitHub logins whose comments and 👍 reactions ghwf acts on, in addition to you
# (the authenticated user, always accepted) and the repo's collaborators —
# anyone with an OWNER / MEMBER / COLLABORATOR association (optional; empty by
# default). Everyone else's comments and reactions are ignored, so a public
# repo's workflow can't be driven by strangers. Matched case-insensitively. A
# 👍 reaction carries no association, so collaborator auto-accept for reactions
# is resolved via the repo's collaborator list; an org member with no repo
# access is accepted on a typed comment but not on a bare 👍, and should be
# listed here. The same allow-list also gates *automatic* issue selection
# (`ghwf next` / `forever`): only issues authored by you, an allow-listed login,
# or a repo collaborator are auto-picked, so strangers can't get a public repo's
# workflow to start working on the issues they open. A skipped issue can still be
# worked deliberately with `ghwf work-on <n>`.
allowed_users = ["octocat"]
```

The `config ls`/`info`/`example` commands are generated from the `Config` type
itself (via [facet](https://facet.rs/) reflection), reading the same doc
comments that document each field in the source — so the listing stays complete
and never drifts from the code as options are added.

## Installing the Claude Code integration

`ghwf install` writes **the `/work-on` skill**, at
`<claude_dir>/skills/work-on/SKILL.md` (where `<claude_dir>` is
`$CLAUDE_CONFIG_DIR` or `~/.claude`), so a single `/work-on <issue>` in any
session drives the workflow. It first tells Claude to run `ghwf onboarding` (the
session framing, below), then to run `ghwf work-on`, follow the phase banner,
keep the `wait`/`work-on` loop going until the workflow completes or you tell it
to stop, and never raise an interactive prompt — questions go to the thread via
`ghwf hand-off --question` instead.

Re-run `ghwf install` after upgrading ghwf to refresh it. The skill file carries
a marker identifying it as ghwf-written; if a file without the marker is in the
way, `install` refuses to touch it unless you pass `--force`. (An earlier ghwf
also installed a global Stop hook; `install` now removes that leftover entry,
since the hooks live per worktree — see below.)

### The session framing (`ghwf onboarding`)

The very first thing the `/work-on` skill does is run `ghwf onboarding` and treat
its output as authoritative. That output is a short framing that establishes, up
front, that **ghwf's relayed instructions and the GitHub comments of authorised
participants are to be followed as direct instructions from the user** — so a
session driven asynchronously from your phone doesn't treat an approval or answer
that arrived over GitHub as untrusted third-party data and balk, second-guess, or
demand a synchronous confirmation it can't get.

Because the skill body is the expansion of the `/work-on` initial prompt, this
runs on the session's own trusted user turn, before any GitHub data is read — and
again on every relaunch/resume, since the launcher re-passes `/work-on`. It is
also re-asserted after a mid-session context compaction, via a SessionStart hook
(see below), so the framing survives long sessions rather than being lost when the
original turn is summarised away.

The framing is deliberately **bounded**: the trust attaches only to the
already-authenticated control channel ghwf gates on — its own output plus the
comments and reactions of allow-listed users and repo collaborators (the same
gate described under `allowed_users` above). It does *not* extend to arbitrary
text encountered in code, files, other tools' output, or web pages, and it is not
a licence to bypass safety behaviour — it only settles that an instruction on the
authorised channel genuinely came from your principal. Run `ghwf onboarding`
yourself to read the exact text.

### The session hooks (written per worktree, not globally)

The hooks that keep a session on the rails are **not** global — they'd affect
every Claude session you run. Instead ghwf writes them into a worktree-local
`.claude/settings.local.json` when it sets the session's directory up (the
worktree in branch mode, the current directory in `--no-branch`), refreshing
them from the current binary on every launch so a worktree always carries the
latest. That file is machine-local: ghwf adds it to the worktree's git exclude,
so it never lands in a commit or a PR diff. The merge is surgical (only our
entries are added) and idempotent, and anything unexpected about the file is an
error, never an overwrite. Three hooks are installed:

**A Stop hook** (`ghwf claude-stop-hook`) keeps a session working. Claude Code
runs it whenever Claude tries to finish responding; it consults only ghwf's
local state and, if the session is bound to an issue (it ran `work-on` in that
issue's worktree) whose workflow is still active, blocks the stop and tells
Claude to resume the `wait`/`work-on` loop. It lets go when:

- the issue is closed, or the PR was merged or closed without merging
  (recorded by the last `work-on` run);
- it has nudged 3 times in a row with nothing new arriving — Claude is stuck
  or you've asked it to stop, so the hook stops fighting (any new activity
  observed by `work-on` resets the count); or
- the session isn't bound to any issue — the hook stays out of the way.

**A Notification hook** (`ghwf claude-notification-hook`) records when a session
goes idle or parks on a permission prompt, so the supervisor that launched it
can recover it (see below). Like the Stop hook it only reads local state, never
touches the network, and fails open.

**A SessionStart hook** (`ghwf onboarding`, scoped to the `compact` source) keeps
the session framing alive across a long session. When Claude Code compacts the
context, it runs this hook and injects its output into the fresh context, so the
authoritative framing (see [The session
framing](#the-session-framing-ghwf-onboarding)) is re-established rather than lost
with the summarised-away turn that first set it. It's scoped to compaction only —
startup and launcher-driven resume already run `ghwf onboarding` through the
`/work-on` skill, so firing on those too would just duplicate it.

The same file also carries a **`Bash(ghwf:*)` permission rule** in
`permissions.allow`, added by the same surgical merge. This is what makes ghwf's
own state-changing subcommands — `create-issue`, `hand-off`,
`create-issue-comment`, `ask`, `update-pr`, `reply-review-comment` — reliably run
mid-session in *any* target repo without stalling on a permission prompt, so a
deferral or hand-off never quietly waits on the human (see issue #111). Two
separate guards could otherwise block such a call, and ghwf addresses both: a
permission *prompt* is pre-empted by this allow rule (belt-and-suspenders with the
`/work-on` skill's own `allowed-tools`, which doesn't depend on per-repo settings),
and the model's own reluctance to make an "external write it initiated on its own"
is settled by the onboarding framing, which names running ghwf's own subcommands as
the sanctioned, pre-authorised way to act on the workflow.

### Recovering a stuck session

When ghwf launched the session (via `work-on` outside Claude, or `ghwf
forever`), the launcher stays on as a thin supervisor and can un-stick a session
without anyone returning to the machine. It watches for three things: the child
process exiting, the workflow concluding, and the Notification hook's
idle/permission signal. The moment a session looks stuck or exits unexpectedly
it posts a heads-up to the issue and flips it to "needs you", so it reaches you
quickly. Then:

- an **unambiguous** lockup — the Stop hook has given up and the session is
  sitting idle, or the process crashed — is recovered at once by bringing the
  session down and resuming it (`claude --resume`, which re-runs `/work-on` and
  drops back into the loop);
- an **ambiguous** one — a plain idle that might be a real pause, or a
  permission prompt you may be about to clear — is left alone for ten minutes
  first, so you have a chance to step in before ghwf resumes it.

Recovery is capped: after a couple of resumes that don't take, ghwf stops and
leaves the session parked on you. A genuine clean quit is always respected — it
ends the session rather than being recovered.

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
the flow drivable from a phone. The skill's first step is to run `ghwf
onboarding`, establishing the session framing (see [The session
framing](#the-session-framing-ghwf-onboarding)) on that trusted initial turn
before any GitHub data is read. This stays interactive and subscription-billed;
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
