# Plan: add a terminal `finished` phase / `ghwf:finished` label

Closes #75.

## Goal

Make it obvious on GitHub when ghwf regards an issue as done. Per the
discussion on #75, `finished` becomes a real terminal **phase**, reached
**only when the PR is merged**. In that phase the issue (and its PR) carry a
single workflow label, `ghwf:finished`, with the prior phase label
(e.g. `ghwf:review`) and the attention label both removed.

Closed-without-merge is deliberately unchanged: it keeps its existing phase
label as a record of how far the work got and just drops the attention label,
exactly as today.

This is purely the GitHub-visible label/phase catching up to "the workflow is
complete." The conclusion message Claude already posts on merge
(`render::concluded_body`) is unchanged.

## Design decisions (settled on the issue)

- **Trigger: merged only.** `Phase::Finished` is entered iff
  `state.pr_outcome == Some(PrOutcome::Merged)`. Merge is terminal and
  effectively irreversible, so stamping the phase on every run while merged is
  idempotent and safe. A closed PR never enters `Finished` (and reopening a
  closed PR still resumes the workflow as before).
- **Modelled as a phase, not a bolted-on label.** `Finished` joins the `Phase`
  enum as the last variant and gets its label slot in `[labels.phase]`. This
  keeps phase handling uniform (status rendering, label sync, the `all()` set).
- **Backward compatible config.** The new `[labels.phase].finished` key carries
  a serde default of `"ghwf:finished"`, so existing `[labels]` sections keep
  parsing unchanged and need no migration.

## Changes

### 1. `src/state.rs` — the `Finished` phase variant

- Add `Finished` as the **last** variant of `enum Phase` (after `Review`). It
  must stay last so the derived `Ord` keeps workflow order — `Finished` being
  the maximum means any approval directive on a merged issue classifies as
  stale (correct: nothing advances past a merge).
- `Phase::next()`: add `Phase::Finished => None`. Leave `Review => None`
  unchanged — `Finished` is reached by the merge, not by an approval/advance,
  so `Review` does not gain a successor.
- `Phase::label()`: add `Phase::Finished => "finished"` (matches the on-disk
  kebab serialization `"finished"`).
- `Phase::approval_command()`: add `Phase::Finished => None`.

### 2. `src/main.rs` — stamp the phase on a recorded merge

- In `run` (around lines 421–446, where `issue_state.pr_outcome` is computed
  and `advance_phase` / `advance_on_pr_ready` run), after those advance steps
  and **before** `let phase = issue_state.phase;` (~line 453), add:

  ```rust
  // A merged PR is terminal: the workflow has finished. Stamp the phase so the
  // labels and status collapse to the single `finished` state. (Closed-without-
  // merge is not "finished" — it keeps its phase as a record.)
  if issue_state.pr_outcome == Some(state::PrOutcome::Merged) {
      issue_state.phase = state::Phase::Finished;
  }
  ```

  Placing it after directive processing means a merge never suppresses a
  legitimate transition/note the same run wants to show; placing it before the
  `phase` capture means the label sync and status see `Finished`.

- Exhaustiveness: the inner `None => match phase { … }` at ~line 485 (the
  phase-body selector, only reached when `pr_outcome` is `None`) needs a
  `state::Phase::Finished => unreachable!("finished implies a merged PR, so \
  pr_outcome is Some")` arm. `Finished` cannot occur there because it implies a
  merged outcome, which the outer `match issue_state.pr_outcome` routes to
  `concluded_body`.
- `conflict_base` (~line 539) already falls through its `_ => None` arm for
  `(Some(Merged), _)`, so no change — a merged PR gets no conflict detection.
- `needs_worktree_guard` (~line 1055) already returns early on
  `pr_outcome.is_some()`, so `Finished` needs nothing there.

### 3. `src/config.rs` — the `finished` phase label

- Add to `PhaseLabels`:

  ```rust
  #[serde(default = "default_finished_label")]
  pub finished: String,
  ```

  with a free function `fn default_finished_label() -> String {
  "ghwf:finished".to_string() }`. The serde default is what preserves
  backward compatibility for existing `[labels.phase]` tables.
- `LabelsConfig::for_phase`: add `Phase::Finished => &self.phase.finished`.
- `LabelsConfig::all`: return `[&str; 8]`, adding `&self.phase.finished`. The
  `.into()` to `BTreeSet` in `labels.rs` is unaffected.
- Update the existing test (~lines 370–393): its `[labels.phase]` omits
  `finished`, which now exercises the serde default — assert
  `labels.for_phase(Phase::Finished) == "ghwf:finished"` and change
  `labels.all().len()` from `7` to `8`.

### 4. `src/labels.rs` — defaults, wizard section, sync

- Add a `finished` entry to `DEFAULTS`, in the phase block (so it lands in
  `[labels.phase]`):

  ```rust
  (
      "finished",
      "ghwf:finished",
      "8957e5", // GitHub's merged-purple, signalling a concluded workflow
      "ghwf: workflow complete",
  ),
  ```

- Bump `PHASE_DEFAULTS` from `4` to `5`. `labels_section()` slices
  `DEFAULTS[..PHASE_DEFAULTS]` for the phase table and the rest for attention,
  so this keeps `finished` in `[labels.phase]` and the three attention labels
  intact.
- `desired_labels` needs no change: with `phase == Finished`,
  `for_phase(Finished)` yields `ghwf:finished`, and `attention` is already
  `None` for a concluded workflow (computed in `sync` as
  `pr_outcome.is_none().then_some(...)`), so the desired set is exactly
  `{ghwf:finished}`. `sync_thread` then removes the stale phase label.
- Update tests:
  - `generated_section_parses_and_covers_every_state`: assert
    `for_phase(Phase::Finished) == "ghwf:finished"`; the attention-count check
    `DEFAULTS.len() - PHASE_DEFAULTS == 3` still holds.
  - Add a focused test: `desired_labels(&labels, Phase::Finished, None)` equals
    `["ghwf:finished"]` (mirrors the existing
    `concluded_workflow_drops_the_attention_label` case).

### 5. `src/render.rs` — status prose for the finished phase

- `phase_status_prose` (~line 555) is an exhaustive `match`. Although a merged
  run routes status through `conclusion_status_prose` (so this is normally not
  hit for `Finished`), a later manual `work-on` with `conclusion == None` could
  reach it, so give it a real arm, e.g.:

  ```rust
  Phase::Finished => "The workflow is **finished**: the PR was merged and the \
      work is complete.".to_string(),
  ```

- `hand_off_prompt` (~line 588): add `(Phase::Finished, _) => None` (terminal,
  nothing to advance) — or fold into the existing `(Phase::Review, _) => None`
  as `(Phase::Review | Phase::Finished, _) => None`.
- `status_primary_is_pr` (~line 617): include `Finished` in the PR-primary set
  (`matches!(phase, Implement | Review | Finished)`) so the merge/completion
  status still posts primarily to the PR thread, preserving today's routing
  (currently the merge is observed while the phase is still `Review`).
- `advance_hint` (~line 475) already covers `Finished` via its `(None, _)` arm
  ("there is nothing further to approve") — no change.
- `Phase::label()`-based call sites (status line "Phase: finished",
  `render_transition`) work automatically from the new `label()` arm.

### 6. `src/stop_hook.rs` — no change

`block_reason` is only reached when `pr_outcome.is_none()` (line 77 lets the
session go once an outcome is recorded), so a `Finished` phase never reaches it;
and it uses `phase.label()` rather than an exhaustive match regardless.

### 7. Docs — `README.md`

- In the **Phases** list (lines ~13–25) and/or the paragraph that follows about
  merging completing the workflow, note that a merged PR moves the issue to a
  terminal **finished** state carrying the `ghwf:finished` label (and that the
  phase/attention labels come off). Keep it brief and consistent with the
  existing prose.
- The annotated `ghwf.toml` example does not currently include a `[labels]`
  section, so there is no per-key line to add there; the label set is generated
  by `ghwf config labels`. The `config init` wizard requires no new prompt —
  it sets up labels wholesale via `labels::configure_at`, which reads
  `DEFAULTS`, so the new label is created automatically for fresh setups.

## Existing installs (the maintainer's repo)

`ghwf config labels` / `config init` create `ghwf:finished` for **new** setups.
For a repo that already has a `[labels]` section, `config labels` bails by
design, so the label is not auto-created there. The sync is best-effort and
just warns on a missing label, so nothing breaks; the one-time fix is to create
the label by hand (GitHub UI or
`gh label create "ghwf:finished" --color 8957e5 --description "ghwf: workflow complete"`).
I'll call this out in the PR description rather than expanding the
`config labels` flow (out of scope for #75).

## Testing

- `cargo test` — covers the new/updated unit tests in `config.rs`, `labels.rs`,
  and any `state.rs`/`main.rs` phase tests touched by the new variant.
- `cargo clippy` — confirm no new warnings and that every `Phase` match is
  exhaustive.
- Manual sanity: a config round-trip test already proves an existing
  `[labels.phase]` without `finished` parses (serde default) and that
  `desired_labels(Finished, None) == {ghwf:finished}`.

## Out of scope

- Auto-creating the new label in already-configured repos (documented manual
  step instead).
- Any change to closed-without-merge behaviour or to the conclusion messages.
