//! Reflection-driven `ghwf config` helpers: `ls`, `info`, and `example`.
//!
//! These read the `Facet`-derived shape of [`Config`](crate::config::Config) —
//! its fields, their types, and their `///` doc comments — so the option
//! documentation has a single source (the struct itself) rather than drifting
//! copies. facet is used only here, for reflection; serde still parses configs.

use anyhow::{anyhow, Result};
use facet::{Def, Facet, Field, Shape, Type, UserType};
use toml_edit::DocumentMut;

use crate::config::{AttentionLabels, Config, LabelsConfig, PhaseLabels};

/// The fields of `shape` if it is a struct, else `None`.
fn struct_fields(shape: &'static Shape) -> Option<&'static [Field]> {
    match shape.ty {
        Type::User(UserType::Struct(st)) => Some(st.fields),
        _ => None,
    }
}

/// The shape `Option<T>`/`Vec<T>` wraps, or `shape` itself when it wraps nothing.
/// Lets a path step transparently through `labels` (an `Option`) and
/// `issue_repos` (a `Vec`) to the struct inside.
fn inner_shape(shape: &'static Shape) -> &'static Shape {
    match shape.def {
        Def::Option(od) => od.t(),
        Def::List(ld) => ld.t(),
        _ => shape,
    }
}

/// The TOML key for a field: its serde/facet rename when present (e.g. the
/// kebab-cased `[labels]` keys), else the Rust field name.
fn wire_name(field: &Field) -> &'static str {
    field.rename.unwrap_or(field.name)
}

/// A human-readable type for `info`: `Option<…>`/`Vec<…>` are unwrapped one
/// level so the inner type is visible.
fn type_label(shape: &'static Shape) -> String {
    match shape.def {
        Def::Option(od) => format!("{} (optional)", od.t().type_identifier),
        Def::List(ld) => format!("array of {}", ld.t().type_identifier),
        _ => shape.type_identifier.to_string(),
    }
}

/// Whether a field is itself a table the user can drill into with `config ls`.
fn is_table(field: &Field) -> bool {
    struct_fields(inner_shape(field.shape())).is_some()
}

/// The doc comment of a field as joined, trimmed lines (empty when undocumented).
fn field_doc(field: &Field) -> String {
    field
        .doc
        .iter()
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first line of a field's doc, for one-line listings.
fn doc_summary(field: &Field) -> &str {
    field.doc.first().map(|line| line.trim()).unwrap_or("")
}

/// Walk a dotted path (e.g. `labels.phase.pre-plan`) from `Config`'s shape,
/// returning the field it names. Each step matches a child by its TOML key and
/// descends through any `Option`/`Vec` wrapper.
fn resolve_field(path: &str) -> Result<&'static Field> {
    let segments: Vec<&str> = path.split('.').collect();
    let mut shape = Config::SHAPE;
    let mut found: Option<&'static Field> = None;
    for (i, segment) in segments.iter().enumerate() {
        let fields = struct_fields(shape).ok_or_else(|| {
            anyhow!(
                "`{}` is not a table, so `{}` has no sub-options",
                segments[..i].join("."),
                segment
            )
        })?;
        let field = fields
            .iter()
            .find(|f| wire_name(f) == *segment)
            .ok_or_else(|| anyhow!("unknown config option `{}`", segments[..=i].join(".")))?;
        shape = inner_shape(field.shape());
        found = Some(field);
    }
    found.ok_or_else(|| anyhow!("no config option given"))
}

/// `ghwf config ls [path]`: list the options at the top level, or within the
/// nested table named by `path`.
pub fn ls(path: Option<&str>) -> Result<()> {
    let (shape, heading) = match path {
        None => (Config::SHAPE, "ghwf.toml".to_string()),
        Some(path) => {
            let field = resolve_field(path)?;
            (inner_shape(field.shape()), path.to_string())
        }
    };
    let fields = struct_fields(shape).ok_or_else(|| {
        anyhow!(
            "`{heading}` is a {}, not a table with sub-options",
            type_label(shape)
        )
    })?;

    println!("Options in {heading}:\n");
    // A trailing `/` marks a nested table the user can `ls` into.
    let width = fields
        .iter()
        .map(|f| wire_name(f).len() + usize::from(is_table(f)))
        .max()
        .unwrap_or(0);
    for field in fields {
        let key = wire_name(field);
        let marker = if is_table(field) { "/" } else { "" };
        let name = format!("{key}{marker}");
        let summary = doc_summary(field);
        if summary.is_empty() {
            println!("  {name}");
        } else {
            println!("  {name:width$}  {summary}", width = width);
        }
    }
    Ok(())
}

/// `ghwf config info <key>`: the full documentation and type of one option.
pub fn info(key: &str) -> Result<()> {
    let field = resolve_field(key)?;
    println!("{key}");
    println!("  type: {}", type_label(field.shape()));
    let doc = field_doc(field);
    if doc.is_empty() {
        println!("\n  (undocumented)");
    } else {
        println!();
        for line in doc.lines() {
            println!("  {line}");
        }
    }
    if is_table(field) {
        println!("\n  A table — run `ghwf config ls {key}` to list its options.");
    }
    Ok(())
}

/// `ghwf config example`: print a fully-filled, annotated `ghwf.toml`.
pub fn example() -> Result<()> {
    print!("{}", render_example());
    Ok(())
}

/// Never called: its exhaustive destructures are a compile-time guarantee that
/// every config field is accounted for in [`render_example`]. Add a field to
/// any of these structs and this stops compiling until the field is handled
/// below — which is exactly the "you can't forget to update the example" safety
/// net this feature is for. (`render_example` writes values explicitly rather
/// than from a built value, so the sample stays hand-curated and readable; the
/// `example_*` tests catch a field that's named here but not actually emitted.)
#[allow(dead_code, unused_variables)]
fn example_covers_every_field(
    config: Config,
    labels: LabelsConfig,
    phase: PhaseLabels,
    attention: AttentionLabels,
) {
    let Config {
        main_repo,
        worktrees_dir,
        priority_labels,
        pr_instructions,
        labels: _,
        permission_mode,
        delete_plan_on_approval,
        only_assigned_to_me,
        blocked_label,
        issue_repos,
        allowed_users,
        auto_collect_garbage,
        auto_collect_garbage_interval_hours,
        auto_merge_base,
    } = config;
    let LabelsConfig {
        phase: _,
        attention: _,
    } = labels;
    let PhaseLabels {
        pre_plan,
        prep_and_plan,
        implement,
        review,
        finished,
    } = phase;
    let AttentionLabels {
        waiting_on_user,
        waiting_on_claude,
        waiting_on_ghwf,
    } = attention;
}

/// Build the annotated example document. Comments are pulled from the structs'
/// doc comments via reflection; values are illustrative.
fn render_example() -> String {
    let mut doc = DocumentMut::new();

    // Scalars and arrays first; the `[labels]` table must come last so it
    // renders after the root keys (toml_edit keeps inserted keys before
    // sub-tables, matching TOML semantics).
    insert(&mut doc, "main_repo", toml_edit::value("repo.git"));
    insert(&mut doc, "worktrees_dir", toml_edit::value("worktrees"));
    insert(
        &mut doc,
        "priority_labels",
        toml_edit::value(["urgent", "soon"].into_iter().collect::<toml_edit::Array>()),
    );
    insert(
        &mut doc,
        "pr_instructions",
        toml_edit::value("pull-request.md"),
    );
    insert(&mut doc, "permission_mode", toml_edit::value("auto"));
    insert(&mut doc, "delete_plan_on_approval", toml_edit::value(true));
    insert(&mut doc, "auto_collect_garbage", toml_edit::value(true));
    insert(
        &mut doc,
        "auto_collect_garbage_interval_hours",
        toml_edit::value(24),
    );
    insert(&mut doc, "auto_merge_base", toml_edit::value(true));
    insert(&mut doc, "only_assigned_to_me", toml_edit::value(true));
    insert(&mut doc, "blocked_label", toml_edit::value("blocked"));

    // issue_repos shows both accepted forms: a plain "owner/repo" and the table
    // form carrying a branch_prefix.
    let mut issue_repos = toml_edit::Array::new();
    issue_repos.push("StileEducation/documentation");
    let mut detailed = toml_edit::InlineTable::new();
    detailed.insert("repo", "StileEducation/wiki".into());
    detailed.insert("branch_prefix", "wiki".into());
    issue_repos.push(toml_edit::Value::InlineTable(detailed));
    insert(&mut doc, "issue_repos", toml_edit::value(issue_repos));

    insert(
        &mut doc,
        "allowed_users",
        toml_edit::value(["octocat"].into_iter().collect::<toml_edit::Array>()),
    );

    // The [labels] table, with its kebab-cased phase/attention keys taken from
    // reflection so they can't drift from the structs.
    let mut labels = toml_edit::Table::new();
    labels["phase"] = toml_edit::Item::Table(label_subtable(
        PhaseLabels::SHAPE,
        &[
            "ghwf:pre-plan",
            "ghwf:planning",
            "ghwf:implementing",
            "ghwf:review",
            "ghwf:finished",
        ],
    ));
    labels["attention"] = toml_edit::Item::Table(label_subtable(
        AttentionLabels::SHAPE,
        &["ghwf:needs-you", "ghwf:claude-working", "ghwf:preparing"],
    ));
    // A table header takes its comment on the table's own decoration, not the
    // key-prefix treatment the scalars above use.
    labels
        .decor_mut()
        .set_prefix(comment_prefix("labels", false));
    doc.insert("labels", toml_edit::Item::Table(labels));

    doc.to_string()
}

/// Build a `[labels.*]` sub-table, pairing each field's reflected key with the
/// matching example value (the slice is in field order).
fn label_subtable(shape: &'static Shape, values: &[&str]) -> toml_edit::Table {
    let fields = struct_fields(shape).expect("label structs are structs");
    let mut table = toml_edit::Table::new();
    for (field, value) in fields.iter().zip(values) {
        table[wire_name(field)] = toml_edit::value(*value);
    }
    table
}

/// Insert `key = item` into the root table, prefixed with the field's doc
/// comment (from reflection) as `# ` lines and a separating blank line unless
/// it's the first entry. Mirrors `init.rs`'s `insert_with_comment`, but sources
/// the comment from the struct rather than a literal.
fn insert(doc: &mut DocumentMut, key: &str, item: toml_edit::Item) {
    let prefix = comment_prefix(key, doc.as_table().is_empty());
    doc.insert(key, item);
    if let Some(mut key) = doc.key_mut(key) {
        key.leaf_decor_mut().set_prefix(prefix);
    }
}

/// The `# ` comment block for `key`, sourced from reflection, preceded by a
/// blank line unless it's the first entry in the file.
fn comment_prefix(key: &str, first: bool) -> String {
    let mut prefix = String::new();
    if !first {
        prefix.push('\n');
    }
    for line in config_comment(key).lines() {
        prefix.push_str("# ");
        prefix.push_str(line);
        prefix.push('\n');
    }
    prefix
}

/// The doc comment of a top-level `Config` field, by TOML key.
fn config_comment(key: &str) -> String {
    struct_fields(Config::SHAPE)
        .expect("Config is a struct")
        .iter()
        .find(|f| wire_name(f) == key)
        .map(field_doc)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn example_round_trips_into_config() {
        let text = render_example();
        if let Err(err) = toml::from_str::<Config>(&text) {
            panic!("example did not parse:\n{text}\n{err}");
        }
    }

    #[test]
    fn example_covers_every_top_level_option() {
        // Reflection-backed completeness: every Config field must appear as a
        // key in the emitted example. Pairs with the compile-time destructure
        // guard — that forces a value to exist, this forces it to be rendered.
        let text = render_example();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let table = parsed.as_table().unwrap();
        for field in struct_fields(Config::SHAPE).unwrap() {
            let key = wire_name(field);
            assert!(table.contains_key(key), "example is missing `{key}`");
        }
    }

    #[test]
    fn example_labels_keys_match_reflection() {
        let text = render_example();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let phase = parsed["labels"]["phase"].as_table().unwrap();
        for field in struct_fields(PhaseLabels::SHAPE).unwrap() {
            assert!(phase.contains_key(wire_name(field)));
        }
    }

    #[test]
    fn resolve_walks_nested_paths() {
        assert_eq!(
            wire_name(resolve_field("worktrees_dir").unwrap()),
            "worktrees_dir"
        );
        assert_eq!(
            wire_name(resolve_field("labels.phase.pre-plan").unwrap()),
            "pre-plan"
        );
        assert!(resolve_field("labels.nope").is_err());
        assert!(resolve_field("nope").is_err());
        // A scalar has no sub-options.
        assert!(resolve_field("worktrees_dir.deeper").is_err());
    }
}
