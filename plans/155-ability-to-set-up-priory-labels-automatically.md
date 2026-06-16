# Plan: set up priority labels automatically (#155)

## Goal

Today `priority_labels` is just a list of names in `ghwf.toml` that `ghwf next`
reads off issues to rank them. ghwf never creates those labels on GitHub, and
the wizard offers no suggested default. This change makes ghwf *set them up* for
you:

- a suggested default set (`high-priority`, `medium-priority`) offered by the
  `ghwf config init` wizard, with an option to create them on GitHub there and
  then;
- a new `ghwf config priority-labels` command that creates/adopts the configured
  priority labels on existing repos/clones, on demand and idempotently;
- recognised names get sensible colours (`high-priority` → GitHub's bright amber
  `fbca04`, `medium-priority` → its pale variant `fef2c0`); unrecognised names
  get a colour derived deterministically from the name.

Adoption rule: if a label with the configured name already exists in the repo,
leave it untouched; otherwise create it. Never recolour an existing label.

Decisions confirmed on the issue:
- separate `ghwf config priority-labels` command (not folded into the existing
  one);
- rename the existing `ghwf config labels` → `ghwf config state-labels`, keeping
  `labels` as a hidden alias so existing usage keeps working;
- unrecognised-label colours are deterministic from the name ("digest"), not
  truly random — stable across runs and testable.

This mirrors the existing workflow-status-label system (`src/labels.rs`), which
already creates labels idempotently across the code repo plus every
`issue_repos` repo.

## Out of scope

- No new config *field*: the recognised-colour table is code, not config, so the
  `config ls`/`info`/`example` schema and the "Adding a config option" checklist
  don't apply. (`priority_labels` itself is already a documented field.)
- No `[priority_labels]` table with per-label colour overrides — the issue hints
  a larger recognised list may come later; the table is structured to grow, but
  we don't add config-driven colours now.
- We don't recolour or rename labels that already exist.

## New module: `src/priority_labels.rs`

A dedicated module for *setting up* priority labels, sitting alongside
`src/labels.rs` (which keeps owning workflow *state* labels). Wire it in
`src/main.rs` with `mod priority_labels;`.

### Recognised colours

```rust
/// Priority-label names we recognise, each with the colour and description we
/// create it with. Matching is case-insensitive. Deliberately easy to grow
/// (the issue anticipates a larger pre-canned list later).
const RECOGNISED: &[(&str, &str, &str)] = &[
    // GitHub's bright amber.
    ("high-priority", "fbca04", "High priority"),
    // GitHub's pale amber.
    ("medium-priority", "fef2c0", "Medium priority"),
];
```

### Colour resolution

```rust
/// The colour to create `name` with: its recognised colour if we know the name
/// (case-insensitive), otherwise a colour derived deterministically from the
/// name so a given label always lands on the same colour.
fn colour_for(name: &str) -> String { ... }

/// The description to create `name` with: the recognised one, else empty.
fn description_for(name: &str) -> &'static str { ... }
```

The digest colour must be **stable across processes**, so it cannot use
`std::collections::hash_map::DefaultHasher` (its `RandomState` seed is
randomised per run). Implement a tiny deterministic hash (FNV-1a over the
lowercased name's bytes) and take three bytes as the RGB hex:

```rust
/// Deterministic 6-hex colour from a label name (FNV-1a; NOT DefaultHasher,
/// whose seed is randomised per process).
fn digest_colour(name: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in name.to_ascii_lowercase().bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:06x}", hash & 0xffffff)
}
```

### Upsert across repos

Mirror `labels.rs`'s repo set and adoption flow, but driven by the
`priority_labels` list rather than a `[labels]` section. There's no TOML section
to append — the names already live in `priority_labels` — so this is a pure
"create what's missing" reconcile that behaves the same first time and on re-run.

```rust
/// `ghwf config priority-labels`: create the configured priority labels in the
/// code repo and every `issue_repos` repo, adopting (leaving untouched) any
/// that already exist. Idempotent: safe to re-run, and run on existing clones.
pub fn configure() -> Result<()> {
    let located = config::require()?;
    create_for(&located)
}

/// The body, also called by the `config init` wizard once it has located the
/// config and written the chosen `priority_labels`.
pub fn create_for(located: &config::Located) -> Result<()> {
    let labels = &located.config.priority_labels;
    if labels.is_empty() {
        println!(
            "No `priority_labels` configured; nothing to create. \
             Set some (e.g. via `ghwf config init`) first."
        );
        return Ok(());
    }
    let mut repos = vec![github::repo_or_cwd()?];
    repos.extend(located.config.issue_repo_refs()?);
    let mut created = 0;
    for (owner, repo) in &repos {
        created += create_in_repo(owner, repo, labels)?;
    }
    if created == 0 {
        println!("All configured priority labels already exist; nothing to create.");
    }
    Ok(())
}
```

`create_in_repo` lists existing labels, builds a **lowercased** membership set
(GitHub label names are unique case-insensitively, so a case-only difference
must count as already-present to avoid a 409 on create), then for each
configured name not present creates it with `colour_for`/`description_for` and
prints `Created label \`{name}\` in {owner}/{repo}.` — matching the existing
`labels.rs` wording. Factor the "which to create" decision into a pure helper
(`labels_to_create(labels, &existing_lower) -> Vec<(&str, String, &str)>`) so it
can be unit-tested without GitHub.

## CLI: rename + new command (`src/main.rs`)

In `enum ConfigCommands`:

- Rename `Labels` → `StateLabels`, and add `#[command(alias = "labels")]` so the
  old `ghwf config labels` keeps working (hidden alias; not advertised in help).
  Update its doc comment to "workflow status labels" wording and the
  `state-labels` name.
- Add a `PriorityLabels` variant with a `///` doc comment describing it
  (create/adopt the configured priority labels across the code and issue repos).

In the `Commands::Config` match:

```rust
ConfigCommands::StateLabels => labels::configure(),
ConfigCommands::PriorityLabels => priority_labels::configure(),
```

(Clap derives `state-labels` / `priority-labels` as the kebab-case subcommand
names automatically.)

## Wizard (`src/init.rs`)

### Suggested default for the priority-labels prompt

In the existing `priority_labels` block (around line 161), give the text prompt
a suggested default so the recommended set is offered:

```rust
let input = prompt(
    Text::new("Priority labels, most urgent first (comma-separated):")
        .with_default("high-priority, medium-priority")
        .prompt(),
)?;
```

The `Confirm` gate stays (opt-in, consistent with the other optional extras);
the default text supplies the suggestion the issue asks for.

### Offer to create them on GitHub

When priority labels are configured in this run (i.e. `set_priority_labels` was
called / `priority_labels` ends up non-empty in the doc), offer to create them,
mirroring the existing workflow-status-labels offer (init.rs:362):

- add a `run_priority_labels: bool` flag, set from a
  `Confirm::new("Create these priority labels in the GitHub repo now?")`
  with `.with_default(true)`;
- add an action line to the "About to:" summary
  ("create the priority labels in the GitHub repo");
- in the execute section, **after** the config file is written (so the typed
  config carries the new `priority_labels`), call
  `priority_labels::create_for(&located)` using the already-constructed
  `Located { dir, config: typed }`. Treat failure as best-effort with a
  `warning:` to stderr, exactly like the `labels::configure_at` call at
  init.rs:458 — the config is saved and `ghwf config priority-labels` can finish
  the job later.

Note the existing wizard only builds the `Located`/`typed` value inside the
`run_labels` branch (init.rs:450-454); refactor so it's available to both the
state-labels and priority-labels creation steps (construct it once if either
flag is set).

Edge case: if the user already had `priority_labels` configured (the block at
161 is skipped because `doc.contains_key("priority_labels")`), don't surprise
them with a creation prompt in `init`; direct them to `ghwf config
priority-labels` instead. Keep the create-offer tied to having just set them in
this run.

## Doc / comment updates for the rename

Live references to `ghwf config labels` that users see, update to
`ghwf config state-labels`:

- `src/config.rs:34` — `labels` field doc comment.
- `src/init.rs:406` and `src/init.rs:464` — the "Run `ghwf config labels` …"
  pointers.
- `src/labels.rs` doc comments at lines ~176, ~238, ~377, ~441.
- `src/main.rs` `ConfigCommands` doc comments.

(`plans/*.md` are historical records — leave them.)

### README.md

- Config-commands paragraph (around 346-359): rename to `state-labels`, mention
  the `labels` alias once, and add `ghwf config priority-labels`.
- Wizard extras list (347-348): note that `config init` can now also create the
  priority labels.
- Annotated example near `priority_labels = ["urgent", "soon"]` (line 380):
  switch the sample to `["high-priority", "medium-priority"]` and add a comment
  that `ghwf config init` / `ghwf config priority-labels` create them on GitHub.

## Tests

In `src/priority_labels.rs` (`#[cfg(test)]`):

- `recognised_colours_are_case_insensitive` — `colour_for("High-Priority")`
  returns `fbca04`; `medium-priority` returns `fef2c0`.
- `digest_colour_is_deterministic_and_valid` — same name → same 6-hex string
  across calls; output is exactly 6 lowercase hex chars; two different names
  generally differ (spot-check a couple).
- `labels_to_create_skips_existing_case_insensitively` — given an existing set
  containing `High-Priority`, a configured `["high-priority", "soon"]` yields
  only `soon` (with its digest colour), proving adoption + colour selection.
- `empty_priority_labels_creates_nothing` — `labels_to_create` over an empty
  list returns empty.

In `src/main.rs` (or wherever CLI parsing is tested, if such tests exist): a
parse test that `ghwf config labels` still resolves to the renamed variant via
the alias, and that `ghwf config priority-labels` parses. If there's no existing
CLI-parse test harness, rely on the `#[command(alias)]` being a compile-time
guarantee and skip this.

Existing `src/labels.rs` tests are unaffected by the rename (they exercise
functions, not the CLI name).

## Verification

- `cargo test` green.
- `cargo build` then manual smoke (don't need a live repo for help text):
  - `ghwf config --help` shows `state-labels` and `priority-labels`, not
    `labels`.
  - `ghwf config labels --help` still works (alias).
  - `ghwf config priority-labels` in a repo with `priority_labels` set creates
    the missing ones and adopts existing same-named labels; a re-run reports
    "All configured priority labels already exist".
- `ghwf config init` in a fresh repo suggests `high-priority, medium-priority`
  and offers to create them.

## Files touched

- `src/priority_labels.rs` (new)
- `src/main.rs` (module decl, `ConfigCommands` rename + alias + new variant,
  match arms)
- `src/init.rs` (suggested default, create-offer wiring, `Located` refactor,
  pointer text)
- `src/config.rs` (doc-comment rename)
- `src/labels.rs` (doc-comment rename)
- `README.md` (commands, wizard extras, annotated example)
