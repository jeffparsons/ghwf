# Plan — `ghwf next` subcommand (#15)

`ghwf next` picks the most important eligible open issue and then proceeds
exactly as if the user had run `ghwf work-on <picked>`: launcher mode outside
a Claude session (worktree + exec Claude), full phase advancement inside one.
It accepts `--no-branch`, passed straight through to the work-on behaviour.

Decisions locked on the issue thread:

1. **Proceeds like `work-on`** — selection is a front-end to the existing
   work-on path, not a separate "print the number" tool.
2. **Issues assigned to other people are excluded** entirely (not treated as
   unassigned).
3. **Already-started issues are skipped.** We can't cheaply and reliably tell
   whether another Claude session is actively working an issue (candidate
   checks considered: scanning for live `claude` processes anchored in the
   worktree, transcript-mtime heuristics, lock files held across `wait`
   round-trips — all racy, platform-dependent, or both), so per the issue
   discussion `next` only picks issues with no recorded ghwf state. Started
   issues are resumed the existing way: `ghwf work-on <n>`.
4. **`priority_labels` lives in `ghwf.toml`**, an optional ordered list;
   earlier = higher priority.
5. **"The user" is the authenticated `gh` user** (`gh api user`).

## 1. Selection rules

Candidate pool: all open issues of the target repo, excluding

- PRs (the REST issues listing includes them; recognised by the
  `pull_request` key),
- issues with assignees that don't include the user,
- issues that already have a ghwf state file (`state::load_if_exists`
  returns `Some`) — i.e. some session has already run `work-on` on them.

Sort key, ascending — first candidate wins:

1. `assigned_to_me` — issues whose assignees include the user before
   unassigned ones.
2. `label_rank` — the smallest index into `priority_labels` of any of the
   issue's labels; issues with no priority label rank after all labelled
   ones. Applies within both the assigned and unassigned groups.
3. `number` — earliest first.

No eligible issue → a clear "nothing to pick" error naming the filters that
emptied the pool (exit 1).

## 2. CLI surface (`main.rs`)

New subcommand:

```rust
/// Pick the next issue to work on and start work on it, as `work-on` would.
Next {
    /// Work without a dedicated branch/worktree/PR (just write the plan file).
    #[arg(long)]
    no_branch: bool,
}
```

Dispatch: `Commands::Next { no_branch } => next::run(no_branch)`. The new
`next` module selects an issue, prints the pick and a one-line rationale
(assigned to you / priority label `x` / earliest open issue), then calls the
same `work_on(&number.to_string(), no_branch)` entry point `WorkOn` uses —
`work_on` moves from a private fn in `main.rs` to `pub(crate)` (or the
selection returns the number for `main.rs` to dispatch; pick whichever reads
better at implementation time).

## 3. Config (`config.rs`)

New optional field, defaulting to empty:

```rust
/// Labels that mark an issue as urgent, most urgent first.
#[serde(default)]
pub priority_labels: Vec<String>,
```

`next` uses `config::find()` (not `require()`): a config is not strictly
needed for selection. With no config there are simply no priority labels,
and the repo falls back as described in §4. (In branch mode the subsequent
work-on path still hard-requires `worktrees_dir`, exactly as today.)

## 4. GitHub plumbing (`github.rs`, `models.rs`)

- `models.rs`: a listing-shaped issue struct (the existing `Issue` lacks the
  fields and carries ones the listing doesn't need):

  ```rust
  pub struct Label { pub name: String }
  pub struct IssueListing {
      pub number: u64,
      pub title: String,
      #[serde(default)] pub assignees: Vec<User>,
      #[serde(default)] pub labels: Vec<Label>,
      // Present (any value) when the entry is a PR.
      #[serde(default)] pub pull_request: Option<serde_json::Value>,
  }
  ```

- `github::list_open_issues(config_repo: Option<&RepoRef>) -> Result<Vec<IssueListing>>`:
  `gh api --paginate --slurp repos/{owner}/{repo}/issues?state=open&per_page=100`,
  flattening the array-of-pages `--slurp` produces. With no config repo, use
  `gh`'s `{owner}`/`{repo}` placeholders to resolve against the current
  directory's repo, mirroring `issue_endpoint`.
- `github::authenticated_user() -> Result<String>`: `gh api user`, parse
  `.login`.
- Skipping started issues needs `(owner, repo)` for `state::load_if_exists`.
  With a config that's `config_repo()`; without one, parse the cwd repo's
  `origin` URL via the existing `git::remote_url` + `parse_remote_url`
  (exposed as needed).

## 5. The `next` module (`next.rs`)

```text
run(no_branch):
    repo_ctx = github::config_repo()?          // also yields priority_labels via config::find()
    me       = github::authenticated_user()?
    issues   = github::list_open_issues(...)?
    pick     = select(issues, &me, &priority_labels, already_started)?
    print pick + rationale (and a note per skipped already-started issue)
    work_on(&pick.to_string(), no_branch)
```

The core is a pure function for testability — state lookups injected as a
predicate so tests don't touch the filesystem:

```rust
fn select(
    issues: &[IssueListing],
    me: &str,
    priority_labels: &[String],
    already_started: impl Fn(u64) -> bool,
) -> Selection
```

returning the winner plus the already-started issue numbers it skipped, so
`run` can report them ("skipping #12 — already started; resume with
`ghwf work-on 12`").

## 6. README

Document `ghwf next` (selection rules in priority order, `--no-branch`,
"already-started issues are skipped — resume those with `work-on`") and the
new `priority_labels` key in the `ghwf.toml` example.

## 7. Tests

`next.rs`, against `select`:

- assigned-to-me beats an unassigned issue with the top priority label;
- label order: earlier label in the list beats later; any priority label
  beats none; an issue's best (smallest-index) label is what counts;
- number tiebreak within equal groups;
- issues assigned to someone else are excluded, including when I am not but
  a higher-priority label would have won;
- co-assigned (me + someone else) counts as assigned to me;
- PR entries are excluded;
- already-started issues are skipped and reported;
- empty `priority_labels` degrades to assigned-then-number;
- empty pool → the "nothing to pick" outcome.

`config.rs`: `priority_labels` parses, and its absence defaults to empty
(existing configs keep loading).

Build order: 3 + 4 (plumbing), then 5 + 2, then 6 + 7.

## Out of scope / punted

- Detecting live Claude sessions on started issues (see decision 3).
- Cross-machine "already started" detection (e.g. probing the remote for the
  issue's deterministic branch name); local state files are the only signal.
- Filtering by milestone, issue type, or other fields beyond labels.
- A `--dry-run`/list mode showing the full ranking.
