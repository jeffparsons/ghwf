# Plan — `collect-garbage` subcommand (#14)

`ghwf collect-garbage` (alias `gc`) deletes branches — local and remote —
whose PRs have already been merged, as long as the branch tip is exactly
what got merged into the main branch, and removes their worktrees when the
working tree is clean. Anything suspicious (extra commits since the merge,
working-tree changes) is warned about and left alone. Nothing is ever
force-deleted.

Decisions locked on the issue thread:

1. **Remote branches are included** — `origin/<branch>` is deleted alongside
   the local branch, and a branch that exists only on the remote is still a
   candidate.
2. **Scope is any branch with a merged PR**, not just `issue_N_*` branches
   ghwf created. The safety checks make this safe.
3. **State files are collected too** — when a branch is fully collected, the
   per-issue ghwf state file that recorded it is removed.
4. **No prompt, plus `--dry-run`** — it acts immediately and reports
   everything deleted/kept/warned; `--dry-run` previews without touching
   anything.
5. **Fetch first** — `git fetch --prune origin` before judging anything, so
   merge state, remote tips, and the remote-tracking ref list are current.

## 1. The safety condition: "tip is what got merged"

For each candidate branch, ask GitHub for every PR whose head is that branch
(`gh pr list --head <branch> --state all --json
number,state,headRefOid,mergeCommit,baseRefName`). Then:

- **Any open PR → skip silently.** The branch is active, not garbage.
- **No merged PR → skip silently.** Never-PRed and closed-without-merge
  branches are out of scope (the issue only covers merged PRs).
- **Merged PR found** (latest by number if several): the PR's `headRefOid`
  is the head commit GitHub merged, frozen at merge time. The branch
  qualifies for deletion only when:
  1. its tip (local and/or remote, checked independently) equals
     `headRefOid` — nothing was added or rewritten since the merge; and
  2. the PR's `mergeCommit` is an ancestor of (or equal to)
     `origin/<default>` — the merge actually landed on the main branch.

Comparing tips against `headRefOid` (rather than ancestry of the tip itself)
makes the check exact for merge commits and *also* correct for squash/rebase
merges, where the merged content is on main but the head commit isn't. So
squash-merged branches collect cleanly rather than falling to the warn side.

Warnings (branch kept) when a merged PR exists but:

- local tip ≠ `headRefOid` → "local branch has different/extra content than
  what was merged";
- remote tip ≠ `headRefOid` → same, for `origin/<branch>` (local and remote
  are judged independently: a matching local is deleted even when a diverged
  remote is kept, and vice versa — whichever side diverged still holds the
  extra content);
- `mergeCommit` is not an ancestor of `origin/<default>` → "PR merged, but
  the merge is not on <default>" (e.g. merged into some other base).

## 2. Candidate discovery

Run from the main repo (`config::find()` → `main_repo_path()`; without a
config, the cwd's repo) with `(owner, repo)` from the existing
`github::repo_or_cwd()`. Candidates are the union of:

- local branches (`git for-each-ref refs/heads`), and
- `origin/*` remote-tracking branches (`git for-each-ref
  refs/remotes/origin`, skipping `origin/HEAD`),

minus the default branch (`github::default_branch`). One `gh pr list` call
per candidate; stale branches are few, so N+1 is fine (a bulk
`gh pr list --state merged` index was considered and rejected — its `--limit`
caps silently miss older PRs).

## 3. Worktree removal

For a branch whose local side qualifies, look up its worktree
(`git::branch_worktree`):

- **No worktree** → delete the local branch directly.
- **Worktree is the main worktree** (path == main repo path) or **contains
  the cwd** → warn and keep both worktree and local branch; never pull the
  rug out from under the running command or the main checkout.
- **Worktree has tracked changes** (`is_tree_clean` false) or **untracked
  files** (a new stricter check; `git worktree remove` refuses on either, and
  both count as "working tree changes" worth a warning) → warn, keep
  worktree and local branch.
- **Clean** → `git worktree remove` (no `--force`), then delete the local
  branch. A refusal from git is downgraded to a warning, never a hard error.

The default-branch exclusion in §2 already guarantees the default branch's
worktree is never considered.

## 4. Deletion order and state cleanup

Per collected branch: worktree → local branch (`git branch -D`; safe because
the tip-equality check already proved the content is merged) → remote branch
(`git push origin --delete`). Each step only runs when its side qualified.

When nothing of a branch remains (no local, no remote, no worktree), find
the issue state file whose `prep.branch` matches (scan
`~/.local/share/ghwf/issues/<owner>/<repo>/*.json`) and delete it. A branch
that was only partially collected keeps its state file.

## 5. CLI surface (`main.rs`)

```rust
/// Delete branches and worktrees for PRs that have already been merged.
///
/// A branch is collected only when its tip is exactly what got merged into
/// the main branch; its worktree only when the working tree is clean.
/// Anything suspicious is warned about and left alone.
#[command(alias = "gc")]
CollectGarbage {
    /// Report what would be deleted without deleting anything.
    #[arg(long)]
    dry_run: bool,
},
```

Dispatch: `Commands::CollectGarbage { dry_run } => collect_garbage::run(dry_run)`,
in a new `collect_garbage` module.

## 6. The `collect_garbage` module

```text
run(dry_run):
    repo      = config main repo, or cwd
    (o, r)    = github::repo_or_cwd()
    default   = github::default_branch(o, r)
    git::fetch(repo)                       // now with --prune
    for branch in candidates(repo, default):
        prs    = github::branch_prs(o, r, branch)
        facts  = gather(repo, branch)      // local tip, remote tip, worktree + status
        plan   = classify(branch, facts, prs, merge_landed)
        report/execute plan
    summary ("nothing to collect" when no actions or warnings)
```

The core is a pure `classify` function — all git/GitHub facts gathered first
and passed in as plain data, so the decision table is unit-testable without a
network or filesystem:

```rust
struct BranchFacts {
    local_tip: Option<String>,
    remote_tip: Option<String>,
    worktree: Option<WorktreeFacts>,   // path + clean/dirty/untracked + main/cwd flags
}

fn classify(facts: &BranchFacts, prs: &[BranchPr], merge_landed: bool) -> Verdict
```

returning the actions to take (remove worktree / delete local / delete
remote / delete state) and the warnings to print. `run` executes the actions
(or prints "would …" under `--dry-run` — dry-run still does all the
read-only gathering, so its output is the real verdict).

Output is line-per-action ("deleted local branch x", "removed worktree …",
"warning: …"), warnings to stderr, actions to stdout.

## 7. New plumbing

`git.rs`:

- `fetch` grows `--prune` (harmless for its existing prep caller, and gc
  needs pruned remote-tracking refs so already-deleted remote branches don't
  show up as candidates that fail to delete);
- `list_local_branches(repo) -> Result<Vec<String>>`;
- `list_remote_branches(repo) -> Result<Vec<String>>` (names with `origin/`
  stripped, `HEAD` skipped);
- `rev_parse_ok(repo, rev) -> Option<String>` (tip lookup that may miss);
- `is_ancestor(repo, ancestor, descendant) -> bool` (`git merge-base
  --is-ancestor`; a failed probe reads as "no", which fails safe — the
  branch is warned about, not deleted);
- `has_untracked_files(dir) -> Result<bool>` (`git status --porcelain`
  filtered to `??` lines, complementing `is_tree_clean`);
- `delete_local_branch(repo, branch)` (`git branch -D`);
- `delete_remote_branch(repo, branch)` (`git push origin --delete`);
- `remove_worktree(repo, path)` (`git worktree remove`).

`github.rs` / `models.rs`:

- `branch_prs(owner, repo, branch) -> Result<Vec<BranchPr>>` via
  `gh pr list --head <branch> --state all --json
  number,state,headRefOid,mergeCommit,baseRefName`, with

  ```rust
  pub struct BranchPr {
      pub number: u64,
      pub state: String,                 // "OPEN" | "CLOSED" | "MERGED"
      pub head_ref_oid: String,
      pub merge_commit: Option<Oid>,     // { oid } or null
  }
  ```

`state.rs`:

- `delete(owner, repo, number)` removing the state file, and a scan helper
  to find the issue number whose `prep.branch` matches a given branch
  (shaped like the existing `find_workflow_issue` directory walk).

## 8. README

Document `collect-garbage`/`gc` under the subcommand list: what it deletes,
the exact-tip safety rule, the clean-worktree rule, warnings for everything
suspicious, and `--dry-run`.

## 9. Tests

`collect_garbage.rs`, against `classify` (pure decision table):

- merged PR, local and remote tips == `headRefOid`, clean worktree → remove
  worktree, delete local + remote, delete state;
- local tip diverged → warn, keep local (and worktree); matching remote
  still deleted;
- remote tip diverged → warn, keep remote; matching local still deleted;
- open PR present (even alongside an older merged one) → skip, no actions;
- only closed-unmerged PRs → skip;
- no PRs at all → skip;
- merge commit not on default branch → warn, nothing deleted;
- squash-merge shape (tip == `headRefOid`, merge commit on main, tip *not*
  an ancestor of main) → still collected;
- dirty worktree → warn, keep worktree and local branch; untracked-only →
  same with the untracked wording;
- worktree is the main worktree / contains cwd → warn, keep;
- remote-only branch matching `headRefOid` → delete remote (and state, when
  no local/worktree existed);
- partial collection (remote kept) → state file kept.

`git.rs`, with real scratch repos (existing `tests::scratch`/`init_repo`
pattern):

- `list_local_branches` / `list_remote_branches` round-trip;
- `is_ancestor` true/false/bogus-rev;
- `delete_local_branch` removes the ref; refuses nothing it shouldn't;
- `remove_worktree` removes a clean worktree; a dirty one errors (which gc
  downgrades to a warning).

Build order: §7 plumbing, then §6 + §5, then §8 + §9 alongside.

## Out of scope / punted

- Closed-but-unmerged PR branches (the issue covers merged PRs only).
- Pruning the seen-cache (per-session digest records); only issue state
  files are collected.
- A bulk merged-PR index to avoid N+1 `gh pr list` calls (rejected: silent
  `--limit` truncation).
- Interactive confirmation or per-branch include/exclude flags.
- `gc` for repos whose remote is not `origin`.
