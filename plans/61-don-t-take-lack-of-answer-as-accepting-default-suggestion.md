# Plan: Don't take lack of answer as accepting default/suggestion (#61)

## Goal

When Claude has presented questions, options, or a suggested default and the
user replies to only *part* of it, Claude should assume more comments may still
be coming — not that the user is finished and the unanswered points are settled.
Only an **explicit phase advancement** (an `/approve-*` directive or a 👍 on a
ghwf approval prompt) may be read as accepting the defaults/suggestions Claude
left open.

## Key facts

- Phase advancement is already explicit-only: `advance_phase` (`src/main.rs`)
  fires solely on a typed `/approve-*` directive or a 👍 on a ghwf approval
  prompt. **Nothing mechanical ever advances on silence**, so no state-machine
  change is needed.
- The behaviour the issue is about is therefore governed entirely by the
  *instructional text* ghwf hands Claude:
  - the global `/work-on` skill body, `SKILL_CONTENT` in `src/install.rs`
    (applies in every phase);
  - the shared `question_instruction` helper (`src/render.rs`), appended in the
    prep and implement waiting banners (`src/prep.rs`, `src/implement.rs`);
  - `pre_plan_body` (`src/render.rs`), which inlines its own discussion prose
    rather than calling `question_instruction`, so it needs its own touch.
- `wait_instruction` is deliberately left alone: it covers the mechanics of the
  poll loop (exit 0 → `work-on`, exit 2 → `wait`), not how to interpret a reply.

This is an instructional-only change — no new subcommand, config key, or state.

## Implementation

### 1. `/work-on` skill body (`src/install.rs`, `SKILL_CONTENT`)

This is the canonical, phase-agnostic home for the rule. Add one bullet to the
list (after the hand-off bullet, before the create-issue one), worded for the
skill's `$ARGUMENTS` style:

> - Don't read a partial reply as the user being finished: if a comment
>   addresses only some of what you raised, assume more may be coming.
>   Unanswered questions, options, and suggested defaults stay open — only an
>   explicit phase approval (an `/approve-*` directive or a 👍) settles them.
>   Acknowledge what arrived, then `ghwf wait $ARGUMENTS` again instead of
>   pressing ahead on the open points.

Note: the installed skill is overwritten on the next `ghwf install` / update;
existing installs pick up the new wording then. No migration needed.

### 2. Shared `question_instruction` (`src/render.rs`)

Append a sentence so the rule reinforces in the prep and implement phases that
use this helper:

> If the user replies to only part of what you raised, assume more comments may
> be coming: answer what arrived and wait again rather than treating the
> unanswered questions, options, or suggested defaults as settled — only an
> explicit phase approval does that.

### 3. `pre_plan_body` (`src/render.rs`)

Pre-plan is the most question-heavy phase and doesn't go through
`question_instruction`, so add the same idea to its discussion paragraph (the
one that currently ends "...never raise an interactive prompt (no
AskUserQuestion)."). A short addition there, e.g.:

> A reply that addresses only part of what you asked is not a sign the user is
> done — assume more may be coming, so respond and keep waiting rather than
> proceeding on the unanswered points.

### 4. Tests (`src/render.rs`)

The existing banner tests assert on substrings of this prose, so extend them:

- Add/extend a `question_instruction` test to assert the new partial-reply
  guidance is present (alongside the existing
  `question_instruction_names_the_command_and_number`).
- Extend `pre_plan_body_steers_questions_off_interactive_prompts` (or add a
  sibling) to assert the pre-plan body carries the partial-reply guidance.

No new test infrastructure; these mirror the existing substring-assertion style.

## Out of scope / non-goals

- No change to the advancement machinery (`advance_phase`, directives,
  reactions) — it is already explicit-only.
- No new config key, subcommand, or persisted state.
- README: the annotated `ghwf.toml` / config docs are unaffected (no new key).
  The README's workflow narrative doesn't currently spell out the
  silence-vs-approval rule, so no doc change is required; can revisit if desired.
