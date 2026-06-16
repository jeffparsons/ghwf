# Plan: Surface encouraged (priority) labels to Claude on `create-issue`

Closes #156.

## Findings / framing

The issue has two halves:

1. **"Does `create-issue` let you specify labels?"** — Yes, already. `ghwf
   create-issue` takes a repeatable `--label` flag (`src/main.rs:225-227`),
   merged with the auto-applied blocked label by `assemble_labels`
   (`src/main.rs:2071`). It's documented in the README
   (`README.md:286,294`). **No code change needed here** beyond making the
   flag more discoverable to Claude.

2. **"Surface to Claude what labels we encourage it to use, in particular the
   priority labels configured for the project."** — This is the real gap. The
   `priority_labels` config field exists (`src/config.rs:26-29`) and drives
   `ghwf next` ordering and `ghwf config priority-labels` creation, but nothing
   tells a *working* session that those labels exist or that it should apply an
   appropriate one when filing a follow-up. The only create-issue guidance Claude
   sees is the static skill bullet (`src/install.rs:67-69`), which shows just
   `--title` and — being baked into the installed skill text — can't know any
   given project's labels.

Decision (from the pre-plan hand-off, defaults accepted): surface **only
`priority_labels`** (the one configured, semantically-meaningful label set), via
the **config-aware work-on banner** rather than the static skill text.

## Approach

### 1. New render helper for the encouraged-labels line

In `src/render.rs`, add a helper alongside the existing shared banner-instruction
helpers (`wait_instruction`, `question_instruction`, `reply_where_asked_instruction`):

```rust
/// One banner line telling Claude which labels the project encourages it to
/// apply when filing a follow-up with `ghwf create-issue` — currently the
/// configured priority labels. `None` when none are configured, so the line
/// is omitted entirely rather than shown empty.
pub fn encouraged_labels_instruction(priority_labels: &[String]) -> Option<String> {
    if priority_labels.is_empty() {
        return None;
    }
    let list = priority_labels
        .iter()
        .map(|l| format!("`{l}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "When you file a follow-up with `ghwf create-issue`, attach labels with \
         `--label <name>` (repeatable). This project's priority labels, most \
         urgent first, are: {list}. Apply the one that matches the follow-up's \
         urgency when it's clear; leave it off when it isn't."
    ))
}
```

Wording notes: "most urgent first" mirrors how `priority_labels` is defined
(`src/config.rs:26`) and surfaced for `ghwf next`. It's an encouragement, not a
mandate — Claude should omit a priority when urgency is unclear, so we don't get
spurious labels.

### 2. Append the line at the banner chokepoint

The full banner is assembled once, at `render::render_phase_banner`
(called from `src/main.rs:1250`), where the located config is already in scope
(it's read for `delete_plan_on_approval` at `src/main.rs:820`). This is the
single DRY chokepoint, so thread the line through here rather than into each
per-phase body function (which live in different modules: `prep::run`,
`implement::run/review`, `render::pre_plan_body`).

- Add a parameter to `render_phase_banner` — e.g. `extra_instruction:
  Option<&str>` — appended to `out` after `body` (with a blank-line separator)
  when `Some`.
- At the call site (`src/main.rs:1250`), compute the line only for **live
  working phases** — i.e. when `issue_state.pr_outcome.is_none()` (a concluded
  PR shows `concluded_body`, where filing follow-ups is moot). Build it from the
  located config's `priority_labels`:

  ```rust
  let encouraged = issue_state
      .pr_outcome
      .is_none()
      .then(|| located.as_ref().and_then(|l|
          render::encouraged_labels_instruction(&l.config.priority_labels)))
      .flatten();
  ```

  and pass `encouraged.as_deref()` into `render_phase_banner`.

This makes the line appear in every live phase (pre-plan discussion, planning,
implementing, review) — out-of-scope discoveries can happen in any of them — and
disappear automatically once `priority_labels` is empty or the PR concludes.

### 3. Make the `--label` flag discoverable in the skill text

Update the create-issue bullet in `src/install.rs` (the installed `/work-on`
skill, ~lines 67-69) so the static guidance mentions the option and points at
the banner for the project-specific list:

> When you decide to defer work or discover something out of scope, file it with
> `ghwf create-issue --title "..."` (body from stdin) instead of dropping it; by
> default the new issue is marked blocked by the one you're working on. Attach
> labels with `--label <name>` (repeatable) — the work-on banner lists the
> project's encouraged (priority) labels.

(`onboarding.rs` only references `create-issue` in passing — no change needed
there.)

## Tests

- `src/render.rs` unit tests (alongside the existing `pre_plan_body_*` /
  `banner_*` tests):
  - `encouraged_labels_instruction` returns `None` for an empty slice.
  - For `["high-priority", "medium-priority"]` it names both, in order, and
    mentions `--label`.
  - `render_phase_banner` appends the extra instruction after the body when
    `Some`, and omits it (no stray separator) when `None`.
- `src/install.rs`: extend the existing `skill_advertises_create_issue` test (or
  add a sibling) to assert the skill text mentions `--label`.

## Docs

- **README.md**: the `create-issue` entry already documents `--label`
  (`README.md:286,294`). Add a short note that ghwf surfaces the project's
  configured `priority_labels` to the working session as encouraged labels (near
  the create-issue bullet or the `priority_labels` description around line 100).
- No new config field is introduced (`priority_labels` already exists), so the
  `ghwf config init` / `config example` / annotated-`ghwf.toml` checklist from
  CLAUDE.md does **not** apply.

## Out of scope

- A broader, separately-configured "encouraged labels" set distinct from
  `priority_labels` — deferred per the pre-plan decision to keep to the existing
  semantically-meaningful set.
- Auto-applying a priority label on Claude's behalf — we surface and encourage;
  the choice stays Claude's per follow-up.
