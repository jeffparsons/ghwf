# Plan: Pull request templates / instruction files (#12)

## Goal

Let each project supply a free-form markdown file telling Claude how to write
and maintain the PR title and body. ghwf points Claude at the file from the
implement- and review-phase instructions, and tells it to finish each round of
work by checking whether the PR title/body need updating. When the file is
absent, a brief built-in default instruction applies instead. (Settled with
the user on the issue thread.)

## Design decisions (from pre-plan discussion)

- The instructions file lives **relative to the ghwf.toml directory**, not
  inside the project repo (ghwf.toml itself isn't versioned). Default path:
  `pull-request.md` next to ghwf.toml; overridable via a new optional
  `pr_instructions` key in ghwf.toml, also resolved relative to that
  directory. The file is free-form markdown and may itself reference
  repo-versioned templates (e.g. `.github/PULL_REQUEST_TEMPLATE.md`).
- When no file exists, fall back to a generic built-in instruction rather
  than staying silent.
- The end-of-round check applies in **both** the implement and review phases.
- PR creation is unchanged: ghwf still opens the draft PR with its
  placeholder body during prep-and-plan; Claude takes ownership of the
  title/body from implementation onward.
- `--no-branch` mode has no ghwf-managed PR, so it gets no PR-maintenance
  instruction.

## Changes

### 1. `src/config.rs` — new config key and path helper

- Add `pr_instructions: Option<PathBuf>` to `Config` (optional, like
  `main_repo`).
- Add a constant for the default file name, `pull-request.md`.
- Add a method on `Located`:

  ```rust
  /// Absolute path to the PR instructions file (which may not exist).
  pub fn pr_instructions_path(&self) -> PathBuf
  ```

  Joins `self.dir` with `pr_instructions` when set, else with the default
  name — mirroring `main_repo_path` / `worktrees_dir_path`.
- Tests: the key parses; configs without it keep loading and resolve to the
  default path.

### 2. `src/implement.rs` — weave the instruction into the phase bodies

- Add a small helper that renders the PR-maintenance paragraph from an
  `Option<&Path>` (the instructions file, present only when it exists on
  disk):
  - **Some(path):** read `<path>` for the project's instructions on writing
    the PR title and body; finish each round of work by checking whether the
    PR title or body should be updated to reflect the branch, and update them
    per those instructions.
  - **None:** finish each round of work by checking whether the PR title or
    body should be updated to reflect the branch; keep them accurate, concise,
    and current.
- `run(...)` and `review(...)` gain a `pr_instructions: Option<&Path>`
  parameter and pass it to `branch_body` / `review_body`, which include the
  paragraph. `review_body` frames it around feedback rounds (it currently
  says "nothing more is needed unless feedback arrives"): when pushing
  changes in response to feedback, finish the round with the same check.
- `no_branch_body` and `review_no_branch_body` are untouched (no ghwf PR).
- Extend the existing body tests: with a path, the body names the file; with
  `None`, it contains the generic instruction.

### 3. `src/main.rs` — resolve the path and plumb it through

- In `work_on`, before the phase-body match, resolve the instructions file:
  `config::find()?` → `located.pr_instructions_path()`, kept only when
  `.is_file()`. (Existence is checked here, once, so `implement.rs` stays
  filesystem-free and easy to test.)
- Pass it to `implement::run` and `implement::review` in their match arms.
  Other phases ignore it.

### 4. `README.md` — document the key

- Add `pr_instructions` to the Configuration section's example ghwf.toml with
  a comment explaining the default (`pull-request.md` next to ghwf.toml) and
  what the file is for.

## Out of scope

- Changing how/when ghwf creates the draft PR or its placeholder body.
- Any automatic templating/substitution — the file is prose for Claude, not a
  template engine.
- Per-phase instruction files beyond PR title/body guidance (a possible
  follow-up, not this issue).

## Verification

- `cargo test` (new config and body-text unit tests included).
- `cargo clippy` / `cargo fmt` clean.
- Manual: with this repo's own ghwf.toml, add a `pull-request.md`, run
  `ghwf work-on <n>` on an implement-phase issue, and confirm the banner
  points at the file; remove it and confirm the generic fallback appears.
