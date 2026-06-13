# Plan for #97 — Audit ghwf issue/PR history for bugs and surprises

This issue ships no code. Its deliverable is a **menu of candidate follow-up
tickets** drawn from places where ghwf behaved in a way the user wouldn't have
expected. The plan below *is* that menu. After it's approved I'll present the
candidates as `ghwf ask` checkboxes, then `ghwf create-issue` the ones ticked.

## Method

I saved the full event stream — **708 comment bodies across all 105 issues and
PRs** (open + closed), plus inline review comments — and read it
systematically: every one of the user's 134 corrective comments in full, plus a
full-thread deep read of every issue/PR, and a codebase check of the current
state of the top findings so I could separate "still latent" from "already
fixed." Surprises were triangulated from three signals: the user re-flagging or
correcting something, ghwf's own status posts revealing odd behaviour, and the
code confirming whether a gap is still open.

The dominant historical pain by a wide margin was **merge conflicts** — the user
manually re-flagged them ~12 times across 9 PRs (#25, #31, #32, #34, #42, #44,
#47, #63, #69, #77), twice with visible exasperation ("Oh dear, more merge
conflicts", "There seem to be more conflicts 😅"). Most one-off bugs (wrong-repo
posting #88, lost `--no-branch` #13, the digest blind spot #18/#19, the
never-removed blocked label #70, spurious wakeups #51) were caught and fixed in
their own issues. What follows are the surprises that are **still latent or only
partially addressed** — the actionable ones.

---

## Candidate follow-up tickets (the menu)

### A. Merge conflicts still reach the user late — detect at hand-off and consider auto-rebase  ★ highest value
**Evidence:** re-flagged ~12× across 9 PRs; persisted *after* conflict detection
landed in #45/#48 (#63/#69/#77 are all post-#48).
**Current state (verified):** detection runs only at the *start* of an
Implement/Review `work-on` (`src/main.rs:813-819` → `git merge-tree`), prefixes a
resolve prompt to the phase banner, and never posts to the thread. There is **no
continuous monitoring and no auto-rebase** — so `main` can advance again while
the PR sits idle in review, and the user discovers the conflict at merge time.
**Proposed scope:** check for conflicts at hand-off / ready-for-review (not just
at the next wake), optionally offer to merge `origin/main` automatically, and/or
surface a conflict signal (status note or label) when `main` moves under an idle
PR — instead of the user being the one to notice every time.

### B. No immediate acknowledgement that an approval directive registered
**Evidence:** user posted `/approve-implementation` twice back-to-back on #27;
earlier `/proceed` was spammed 3–4× on #1 and #3 — the classic "no feedback, so
repeat it" pattern.
**Current state (verified):** a directive is consumed silently
(`src/main.rs:1550`) with **no reaction, reply, or label flip at that moment**;
the only feedback is the status comment the *next* session posts when it
finishes. Between approving and that post, the user sees silence.
**Proposed scope:** acknowledge a directive/👍 the instant it's consumed — e.g.
react to the comment, or flip the attention label to `claude-working`
immediately — so the user knows it landed without re-posting.

### C. Labels/state not cleaned up when a PR is merged without a follow-up `work-on`
**Evidence:** user's #70 complaint, "labels don't get cleaned up when the full
workflow is finished."
**Current state (verified):** `labels::sync` correctly normalizes to
`ghwf:finished` — *but only when `work-on` runs after the merge*
(`src/main.rs:1199-1205`). Merge detected via the `wait` feed (`src/wait.rs`)
does **not** trigger a sync, so `ghwf:review` / `ghwf:needs-you` linger
indefinitely if no further `work-on` runs.
**Proposed scope:** normalize labels on merge detection (or in a periodic GC pass
— overlaps with open issue #99). Worth confirming whether to fold this into #99
rather than file separately.

### D. Orphaned attention/label state after a crashed or killed session
**Evidence / current state (verified):** the session *lease* is auto-reclaimed on
a dead PID / expired TTL (`src/state.rs:700-925`), but the issue's GitHub labels
and `attention` state stay frozen at whatever the crashed run last set — e.g. an
issue killed mid-prep can sit showing `waiting-on-ghwf` until a human re-enters
it. `collect_garbage` is manual-only and only touches *merged* PRs.
**Proposed scope:** when a stale lease is reclaimed (or in periodic GC), re-sync
labels / reset attention so a crashed session doesn't leave the issue looking
stuck. Also overlaps with #99.

### E. ghwf's own write subcommands can be blocked as "external writes" mid-session
**Evidence:** on #86, Claude tried to file the agreed follow-up issue and was
blocked — "the action was blocked as an external write I'd initiated on my own" —
and had to ask the user to do it. This defeats the deferral mechanism ghwf relies
on (`create-issue`).
**Current state:** likely *partially* mitigated since, by the project's CLAUDE.md
pre-authorisation of all `ghwf` commands and the #101/#105 onboarding framing —
but never confirmed to actually unblock `create-issue` in a launched session.
**Proposed scope:** verify (and if needed harden) that ghwf's own write
subcommands are reliably pre-authorised in launched sessions, so a deferral never
stalls waiting on the human. Possibly already closed — offer only if you want it
confirmed.

### F. Smoke-test the live write/shutdown paths that shipped untested
**Evidence:** several features were merged on unit-test confidence with their
live paths self-flagged as unexercised: `create-issue` + native `blocked_by` POST
(#58), cross-repo `issue_repos` end-to-end (#69), session lease / crash-recovery
(#82), and the double-SIGINT shutdown gesture (#77).
**Proposed scope:** a small tracking ticket to live-exercise these once, so the
first real use isn't where they surprise you (wrong-repo dependency, terminal
left in raw mode, lease not reclaimed). QA/tracking rather than a behaviour bug.

### G. (minor) Approval provenance — human vs. the bot's shared account
**Evidence:** many phase advances are stamped "triggered by a 👍 from
**jeffatstile**" — the same account ghwf posts from. I verified ghwf never posts
reactions itself, so these are *you* approving from the bot's shared account, not
a self-approval bug. But the approval gate doesn't author-filter the bot account,
so the audit trail can't distinguish a human approval from the bot's identity,
and any future automation reacting from that account would self-advance.
**Proposed scope:** optionally exclude the bot's own account from counting as
approver, and/or record provenance. Low priority / defensive.

---

## Considered but excluded (already addressed — listed for transparency)
- Approving an implementation before it exists (#28) → fixed by the phases work.
- `claude-working` label "lying" while the user was actually blocked (#43) →
  fixed by `hand-off --question`.
- Digest missing PR-thread plan feedback (#18) → fixed by #19/#20.
- Bare issue numbers posting to the wrong repo (#88) → fixed by #89 + #91.
- `--no-branch` lost between outer/inner ghwf (#13) → fixed by #31.
- Spurious wakeups from ghwf's own label churn (#51) → fixed by #52.
- Never-removed temporary `blocked` guard label (#70) → fixed by #73.

## Execution after approval
1. Present candidates A–G as `ghwf ask` checkboxes (plus an "other / none"
   escape), so you tick exactly the ones to file.
2. For each ticked item, `ghwf create-issue --title "…"` with a body capturing
   the evidence and proposed scope above. (These are improvement tickets, not
   blocked-by this audit issue, so I'll pass `--no-block`.)
3. Report back the created issue numbers. No PR results from this issue.
