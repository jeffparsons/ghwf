# Replace `next --forever` with a separate `forever` subcommand

## Goal

Promote forever-mode to its own top-level subcommand — `ghwf forever` —
because running `ghwf forever` reads better than `ghwf next --forever`.
Keep `--forever` on `next` as a **hidden** alias for a transitional
period so existing invocations keep working.

## Background

Today `--forever` is a flag on the `Next` subcommand (`src/main.rs`).
When set, dispatch in `main()` short-circuits to
`next::run_forever(no_branch)`, ignoring the `--wait`/`--timeout` path.
`run_forever` (`src/next.rs:174`) loops: wait for a pick, supervise the
session to conclusion, repeat. The only knob forever-mode honours is
`--no-branch`; it implies waiting, so it conflicts with `--timeout`.

## Changes

### 1. `src/main.rs` — add the `Forever` subcommand

- Add a new variant to the `Commands` enum:

  ```rust
  /// Work issues one after another, indefinitely: claim the next eligible
  /// issue, run its session to conclusion, bring it down, and pick again —
  /// parking the worker when the queue is empty. Stops when you quit a
  /// session before its workflow concludes. (This is `ghwf next` in a
  /// self-renewing supervised loop.)
  Forever {
      /// Work without a dedicated branch/worktree/PR (just write the plan file).
      #[arg(long)]
      no_branch: bool,
  },
  ```

- In the `match cli.command`, add:

  ```rust
  Commands::Forever { no_branch } => next::run_forever(no_branch),
  ```

### 2. `src/main.rs` — hide `--forever` on `next`

- Mark the existing flag hidden so it stays functional but drops out of
  help: `#[arg(long, hide = true, conflicts_with = "timeout")]`.
- Trim the `Next` doc comment's "With `--forever`, …" sentence (or
  reword it to point at `ghwf forever`) since the flag is no longer the
  advertised entry point. The dispatch arm keeps the
  `if forever { return next::run_forever(no_branch); }` short-circuit
  unchanged.

### 3. User-facing strings → `ghwf forever`

- `src/next.rs:185` — the UserQuit message says "Re-run
  `ghwf next --forever` to resume." Change to "Re-run `ghwf forever` to
  resume." Also reword the nearby "so the --forever worker is stopping"
  text if it reads better as "so the forever worker is stopping".

### 4. Doc comments mentioning `--forever`

Light touch — these are internal docs; update the ones that name the
user command, leave purely descriptive "the forever supervisor" prose
as-is:

- `src/next.rs:164` doc comment header `(next --forever)` → `(ghwf forever)`.
- `src/launch.rs`, `src/state.rs` references to "the `--forever`
  supervisor" can stay (they describe the mechanism, not the CLI
  surface), but I'll skim them for any that read as the user-facing
  command name and adjust if warranted.

### 5. `README.md` (~line 123–132)

Rewrite the paragraph that introduces `ghwf next --forever` to lead with
`ghwf forever`, noting that `next --forever` remains as a hidden alias
for now. Keep the supervisor explanation and the stop gesture intact.

### 6. Tests (`src/main.rs`)

- Update `next_forever_parses_and_conflicts_with_timeout` — still valid
  since the hidden flag parses and still conflicts with `--timeout`.
- Add a test that `ghwf forever` and `ghwf forever --no-branch` parse,
  and that `ghwf forever --timeout 30` / `--wait` are rejected (no such
  args on the subcommand).

## Out of scope

- No change to forever-mode behaviour or the supervisor loop itself.
- No removal of the `--forever` flag (that's the eventual follow-up once
  the transitional period ends).

## Verification

- `cargo test` (parse tests).
- `cargo run -- forever --help` shows the new subcommand; `cargo run --
  next --help` no longer lists `--forever`; `cargo run -- next
  --forever` still works.
