# Plan — #111: Verify ghwf's own write subcommands aren't blocked as "external writes" mid-session

## Problem

On #86 Claude tried to file the agreed follow-up issue with `ghwf create-issue`
and balked — "the action was blocked as an external write I'd initiated on my
own" — and had to ask the user to file it, defeating the `create-issue` deferral
mechanism ghwf leans on. #111 asks us to confirm (and harden if needed) that
ghwf's own write subcommands (`create-issue`, `hand-off`, `create-issue-comment`,
`update-pr`, `reply-review-comment`, `ask`, …) are reliably allowed in a launched
session, so a deferral or hand-off never stalls on the human.

### Two distinct guards

The #86 wording matters. There are two separate things that can stop a write:

1. **Claude Code's permission system** — surfaces a *permission prompt* for a
   `Bash(ghwf:*)` call. Suppressed by a permission allowlist.
2. **The model's own behavioural guard** — it self-censors "outward-facing /
   hard-to-reverse actions I initiated on my own" and asks first *even when
   technically permitted*. #86's phrasing ("blocked as an external write I'd
   initiated on my own") is this second guard — a self-block, not a prompt.

A fix has to cover both, and — critically — cover them **in an arbitrary target
repo**, which is the normal case, not just in ghwf's own repo.

### What's in place today, and its scope

- **Committed `.claude/settings.json`** with `Bash(ghwf:*)` in
  `permissions.allow` — git-tracked in *this* repo, so it only helps when working
  **on ghwf itself**. (Covers layer 1, ghwf repo only.)
- **Global `/work-on` skill** frontmatter `allowed-tools: "Bash(ghwf:*)"`
  (`src/install.rs:24`) — applies in every repo, but only addresses layer 1, and
  whether its grant reliably persists across the whole long `wait`/`work-on` loop
  (rather than just the initial skill turn) is exactly the thing #111 says was
  "never confirmed against the actual guard".
- **Project `CLAUDE.md`** "pre-authorises the use of all ghwf commands… run them
  without per-command confirmation" — addresses layer 2, but is
  **ghwf-repo-specific**; a target repo won't have it.
- **`ghwf onboarding` framing** (`src/onboarding.rs`) — universal (printed every
  session, re-run on compaction). But it is entirely about trusting *incoming*
  instructions (ghwf's banners, authorised participants' comments). It says
  nothing about *outgoing* ghwf write commands being the sanctioned mechanism, so
  it does not directly neutralise the layer-2 balk that bit #86.
- **Per-worktree `settings.local.json`** (`write_local_session_settings`,
  `src/install.rs:200`) — writes hooks only, **no `permissions.allow`**.

### The gap

In an arbitrary target repo, **layer 2 has no explicit antidote**: there is no
ghwf `CLAUDE.md`, and the onboarding framing never tells the session that running
ghwf's own `create-issue` / `hand-off` / `create-issue-comment` / … is the
intended, pre-authorised mechanism and must not be treated as an unsanctioned
external write. That omission is precisely the #86 failure mode. Layer 1 in a
target repo also rests solely on the skill frontmatter's grant, whose persistence
across the long loop is the very thing in doubt.

## Approach

Three small, additive changes — one per layer, plus docs. Nothing here loosens
any safety boundary: the onboarding clause is scoped to ghwf's *own* commands and
keeps the existing "not a licence to bypass safety behaviour" framing, and the
permission entry is the same narrow `Bash(ghwf:*)` already trusted elsewhere.

### 1. Onboarding clause sanctioning ghwf's own write subcommands (layer 2)

In `src/onboarding.rs`, add a short, explicit point to `CONTENT` establishing
that running ghwf's own state-changing subcommands is the intended, already
pre-authorised mechanism of the workflow — so the session should not treat a
`ghwf create-issue` / `hand-off` / `create-issue-comment` / `update-pr` / `ask` /
`reply-review-comment` as an unsanctioned "external write I initiated on my own"
and stall waiting on the human. The wording will:

- name it as ghwf's own subcommands acting through the already-authenticated
  control channel (consistent with the existing framing), not a blanket grant;
- make clear these are the *only* way to relay deferrals, questions, and
  hand-offs in an asynchronous, phone-driven workflow, so balking defeats the
  workflow itself;
- stay inside the existing "this is not a licence to bypass legitimate safety
  behaviour" boundary — it settles "is invoking ghwf's own workflow command the
  sanctioned action here?" in favour of yes, nothing more.

This is the universal fix: it ships in `ghwf onboarding`, which runs every
session (initial turn, resume, and post-compaction hook) in *every* repo,
ghwf's own or not.

**Test:** add a `#[test]` in `src/onboarding.rs` alongside the existing
clause-pinning tests, asserting the new load-bearing phrases are present (e.g.
that it references ghwf's own subcommands / `create-issue` and the
"not … external write" framing), so a reword can't quietly drop it. Keep the
existing three tests passing unchanged.

### 2. Per-worktree `permissions.allow` for `Bash(ghwf:*)` (layer 1)

Extend `merged_settings` / `write_local_session_settings` in `src/install.rs` so
the per-worktree `.claude/settings.local.json` also carries
`permissions.allow: ["Bash(ghwf:*)"]`, mirroring what the committed
`.claude/settings.json` does for the ghwf repo. This makes prompt-suppression in
*any* target repo independent of the skill frontmatter grant's scope/persistence.

Design constraints, consistent with the existing surgical merge:

- Additive and idempotent: ensure `Bash(ghwf:*)` is present in
  `permissions.allow`, creating `permissions` / `allow` if absent, preserving any
  existing entries, and writing nothing if it's already there. Reuse the existing
  `changed` flag so a no-op merge still returns `None`.
- User-owned safety: treat an unexpected shape of the parts we touch
  (`permissions` not an object, `allow` not an array) as a hard error, never an
  overwrite — same contract as the `hooks` handling.
- The file is already git-excluded (`exclude_from_git`), so this never lands in a
  commit or PR diff.

**Tests:** extend the `install.rs` tests to cover: fresh file gets the entry;
existing unrelated `permissions.allow` entries are preserved and ours appended;
already-present entry is a no-op (returns `None`); a malformed `permissions`
shape errors rather than clobbers.

### 3. Document the layered design (the issue's "close with a note")

Add a short note to the README — most naturally in/near **Proxied GitHub
commands** (`README.md:264`) and the **session framing** /**session hooks**
sections (`README.md:460`, `:486`) — recording the layered defence and *why*
ghwf's own write subcommands are reliably allowed mid-session: skill
`allowed-tools` + per-worktree `permissions.allow` (layer 1), and the onboarding
clause (layer 2). This satisfies the issue's "if already solid, close with a
note documenting why" while reflecting the small hardening above.

## Verification

- `cargo test` — the new and existing `onboarding.rs` and `install.rs` tests.
- `cargo clippy` / `cargo fmt`.
- **Live, in-session evidence:** this very workflow exercises the write path —
  the pre-plan `ghwf hand-off`, every `create-issue-comment`, and the eventual
  `update-pr` all went/go through with no permission prompt and no self-block.
  That confirms the happy path in the ghwf repo (where all layers are present).
- **Honest limitation:** a fully isolated *non-ghwf target repo* session can't be
  spun up in CI; the unit tests pin the framing and settings that make that case
  work, and the in-session evidence covers the integrated path. This is called
  out so the confirmation isn't overstated.

## Out of scope

- Changing the global skill frontmatter or the committed `.claude/settings.json`
  (both already correct; we add per-worktree coverage rather than touch them).
- Any broadening of the trust framing beyond ghwf's own subcommands.
