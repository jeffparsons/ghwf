# Plan: clean up the temporary `blocked` guard label

## Problem

`ghwf create-issue` applies the `blocked_label` (top-level config, default
`blocked`) to a follow-up issue purely as a stop-gap guard: it goes into the
create payload so a worker can't grab the issue in the window between it being
created and its native `blocked_by` dependency being set. The native dependency
is set immediately afterwards and is the durable, GitHub-UI-visible source of
truth.

But nothing ever removes the label again. The only label-removal path,
`labels::sync` (`src/labels.rs`), deliberately touches only the configured
`[labels.phase]`/`[labels.attention]` set and leaves everything else alone — and
`blocked_label` is not in that set. So once applied the guard label lingers
forever, even after the blocker closes and the issue is genuinely unblocked.

## Scope

Just the guard-label cleanup. The "phase label after merge" idea raised in the
issue thread was withdrawn — a concluded workflow keeps its phase label as a
record, exactly as it does today. No change to `labels.rs` or conclusion
behaviour.

## Fix

Single localized change in `create_issue` (`src/main.rs`, ~lines 1515–1524).

Today, after creating the issue, the dependency is set best-effort:

```rust
if let Some(blocker) = &blocker {
    if let Err(err) = github::add_blocked_by(&owner, &repo, issue.number, blocker.id) {
        eprintln!(/* couldn't set dependency; label is the fallback guard */);
    }
}
```

Change it so that **on success** of `add_blocked_by` we peel the guard label
back off, since the native dependency now stands on its own. On failure, behave
exactly as before — leave the label on as the fallback guard (the existing
warning already promises this).

```rust
if let Some(blocker) = &blocker {
    match github::add_blocked_by(&owner, &repo, issue.number, blocker.id) {
        Ok(()) => {
            // The native dependency is now the GitHub-UI truth and is visible on
            // its own, so the guard label has served its one purpose — peel it
            // back off. Best-effort: a failure here just leaves a stale label.
            if let Some(guard_label) = guard_label {
                if let Err(err) =
                    github::remove_issue_label(&owner, &repo, issue.number, guard_label)
                {
                    eprintln!(
                        "warning: filed #{} and set its `blocked_by` dependency, but \
                         couldn't remove the temporary `{guard_label}` guard label: \
                         {err:#}\nremove it by hand; the dependency is what matters.",
                        issue.number
                    );
                }
            }
        }
        Err(err) => {
            eprintln!(
                "warning: filed #{} but couldn't set its `blocked_by` dependency on \
                 #{}: {err:#}\nthe `{blocked_label}` label is set, so a worker still \
                 won't pick it up; add the dependency by hand for the GitHub UI.",
                issue.number, blocker.number
            );
        }
    }
}
```

Notes:

- `guard_label: Option<&str>` is already computed earlier and is `Some` only
  when there's a blocker *and* the label is non-empty. The inner `if let Some`
  naturally no-ops the empty/misconfigured case (nothing was added, nothing to
  remove).
- `github::remove_issue_label(owner, repo, number, label)` already exists
  (`src/github.rs:361`) and is the same call `labels::sync` uses.
- The `--no-block` / no-blocker path is unaffected: no guard label is added, so
  the whole block is skipped.

## Documentation

The label is no longer durable, so the docs that describe it as "set right
after" need a clause noting it's then removed:

- `src/config.rs` doc comment on `blocked_label` (~lines 48–52): reword to say
  the label is a transient creation-race guard that's removed again once the
  native `blocked_by` dependency is set (and only kept if that call fails).
- `README.md` — the `create-issue` command description (~lines 182–185) and the
  annotated `ghwf.toml` comment for `blocked_label` (~lines 256–259): add that
  the guard label is stripped once the dependency is in place.

No new config key, so the `ghwf config init` wizard (`src/init.rs`) is
unaffected.

## Tests

The `create_issue` body makes live `gh api` calls and has no unit-test seam; the
new branch is a thin "on Ok, also remove" and isn't worth extracting a helper
for. Existing `assemble_labels` tests still cover the add-side label assembly.

Verification is manual, mirroring how the feature was originally exercised
(`scratch-blocker` / `scratch-blocked` issues #56/#57): file a follow-up against
a blocker with `ghwf create-issue`, confirm the new issue ends up with the
native `blocked_by` dependency set **and no** `blocked` label, then delete the
scratch issues.

## Out of scope

- Phase/attention label behaviour on conclusion (withdrawn by the user).
- Retroactively cleaning the stale `blocked` label off issues created before
  this fix — there's no scan/observe path and the user didn't ask for one;
  those can be cleared by hand.
