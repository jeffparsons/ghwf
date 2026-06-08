# Plan: ghwf config wizard (#36)

## Summary

A new interactive subcommand, `ghwf config init`, that sets up `ghwf.toml`:

- **Essentials** (`worktrees_dir`, and `main_repo` where the layout needs
  it), skipped when already present.
- **Optional extras**: `priority_labels`, a `pull-request.md` stub, and the
  `[labels]` section — the last by offering to run the `ghwf config labels`
  logic inline, falling back to printing a pointer to it.

Two new dependencies: `inquire` for the prompts (arrow-key selection,
defaults, validation) and `toml_edit` for format- and comment-preserving
edits to an existing config. (`toml` 0.8 already depends on `toml_edit`
internally, so the second adds almost nothing to the build.)

## 1. CLI (`main.rs`)

- `ConfigCommands::Init` — "Interactively create or extend `ghwf.toml`:
  essentials if missing, then optional extras." Dispatches to
  `init::run()` in a new `src/init.rs`.

## 2. Wizard structure (`src/init.rs`)

Gather-then-execute, so an abort mid-way mutates nothing:

1. **Guard.** Bail early (via `std::io::IsTerminal` on stdin/stdout) with
   "ghwf config init is interactive; run it in a terminal." Inquire's
   cancel/interrupt errors (Esc, Ctrl-C) abort the whole wizard with
   "aborted; nothing written".
2. **Gather** all answers through the steps below, accumulating planned
   actions (file edits, directory creation, gitignore line, labels run).
3. **Confirm**: print the action list and ask one final `Confirm` before
   touching anything.
4. **Execute**: write the file, create directories, then (if chosen) run the
   labels setup, which appends its own section to the file on disk. Finish
   with a summary; when labels setup was declined, print the
   "run `ghwf config labels` later" pointer here.

The file is edited as a `toml_edit::DocumentMut`: parse the existing text
when a config was found, start from an empty document otherwise. All
formatting and comments in an existing file are preserved; new keys are
written with short explanatory comments matching the README's example
(via key decor), so a wizard-generated file reads like the documented one.

## 3. Locate or create

- Reuse the walk-up from `config.rs`: make `locate()` `pub(crate)`. The
  wizard deliberately does *not* go through `config::find()` — a half-written
  file (e.g. missing `worktrees_dir`) fails the typed parse, and the wizard's
  job is exactly to repair that. It reads the raw text into a `DocumentMut`
  and checks keys directly.
- **Found**: report where, treat essentials as done when the
  `worktrees_dir` key is present (it's the only required key), and proceed to
  whatever's missing — the layout step if `worktrees_dir` is absent, then
  extras.
- **Not found**: run the full flow; the config's location follows from the
  layout choice (§4).

## 4. Layout: detect, suggest, ask

Detect whether the cwd is inside a git work tree (`git rev-parse
--is-inside-work-tree`; small new helper `git::is_inside_work_tree(dir)`
next to the existing private `git_ok`). Then present a `Select` between the
two supported layouts, with the detected one pre-selected and annotated as
suggested:

- **Repo-root layout** (suggested when inside a repo): `ghwf.toml` at the
  repo root (`git rev-parse --show-toplevel`), `main_repo` omitted.
  `worktrees_dir` default `worktrees`. Because the config walk-up must find
  `ghwf.toml` from inside a worktree, the worktrees directory has to live
  under the repo root — so the wizard also offers to append the directory to
  `.gitignore` (skipped when already ignored, checked with
  `git check-ignore`).
- **Parent-dir layout** (suggested otherwise): `ghwf.toml` in the cwd, with
  `main_repo` pointing at the repo. Default for the `main_repo` prompt: scan
  the cwd's immediate children for git repos (including bare ones like
  `repo.git`) and pre-fill when there's exactly one. Validate the entered
  path is a git repo (`git rev-parse --git-dir` succeeds there).
  `worktrees_dir` default `worktrees`.

Before the layout select, remark that this is the best time to rearrange the
directory layout — change it now and re-run, rather than after the wizard
has written paths into the config.

In both layouts: prompt for `worktrees_dir` (Text with default), and offer to
create the directory if it doesn't exist.

## 5. Extras

Each offered only when not already configured:

- **`priority_labels`** (skip when the key exists): Confirm, then a Text
  prompt for a comma-separated list (most urgent first), stored as a TOML
  array. Empty answer → key not written.
- **`pr_instructions`**: the key itself stays unwritten (the default path,
  `pull-request.md` next to the config, is right for the wizard's layouts).
  When that file doesn't exist, offer to create a stub: a few lines of prose
  explaining the file holds free-form instructions for PR titles and bodies,
  with a commented example guideline. Skip the offer entirely when the file
  (or an explicit `pr_instructions` key) already exists.
- **`[labels]`** (skip when the section exists): Confirm "set up workflow
  status labels now?". Accepted → run the `config labels` logic inline during
  execute, after the config file is saved (it appends its section to the
  file on disk and creates the labels in the GitHub repo). Declined → print
  the pointer to `ghwf config labels` in the final summary.

### `labels.rs` refactor

`configure()` currently does locate + already-present check + create labels +
append. Split out `pub fn configure_at(located: &config::Located) ->
Result<()>` holding the create-and-append body; `configure()` becomes
`configure_at(&config::require()?)` plus the already-present bail. The wizard
calls `configure_at` with the location it just wrote, avoiding a second
walk-up and keeping the duplicate-section bail (which the wizard has already
ruled out) out of the shared path.

## 6. Pointers to the wizard

- `config::warn_if_absent()` and `config::require()`'s bail message both gain
  "run `ghwf config init` to set one up."
- README's Configuration section: lead with "run `ghwf config init` for an
  interactive setup", keeping the annotated example for reference.

## 7. Tests

Prompting stays a thin shell; the logic lives in pure functions over
`DocumentMut` and plain inputs, which is what gets tested:

- Essentials written into an empty document parse back through the typed
  `Config` (both layouts: with and without `main_repo`).
- Editing an existing document (comments, odd whitespace) adds
  `priority_labels` while leaving every existing byte intact.
- Missing-keys detection: `worktrees_dir` present → essentials done; absent
  key vs. present section for each extra.
- The comma-separated `priority_labels` parser (trimming, empty → none).
- The stub `pull-request.md` content is non-empty prose.
- `labels.rs`: existing tests keep passing against the refactored
  `configure_at` split.

## Out of scope

- Renaming or restructuring an existing config (the wizard only adds what's
  missing).
- Setting up the Claude Code integration (`ghwf install` remains separate;
  the final summary can mention it as a next step).
- Migrating `config labels`'s append-based write to `toml_edit` — appending
  already preserves the file as-is; not worth the churn.
