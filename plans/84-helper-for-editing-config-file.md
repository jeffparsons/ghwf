# Plan: Helper for editing the config file (#84)

## Goal

Make ghwf's config options discoverable, and make it hard to add a new option
without surfacing it. Today the options in `Config` (`src/config.rs`) are
documented in three hand-maintained places — the struct doc-comments, the
`ghwf config init` wizard (`src/init.rs`), and the annotated `ghwf.toml` block in
the README — none of them compiler-enforced.

We'll use [`facet`](https://facet.rs/) for **runtime reflection only**: derive
`Facet` on the config structs *alongside* the existing serde derives, keep serde
+ the `toml` crate as the real parser, and use facet purely to drive three new
introspection subcommands. This keeps the config-loading path the whole tool
depends on on the stable serde stack; facet (pre-1.0, churns) only affects the
new commands.

This direction was agreed on the issue (reflection-only over a full facet-toml
migration; the migration is filed as a follow-up).

## Spike findings (facet 0.46.5, stable Rust)

Verified in a throwaway project before writing this plan:

- `#[derive(Facet)]` coexists cleanly with `#[derive(Deserialize)]` and serde
  attributes on the same struct.
- Doc comments are captured: `Field.doc: &'static [&'static str]`, one entry per
  `///` line (leading space included), multi-line preserved. The type's own doc
  is on `Shape.doc`.
- Field iteration: `shape.ty` → `Type::User(UserType::Struct(st))` → `st.fields`,
  each a `Field { name, shape, doc, attributes, flags }`.
- **Wrinkle 1:** facet requires enums to carry an explicit `#[repr(...)]`. Our
  `IssueRepo` (a `#[serde(untagged)]` enum) needs `#[repr(u8)]` added; it
  compiles and reflects fine with it, and serde ignores `repr`.
- **Wrinkle 2:** facet does **not** see serde's `rename_all`/`rename`.
  `Field.name` is the Rust identifier (`pre_plan`), not the TOML key
  (`pre-plan`). Only the `[labels.phase]`/`[labels.attention]` tables use
  `rename_all = "kebab-case"`. Handled in the renderer (apply a kebab-case
  transform for those, or — for `config example`, whose keys are hand-written —
  the correct keys are emitted directly). To confirm during implementation:
  whether a current facet attribute can carry the wire-name; if so, prefer that
  over a manual transform.

## Changes

### 1. Dependency

Add `facet = "0.46"` to `Cargo.toml`.

### 2. Derive `Facet` on the config structs (`src/config.rs`)

Add `#[derive(Facet)]` to `Config`, `LabelsConfig`, `PhaseLabels`,
`AttentionLabels`, and `IssueRepo`; add `#[repr(u8)]` to `IssueRepo`. Keep every
existing serde attribute untouched. No change to how configs are parsed.

### 3. New module `src/config_schema.rs`

Reflection-driven helpers plus the three command bodies.

- **Shape traversal helper.** Given a `&Shape`, unwrap `Option<T>`/`Vec<T>` to the
  inner user-struct shape (via facet's `Def::Option`/`Def::List` — confirm the
  exact API during implementation), so `labels` (`Option<LabelsConfig>`) and
  `issue_repos` (`Vec<IssueRepo>`) resolve to their element struct. Returns the
  list of `(key, doc, is_nested_struct)` for a struct shape, applying the
  kebab-case transform for the labels sub-tables.

- **`config ls [path]`** — `ConfigCommands::Ls { path: Option<String> }`.
  No path: list top-level `Config` options, each as `key` + first doc line, and
  mark options whose inner type is a struct as drillable. With a dotted path
  (e.g. `labels`, `labels.phase`): resolve segment by segment from `Config::SHAPE`
  through the traversal helper and list that struct's fields. Unknown path → a
  clear error naming the valid children.

- **`config info <key>`** — `ConfigCommands::Info { key: String }`.
  Resolve a dotted key to a field; print the full doc (all lines), the type
  (shape) name, and whether it's optional / has a default (from serde — or simply
  from the `Option<…>` shape). Unknown key → the same kind of helpful error.

- **`config example`** — `ConfigCommands::Example`.
  Emit a fully-filled, annotated `ghwf.toml` to stdout, built with `toml_edit` in
  the same `insert_with_comment` style as `init.rs`.
  - **Compile-break safety (the core ask):** a `fn example_config() -> Config`
    built with an *exhaustive struct literal* — every field named, no `..`. Adding
    a `Config` field stops this compiling until an example value is supplied.
    (Nested `LabelsConfig`/`PhaseLabels`/`AttentionLabels` and a representative
    `IssueRepo` are built the same way.)
  - **Comments come from reflection** (`Field.doc`), keyed by field name, so the
    prose has a single source (the struct doc-comments) rather than a fourth copy.
  - **Renderer-completeness safety:** a test asserts every field name in
    `Config::SHAPE` (and the nested shapes) appears as a key in the emitted
    output — so adding a field to the literal but forgetting to render it fails
    the test, not just silently drops it.
  - A fully-reflective renderer (walk the example value with `facet-reflect`
    `Peek` and serialize each field) is the more elegant end state; pursue it only
    if it stays simple, otherwise the hand-built toml_edit document above is the
    baseline.

### 4. Wire the subcommands (`src/main.rs`)

Add `Ls`, `Info`, `Example` to the `ConfigCommands` enum (with doc-comments for
`--help`) and dispatch them to `config_schema` alongside `Init`/`Labels`.

### 5. Docs

- **README:** add a short subsection documenting `config ls` / `config info` /
  `config example`, and note that `ghwf config example` prints the canonical
  fully-filled config. Leave the existing annotated README block as-is (unifying
  it with the generated output is the out-of-scope follow-up).
- **`CLAUDE.md`** ("Adding a config option"): note that a new key is now
  automatically surfaced by `config ls`/`info` via its doc-comment, and that
  `example_config()` won't compile until the new field gets an example value.

## Tests

- `config example` output parses back into a `Config` (round-trip).
- `config example` output contains a key for every `Config` field, via
  reflection (completeness guard).
- `config ls` with no path and with a nested path (`labels.phase`) lists the
  expected keys, including the kebab-case labels keys.
- `config info` prints the doc text for a known key and errors helpfully on an
  unknown one.

## Risks / notes

- **facet is pre-1.0 (~0.46) and churns.** Accepted by the issue author. It's
  confined to the new commands; a breaking facet release can't stop ghwf reading
  a config. Pin to a `0.46`-compatible range.
- `IssueRepo` gains `#[repr(u8)]` (benign; required by the facet derive).
- serde `rename_all` is invisible to facet — handled in the renderer for the
  labels sub-tables (see Spike findings, wrinkle 2).
- The `init.rs` wizard and README example stay hand-maintained for now; fully
  collapsing all option documentation into one reflected source is the deferred
  follow-up.

## Follow-up to file

- Evaluate a full `facet-toml` migration (replace serde on the config parse path)
  once facet matures and after confirming `facet-toml` handles the untagged
  `IssueRepo` enum, `default = "fn"` defaults, and `rename_all` — folding the
  init wizard and README example into the single reflected source.
