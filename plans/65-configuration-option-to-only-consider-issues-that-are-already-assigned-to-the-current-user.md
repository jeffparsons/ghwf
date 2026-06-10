# Plan: only consider issues already assigned to the current user (#65)

## Goal

Add an opt-in config option so that `ghwf next` / `ghwf next --wait` only
consider issues already assigned to the current user. This suits teams where
work is allocated by discussion or a manager rather than picked off the list.

Default off, preserving today's behaviour (unassigned issues are eligible;
assigned-to-me issues sort first).

## Behaviour

Today, `select()` in `src/next.rs` excludes an issue only when it has assignees
*and* the current user isn't one of them — unassigned issues stay eligible.

With the new option enabled, the candidate pool becomes exactly "issues
assigned to me": any issue not assigned to the current user is dropped,
including unassigned ones.

Issues dropped purely for being unassigned are dropped silently (not added to a
"skipped" report). Rationale: with the option on, unassigned issues are the
common case and not noteworthy — reporting each one would be noise. This matches
how issues-assigned-to-someone-else are already dropped silently today. (Per the
pre-plan hand-off; revisit if visibility is wanted later.)

## Config key

`only_assigned_to_me: bool`, `#[serde(default)]`, default `false`.

## Changes

### 1. `src/config.rs`
Add to the `Config` struct:

```rust
#[serde(default)]
pub only_assigned_to_me: bool,
```

### 2. `src/next.rs`
- Thread the flag from config through to `select()`:
  - In `pick()` and `wait_for_pick()`, read it alongside `priority_labels` from
    the located config (default `false` when there's no config), e.g.

    ```rust
    let (priority_labels, only_assigned_to_me) = match config::find()? {
        Some(located) => (
            located.config.priority_labels,
            located.config.only_assigned_to_me,
        ),
        None => (Vec::new(), false),
    };
    ```
  - Add an `only_assigned_to_me: bool` parameter to `claim_pick()` and pass it
    down to `select()`.
- In `select()`, replace the current assignee guard:

  ```rust
  if !issue.assignees.is_empty() && !assigned_to(issue, me) {
      continue;
  }
  ```

  with:

  ```rust
  if only_assigned_to_me {
      if !assigned_to(issue, me) {
          continue;
      }
  } else if !issue.assignees.is_empty() && !assigned_to(issue, me) {
      continue;
  }
  ```
- Update the `select()` / `pick()` doc comments to mention the option.

### 3. `src/init.rs`
Add a wizard prompt following the `delete_plan_on_approval` pattern: a
`Confirm` defaulting to `false`, guarded by `!doc.contains_key("only_assigned_to_me")`,
that calls a new `set_only_assigned_to_me()` helper using `insert_with_comment()`
to write the key with an explanatory comment.

### 4. `README.md`
Add `only_assigned_to_me` to the annotated `ghwf.toml` example with a one-line
explanation.

## Tests

- `src/next.rs`: a unit test for `select()` covering, with the flag on, that an
  unassigned issue is excluded while an assigned-to-me issue is picked; and with
  the flag off, that the unassigned issue is still eligible (guarding the
  default behaviour).
- `src/init.rs`: a round-trip test that the wizard's `set_only_assigned_to_me()`
  output parses back into `Config` with the field set, mirroring the existing
  setter tests.

## Out of scope

- No change to sort ordering (assigned-to-me already sorts first).
- No reporting/printing of issues skipped for being unassigned.
