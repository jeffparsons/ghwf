# Plan — Automatically detect and resolve merge conflicts (#45)

The goal: while a ghwf PR is open, notice when its branch has started
conflicting with the moved-on base (`origin/<default>`) and have Claude
resolve it — without adding expensive GitHub polling.

## Why this is feasible without extra polling (the issue's question)

A merge conflict arises from *base-branch* movement (usually another PR
merging), not from any activity on our own PR. The `wait` loop is structurally
blind to it: it watches conditional GETs keyed to our issue/PR plus the events
feed filtered to our issue/PR number, and base movement bumps none of those.

GitHub's native signal — the PR object's `mergeable` / `mergeable_state` — is a
poor fit for the cheap loop: it's computed lazily (the first GET after a base
change returns `null` and only *kicks off* the computation), and the recompute
doesn't reliably bump the PR's ETag/`updated_at`, so the 304-conditional
machinery can mask it. Reliable detection through it means unconditional PR
fetches on a cadence.

So we detect **locally** instead. ghwf already owns the bare repo + worktree.
`git merge-tree --write-tree <branch> origin/<default>` (git ≥2.38; the project
runs 2.52) performs an in-memory trial merge and reports conflicts without
touching the working tree or index — **zero GitHub API calls**; the only
network cost is a `git fetch` of the base ref, which is cheap.

## Decisions locked on the issue thread

1. **Trigger scope — wake points only.** Detection runs on each `ghwf work-on`
   run. No new dedicated polling and no events-feed changes. (Scope note in §6.)
2. **Resolve strategy — merge.** Claude runs `git merge origin/<default>` into
   the branch (no force-push; review-comment anchors preserved), not rebase.
3. **Auto-resolve.** When a conflict is detected, the banner *leads* with an
   instruction to resolve it now, rather than merely surfacing it.
4. **Conflicts only.** A merely-behind (non-conflicting) branch is left alone.

**Division of labour:** ghwf detects the conflict and instructs; Claude does
the `git merge`, resolves the textual conflicts in the worktree, commits, and
pushes. ghwf never edits conflicted hunks itself.

No new config key (detection is always on; strategy is fixed), so `config.rs`,
`init.rs`, and the README `ghwf.toml` example are untouched.

## 1. Git plumbing (`src/git.rs`)

Two read-only helpers.

```rust
/// The default branch name as the worktree's remote knows it, derived from the
/// `origin/HEAD` symref (e.g. "main"). Local-only — no network, no GitHub API.
pub fn default_remote_branch(dir: &Path) -> Result<String>
```

Implemented with `git -C <dir> rev-parse --abbrev-ref origin/HEAD`, stripping
the `origin/` prefix. `origin/HEAD` is set up at clone time
(`setup_conventional_remote` runs `remote set-head origin --auto`), so it's
present in every ghwf-managed clone.

```rust
/// Whether merging `base` into the commit at `dir`'s HEAD would conflict.
/// A read-only trial merge: writes a tree object but never touches the index
/// or working tree.
pub fn would_conflict(dir: &Path, base: &str) -> Result<bool>
```

Mechanics (and the subtlety that earns this its own helper rather than
`git_ok`): `git merge-tree --write-tree HEAD <base>` exits **0** on a clean
merge and **1** on conflicts — but it *also* exits 1 when a rev doesn't
resolve (printing nothing to stdout). So:

- First verify both revs resolve (`rev_parse_ok` for `HEAD` and `base`);
  bail otherwise.
- Run merge-tree. Exit 0 → `Ok(false)`. Exit 1 **with non-empty stdout** (the
  conflicted tree OID + entries) → `Ok(true)`. Any other exit, or exit 1 with
  empty stdout → `bail!` with stderr (a real error, surfaced to the caller as
  a best-effort skip, see §3).

Unit tests drive real git in a scratch repo (the file already has the
`scratch`/`run_git`/`init_repo` harness): a clean fast-forward base, a
genuinely conflicting base, and a non-resolving base ref.

## 2. Conflict check orchestration (`src/implement.rs` or a small new fn)

A single entry point called from `work_on`:

```rust
/// Detect whether the open PR's branch conflicts with the freshly-fetched
/// base. Best-effort: any failure (offline, missing worktree, git error) logs
/// a warning and returns `None` — conflict detection must never break a
/// `work-on` run. Returns `Some(base_branch_name)` when a conflict exists.
pub fn detect_conflict(prep: &PrepState) -> Option<String>
```

Steps, all gated so it only runs when meaningful:

1. `prep` is branch-mode, has a `worktree_path` that exists on disk, and a
   `pr_number` (an open PR — the caller only invokes this when
   `pr_outcome.is_none()`).
2. `git::fetch(worktree)` to refresh `origin/<default>` (best-effort).
3. `let base = git::default_remote_branch(worktree)?;`
4. `git::would_conflict(worktree, &format!("origin/{base}"))?` → on `true`,
   return `Some(base)`.

Errors at any step → `eprintln!("warning: …")` and `None`.

## 3. Wiring into `work_on` (`src/main.rs`)

After the phase body is built and `issue_state.pr_outcome` is known, compute
the conflict once for the phases where an open-PR branch can conflict:

```rust
let conflict = match (issue_state.pr_outcome, issue_state.phase) {
    (None, Phase::Implement | Phase::Review) => issue_state
        .prep
        .as_ref()
        .and_then(implement::detect_conflict),
    _ => None,
};
```

Detection runs in both **implement** and **review** (both have an open PR +
worktree, and a conflict blocks merge in either). It is skipped for a concluded
workflow, `--no-branch`, and the pre-plan/prep-and-plan phases. It runs once per
`work-on`, just before the banner is printed.

The notice is prepended to the phase `body` so it leads the banner (which
already concatenates `Phase:` line, transitions, notes, then `body`). Render it
via a small helper in `render.rs`:

```rust
/// The conflict-resolution banner block, shown above the phase body when the
/// branch conflicts with the base.
pub fn conflict_notice(base: &str, number: u64) -> String
```

Text (auto-resolve framing, merge strategy fixed):

```
⚠️ Merge conflict with `origin/<base>`

The base branch has moved on and your branch now conflicts with it. Resolve
this before other work:

1. In the worktree, run `git merge origin/<base>`.
2. Resolve the conflicted files, then `git add` them and commit the merge.
3. Push the branch (the PR updates automatically).

Then carry on below.
```

Applied as `body = format!("{}\n\n{}", conflict_notice(&base, number), body)`
when `conflict` is `Some`.

This also counts as activity for the Stop hook / attention axis so the session
stays engaged to do the resolve: fold `conflict.is_some()` into the existing
`activity` boolean in `work_on` (the same flag that resets `stop_nudges` and
sets `WaitingOnClaude`). A standing conflict on an otherwise-quiet re-run thus
keeps the ball with Claude until it's resolved.

## 4. Banner-only, not a status comment

The conflict notice goes to the **banner** (Claude's instructions), not to a
posted GitHub status comment: it's an instruction to act, not a workflow
transition, and re-posting it on every quiet `work-on` while the conflict
stands would spam the thread. Once Claude merges and pushes, the next
`work-on`'s check returns `None` and the notice disappears on its own.

## 5. Tests

- `git.rs`: `would_conflict` and `default_remote_branch` against real scratch
  repos (clean / conflicting / bad-ref cases).
- `render.rs`: `conflict_notice` contains the base name, the `git merge
  origin/<base>` command, and the issue number.
- `implement.rs`: `detect_conflict` returns `None` for `--no-branch` and for a
  prep with no worktree on disk (the gating logic), without needing a live
  PR.

## 6. Scope note — idle PRs (flag at plan approval)

Per decision 1 (wake-points-only, the explicitly *simplest* option) the `wait`
loop is left unchanged: it gains no conflict-detection cycle and no new wake
reason. Consequence: a conflict introduced into an **idle** PR (e.g. a
review-phase PR waiting on a human while another PR merges) is surfaced only on
the next `work-on` — which fires when any other activity (a comment, reaction,
push, or draft/state change) wakes the loop. In the steady state of an active
session this is immediate; for a long-idle review PR it waits for the next
event.

If you'd rather an idle `wait` wake promptly on a fresh conflict, that's the
"also wake wait on conflict" option from the discussion: add the same local
`detect_conflict` check to `wait`'s periodic direct sweep (bounded to roughly
the existing 5-minute backstop cadence, not every cycle) and emit a wake
reason. It's a clean follow-up and doesn't change anything above. Called out
here so it can be folded in now if preferred.
