# Plan — pass --no-branch and the issue through from outer to inner ghwf (#13)

Two gaps in the outer-launcher → inner-session handoff, and the agreed fixes
(options 1a + 2a + 2b from the pre-plan discussion):

1. **`--no-branch` is lost.** The launcher honours it for the launch itself
   (`launch.rs:43-50`) but persists nothing, so the inner
   `ghwf work-on <n>` (run without the flag) records **branch mode** on first
   prep entry and creates a worktree anyway. Fix: persist the mode in issue
   state at launch time; the inner side's "recorded mode wins" logic already
   exists.
2. **The user must re-type the issue number** as `/work-on <n>` in the fresh
   session. Fix: set a `GHWF_ISSUE` env var on the exec'd `claude`, make the
   issue argument optional on the subcommands the skill runs, and fall back to
   env var → worktree inference. Explicit argument always wins silently.

## 1. Persist `--no-branch` in state at launch (gap 1)

In `launch::run`, the `use_no_branch` path currently saves nothing. When
`issue_state.prep` is `None` and `use_no_branch` is true, record and save:

```rust
issue_state.prep = Some(PrepState { no_branch: true, ..Default::default() });
state::save(&owner, &repo, number, &issue_state)?;
```

The branch-mode path already persists prep state via `ensure_worktree`, and
`prep::run` (prep.rs:107) only records the mode when `state.prep.is_none()`,
so the inner run picks the recorded mode up unchanged. The existing
"recorded mode wins" warnings in both `launch.rs:28-39` and `prep.rs:115`
keep handling conflicting later flags.

Update the comment in `prep::run` ("the outside-Claude launcher may already
have created branch-mode prep state") — it can now be no-branch prep state too.

## 2. `GHWF_ISSUE` env var (gap 2, half one)

- Add `pub const ISSUE_ENV: &str = "GHWF_ISSUE";` in `store.rs`, beside
  `SESSION_ID_ENV`.
- In `launch.rs`, pass the resolved issue to `exec_claude` and set it on the
  child: `cmd.env(store::ISSUE_ENV, url)`. Use the canonical issue URL
  (`https://github.com/{owner}/{repo}/issues/{number}`) — already accepted
  everywhere an issue ref is, and unambiguous across repos. Set it on **both**
  launch paths (no-branch and worktree). `ghwf next`'s launcher path goes
  through `work_on` → `launch::run`, so it inherits this for free.

## 3. Optional issue argument + resolution chain (gap 2, half two)

Make the `issue` argument optional (`Option<String>`) on **`work-on`**,
**`create-issue-comment`**, **`wait`**, and **`worktree-path`** (the four
issue-taking commands; the skill runs the first three). Resolve via a shared
helper before dispatch in `main()`:

```rust
/// Resolve the issue to operate on: explicit argument, then $GHWF_ISSUE,
/// then the worktree the cwd is inside. Errors when none applies.
fn resolve_issue_arg(arg: Option<String>) -> Result<String>
```

1. `Some(arg)` → use it (explicit argument wins silently over the env var,
   per the pre-plan discussion).
2. `GHWF_ISSUE` set and non-empty → use its value.
3. Worktree inference: walk `data_dir()/issues/<owner>/<repo>/*.json` (all
   owners/repos — no config needed, mirroring `state::find_workflow_issue`'s
   directory walk) and return the issue whose recorded
   `prep.worktree_path` contains the cwd (`worktree::cwd_is_inside`).
   Construct the canonical URL from the matched owner/repo/number. If
   multiple match (shouldn't happen — worktrees are per-issue), take the
   first and warn.
4. Otherwise error: "no issue given and none could be inferred; pass an
   issue number or URL, e.g. `ghwf work-on 13`."

Implementation split for testability: the directory walk lives in `state.rs`
(e.g. `find_issue_for_dir(dir: &Path) -> Result<Option<(String, String, u64)>>`),
and `resolve_issue_arg` takes the env value and inferred fallback as inputs
(or is thin enough that only the walk needs unit tests — avoid `set_var` in
parallel tests).

## 4. Skill + launcher text updates

- `install.rs` `SKILL_CONTENT`: change `argument-hint` to
  `[issue number or URL]` and note that the argument may be omitted — ghwf
  infers the issue from the session environment or current worktree. The
  `$ARGUMENTS` substitutions stay (they expand to nothing when no argument
  is given, which now works).
- `launch.rs` `print_fresh_reminder`: the reminder becomes
  "run `/work-on` to pick up the workflow" — no number needed, since the
  launched session has `GHWF_ISSUE`. (Keep mentioning the number
  parenthetically so the message still works if the user ignores the skill.)
- README: update the `/work-on` examples to mention the no-argument form.

## 5. Tests

- `state`: `find_issue_for_dir` walk — match, no-match, cwd outside any
  recorded worktree (build fake state files in a temp data dir; the walk
  function should take the issues root as a parameter so tests don't touch
  the real data dir).
- `launch`: factor the no-branch persistence decision so it's testable, or
  cover it via the state round-trip (prep state written with
  `no_branch: true`, no worktree fields).
- `install`: existing skill-content tests keep passing with the new text
  (the marker test is content-independent).
- `main`/resolution: precedence — explicit arg beats env, env beats
  inference, clear error when nothing applies (test the pure helper with
  injected env value rather than mutating process env).

## Out of scope

- A separate `work` subcommand (option 2c) — rejected in pre-plan.
- A generic `GHWF_FLAGS` pass-through (option 1c) — speculative until a
  second passable flag exists.
- Auto-running `/work-on` in the launched session — injecting a prompt is
  programmatic use, billed as API traffic.
