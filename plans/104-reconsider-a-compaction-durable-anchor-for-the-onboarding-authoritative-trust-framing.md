# Plan — compaction-durable anchor for the onboarding framing (#104)

`ghwf onboarding` establishes the authoritative session framing once per launch,
on the trusted first turn (the `/work-on` skill runs it before any GitHub data is
read). The `wait`/`work-on` loop that runs for the rest of a session never
re-prints it, so the only thing that re-asserts the framing is a *process*
relaunch/resume. If Claude Code auto-compacts mid-session, the framing turn is
summarised and its load-bearing specifics (don't balk at an async GitHub approval,
don't demand a synchronous confirmation) can be weakened or dropped, with nothing
to restore them until the next relaunch.

This was deferred from #101, which floated a git-excluded `CLAUDE.local.md`
pointer. We're taking a different, stronger route.

## Decision: a `SessionStart`/`compact` hook that re-runs `ghwf onboarding`

Add one more per-worktree hook alongside the existing Stop and Notification hooks,
written into `.claude/settings.local.json` by `write_local_session_settings`:

```
SessionStart, matcher "compact", command "ghwf onboarding"
```

Claude Code fires a `SessionStart` hook after a compaction with `source:
"compact"`, and its stdout **is injected into the fresh post-compaction context**.
So this re-establishes the *full authoritative framing text* on a clean turn,
exactly when compaction would otherwise have dropped it.

Why this over the alternatives considered:

- **`CLAUDE.local.md` pointer** (the #101 idea): survives compaction, but it can
  only carry *static* text. A pointer ("run `ghwf onboarding`") still depends on
  Claude choosing to act on it, and inlining the full text would duplicate the
  single source. It also adds a small permanent per-turn context cost on *every*
  session, compacted or not. The hook fires only when needed and injects the real
  text.
- **`PreCompact` hook**: its stdout goes only to the debug log — it is *not* shown
  to the model — so it can't re-inject the framing. (It can only block compaction,
  which we don't want.)
- **Periodic re-assertion in the loop output**: would re-print on every `work-on`
  round, adding noise to banners, and still only fires when the loop next cycles.
  The hook is targeted at the actual event.

`ghwf onboarding` stays the **single source** of the framing text — the hook just
runs it. This reuses machinery we already have: the same matcher-scoped
`merged_settings`/`ensure_hook` plumbing that writes the Stop and Notification
hooks, git-excluded and refreshed from the binary on every launch.

### Scope decision: `compact` only, not also `resume`

In the pre-plan hand-off I floated also scoping the hook to `resume` (to cover a
user's own out-of-band `claude --resume`). On implementation inspection I'm
**dropping `resume`** for two concrete reasons:

1. The launcher already re-passes `/work-on` as the initial prompt on resume, so a
   ghwf-driven resume re-runs onboarding via the skill. The only uncovered case is
   a manual `claude --resume` outside ghwf — a genuine edge, and not what this
   issue (compaction) is about.
2. `ensure_hook` identifies "our" entries by **command substring alone**
   (deliberately tolerant of a user reformatting the matcher — see
   `merge_recognises_ours_with_extra_wrapping`). Two `SessionStart` entries that
   both run `ghwf onboarding` (one `compact`, one `resume`) would be
   indistinguishable to that dedup: the second would never be added, and making
   dedup matcher-aware would break the existing reformatting tolerance. Covering
   `resume` would therefore need either an unproven regex/alternation matcher or a
   refactor of well-factored logic — not worth it for the edge case.

Scoping to a single `compact` matcher keeps the new entry's command (`ghwf
onboarding`) unique, so the existing command-substring dedup keeps working
unchanged and re-install stays idempotent. (If we later want `resume` too, that's
a separate, deliberate change to the dedup model.)

## Changes

### 1. `src/install.rs` — register the hook

- Add a constant for the command, e.g.
  `const SESSION_START_HOOK: (&str, &str) = ("compact", "ghwf onboarding");`
  (matcher, command), mirroring `NOTIFICATION_HOOKS`.
- In `merged_settings`, after the Notification loop, add:
  `if ensure_hook(hooks, "SessionStart", Some("compact"), "ghwf onboarding")? { changed = true; }`
- No change to `ensure_hook`/`entry_has_command` — the command is unique, so
  command-substring dedup already does the right thing.

### 2. `src/install.rs` — tests (mirror the existing ones)

- Extend `merge_into_empty_settings_installs_stop_and_notification` (or add a
  sibling test) to assert the `SessionStart` entry is present with
  `matcher == "compact"` and command `ghwf onboarding`.
- `merge_is_idempotent` already re-merges the full output, so it will cover the new
  hook automatically once it's emitted; confirm it still passes.
- Add a malformed-shape case to `merge_rejects_wrong_shapes` that exercises
  `SessionStart` specifically — e.g. `{"hooks": {"SessionStart": "nope"}}` (Stop and
  Notification merge fine first, then `SessionStart` bails), proving the new
  `ensure_hook` call rejects a non-array like the others.
- Extend `local_settings_are_written_and_git_excluded` to also assert the written
  file contains `ghwf onboarding`.
- Optionally a small test asserting re-merging a doc that already has our
  `SessionStart`/`compact` entry is a no-op (parallels
  `merge_recognises_ours_with_extra_wrapping`).

### 3. `README.md` — document the third hook

Under "The session hooks (written per worktree, not globally)", which currently
says *"Two hooks are installed"*:

- Bump to three and add a short paragraph for the `SessionStart` hook: on
  compaction, Claude Code re-runs `ghwf onboarding` and injects its output, so the
  authoritative framing is re-established on the fresh context rather than being
  lost when the original turn is summarised.
- In "The session framing (`ghwf onboarding`)" section, add a sentence noting the
  framing is also re-asserted after a mid-session compaction via that hook, so it
  survives long sessions — closing the gap called out as deferred in #101.

## Verification

- `cargo test` (the install-module tests above; `cargo fmt`/`clippy` clean).
- Manual sanity: run `ghwf install` + a session setup and confirm
  `.claude/settings.local.json` contains the `SessionStart`/`compact` entry
  alongside the others, and that re-running is idempotent (no duplicate). Note that
  exercising real compaction end-to-end is impractical to script; we rely on
  Claude Code's documented `SessionStart`-with-`source: "compact"` stdout-injection
  behaviour for the runtime guarantee.

## Out of scope / not doing

- No `CLAUDE.local.md` file (rejected above).
- No `resume`/`startup` coverage (see scope decision); startup is already handled
  by the skill running onboarding, and a startup hook would double it.
- No change to the framing text itself or to `ghwf onboarding`.
