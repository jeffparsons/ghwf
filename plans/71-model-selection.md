# Plan: Model selection (#71)

Let an issue choose the Claude model its ghwf session runs on, via a line in
the issue body. No repo configuration; the value is passed straight through to
`claude --model`.

## Behaviour

- **Source.** A standalone line in the issue body whose trimmed text begins,
  case-insensitively, with `model:`. The value is the remainder after the first
  `:`, trimmed, taken **verbatim** — so aliases (`fable`, `opus`, `sonnet`) and
  full names (`claude-fable-5`) both work.
- **Zero matching lines** → no `--model` flag; Claude's default.
- **Exactly one, non-empty value** → launch with `claude --model <value>`.
- **Two or more matching lines** → ambiguous: refuse to start (see *Refusing
  visibly*).
- **One line with an empty value** (`Model:` with nothing after it) → also a
  refusal, same path.
- **A value only Claude can reject** (not detectable without a model list) is
  passed through; Claude reports it in-session. The `state::claim` written by
  `ghwf next` before launch already excludes the issue from future picks, so a
  `next --wait` worker loop won't busy-retry it.
- `--model` is session-scoped (verified against the Claude Code settings docs):
  it overrides the model for that one session only and writes nothing to
  settings, so this never mutates the user's default.

## Where it plugs in

All changes are in `src/launch.rs`, plus tests and a README note.

### 1. Parse the model from the body (`src/launch.rs`)

A pure function over the body, unit-testable:

```rust
/// What a `Model:` line (if any) in an issue body selects.
enum ModelSelection {
    /// No `Model:` line — use Claude's default.
    Default,
    /// Exactly one line with a non-empty value, passed through verbatim.
    Selected(String),
    /// A problem ghwf refuses to start on.
    Problem(ModelProblem),
}

enum ModelProblem {
    /// One `Model:` line, but no value after the colon.
    Empty,
    /// More than one `Model:` line; carries the offending lines for the message.
    Multiple(Vec<String>),
}

fn parse_model(body: Option<&str>) -> ModelSelection
```

Match rule: for each line, `let t = line.trim();` and treat it as a model line
when `t.to_ascii_lowercase().starts_with("model:")`. The value is
`t[colon+1..].trim()`. Collect all matches:

- 0 → `Default`
- 1, value empty → `Problem(Empty)`
- 1, value non-empty → `Selected(value)`
- ≥2 → `Problem(Multiple(matched_lines))`

Deliberately strict (whole trimmed line starts with `model:`), so prose and
markdown-decorated lines (`- Model:`, `**Model:**`) don't false-positive. We can
loosen later if wanted; documented as the convention.

### 2. Resolve it once, near the top of `run` (`src/launch.rs`)

`run` already computes `owner`/`repo`/`number`, the code repo, `issue_url`,
`permission_mode`, and loads `issue_state`. After that, fetch the issue
**best-effort** and resolve the model, reusing the fetched issue on the
worktree-creation path so first launch doesn't double-fetch:

```rust
let (model, fetched_issue) = match github::fetch_issue(&issue_url, repo_ctx.as_ref()) {
    Ok(issue) => match parse_model(issue.body.as_deref()) {
        ModelSelection::Default => (None, Some(issue)),
        ModelSelection::Selected(m) => (Some(m), Some(issue)),
        ModelSelection::Problem(problem) => {
            return refuse_to_start(/* owner, repo, number, &issue_url, code repo,
                                      &mut issue_state, problem, repo_ctx */);
        }
    },
    Err(err) => {
        // Offline / transient: never block a launch (esp. an offline resume of
        // an existing worktree) on model resolution. Fall back to the default.
        eprintln!(
            "warning: couldn't fetch issue #{number} to resolve its model ({err:#}); \
             launching with Claude's default model."
        );
        (None, None)
    }
};
```

Notes:

- This is best-effort by design. The existing-worktree path already touches the
  network best-effort (`refresh_main_repo`), so an unconditional best-effort
  fetch here is consistent and keeps **offline resume working**: a fetch failure
  warns and proceeds with the default rather than erroring.
- On first launch (worktree creation) the fetch normally succeeds, so ambiguity
  is enforced there — the common case. Offline, ambiguity simply isn't detected
  until the next online launch.

### 3. Reuse the fetched issue when creating the worktree (`src/launch.rs`)

In the `None =>` arm of the worktree match (where it currently calls
`github::fetch_issue` again), use `fetched_issue` when present, else fetch:

```rust
let issue_data = match fetched_issue {
    Some(issue) => issue,
    None => github::fetch_issue(&issue_url, repo_ctx.as_ref())?,
};
```

The `None` (offline) case keeps the current mandatory-fetch behaviour, so an
offline first-launch still fails with today's clear error.

### 4. Thread the model into `exec_claude` (`src/launch.rs`)

Add a `model: Option<&str>` parameter to `exec_claude`, passed at all three call
sites (no-branch, fresh worktree, resume). It adds the flag alongside the
existing ones, before the `/work-on` positional:

```rust
if let Some(model) = model {
    cmd.args(["--model", model]);
}
```

To keep this unit-testable (the function `exec`s and can't be called in tests),
extract the flag assembly into a small helper:

```rust
/// The argument list for `claude`, in order: resume, permission mode, model,
/// then the `/work-on` initial prompt.
fn claude_args(resume: Option<&str>, permission_mode: Option<&str>, model: Option<&str>)
    -> Vec<String>
```

`exec_claude` builds its `Command` from `claude_args(...)`. Tests assert
`--model <value>` appears only when set, and ordering.

### 5. Refusal path (`src/launch.rs`)

```rust
fn refuse_to_start(/* … */, problem: ModelProblem) -> Result<()>
```

Mirrors how `hand_off` flips the issue to the user:

1. Build a status message body with `render::build_status_comment_body`:
   - `Empty` → e.g. *"Found a `Model:` line with no value. Set it to a model
     name (e.g. `Model: opus`) or remove the line, then relaunch."*
   - `Multiple(lines)` → e.g. *"Found multiple `Model:` lines; keep exactly
     one:"* followed by the quoted offending lines.
2. `github::post_issue_comment(&issue_url, &body, repo_ctx.as_ref())` — posts to
   the issue thread (its own repo, correct for a foreign `issue_repos` issue).
3. Flip attention: `issue_state.attention = state::Attention::WaitingOnUser;`
   then `labels::sync(&(owner, repo), &code_repo, number, pr_number, &mut
   issue_state)` (no-ops when labels aren't configured), then
   `state::save(&owner, &repo, number, &issue_state)`.
   `pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number)`.
4. `bail!` with the same human-readable problem, so a worker's logs/stderr show
   it and the process exits non-zero **without** `exec`ing Claude.

The claim and worktree stay put, so fixing the body and relaunching
(`ghwf work-on <n>`) re-resolves the corrected model and starts cleanly.

Posting/sync failures are best-effort warnings (don't mask the underlying
refusal); the `bail!` is what stops the launch regardless.

### 6. Launch message (`src/launch.rs`)

When a model is selected, print a short line before `exec` (alongside the
existing resume/permission-mode messages) so the user sees it, e.g.
*"Using model `opus` (from the issue's `Model:` line)."*

## Tests (`src/launch.rs` `#[cfg(test)]`)

- `parse_model`: none; one alias; one full name; case-insensitivity
  (`MODEL:`, `model:`); surrounding whitespace; value taken verbatim; empty
  value → `Problem(Empty)`; two lines → `Problem(Multiple)`; a non-matching
  decorated line (`- Model: x`) is ignored.
- `claude_args`: `--model` present only when set; correct ordering with
  resume/permission-mode and the `/work-on` positional last.

## Out of scope / housekeeping

- **No `ghwf.toml` key, no `init.rs` wizard change** — selection is body-driven.
- **README**: add a short note documenting the `Model:` line convention (one
  standalone line, aliases or full names, omit for the default, multiple lines
  rejected). Likely a brief subsection near the usage/workflow description.
- No change to in-session `ghwf work-on`: the model is fixed by the launcher at
  process start and carried through `--resume`; editing the body retunes on the
  next launch, not mid-session.
