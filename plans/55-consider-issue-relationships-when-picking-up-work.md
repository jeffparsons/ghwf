# Plan: Consider issue relationships when picking up work (#55)

## Goal

ghwf should respect GitHub's native **issue dependencies** and **sub-issues**
when deciding what to work on:

1. **Never start a currently-blocked issue.** An issue blocked by another *open*
   issue is excluded from automatic pickup (`ghwf next` / `next --wait`).
2. **Never work a tracking issue directly.** An issue that has sub-issues is a
   "tracking issue"; `next` skips it, and explicitly running
   `ghwf work-on <tracking>` redirects to one of its workable descendant
   issues instead.

Both signals are already present on every entry of the open-issues listing
`next` fetches, so detection for `next` costs **zero extra API calls**.

## Key facts (verified against the live API)

The REST issues listing (`repos/{owner}/{repo}/issues?state=open`) and the
single-issue fetch both carry two summary objects per issue:

```jsonc
"issue_dependencies_summary": { "blocked_by": 1, "total_blocked_by": 1,
                                "blocking": 0, "total_blocking": 0 },
"sub_issues_summary":         { "total": 0, "completed": 0, "percent_completed": 0 }
```

Confirmed empirically (linked two throwaway issues, toggled the blocker's state):

- `issue_dependencies_summary.blocked_by` counts **open** blockers only. When a
  blocker is closed it drops to `0` while `total_blocked_by` stays `1`. So
  **"currently blocked" ⇔ `blocked_by > 0`** (`total_blocked_by` is the wrong
  field — it counts closed blockers too).
- `sub_issues_summary.total > 0` ⇔ the issue **has sub-issues** (tracking issue).

Children are listed via `GET repos/{owner}/{repo}/issues/{n}/sub_issues`, which
returns full issue objects (number, title, state, assignees, labels, and both
summaries) — everything the redirect needs, including each child's own blocked
and tracking status for recursion.

## Decisions (settled in the issue discussion)

1. Recurse into tracking-issue children: descend depth-first to a workable leaf,
   not just direct children.
2. When redirecting, prefer to **resume an already-started descendant** over
   starting a fresh one; only pick a fresh workable leaf when none is started.
   Resolved purely from **local** ghwf state — single machine/user is assumed,
   so no cross-machine locking or liveness check (matching `next`'s claim and
   the launcher's existing resume).
3. No workable descendant → a clear error rather than silent no-op.
4. Scope is **blocked-by + sub-issues** only; "blocking", milestones, and other
   relationship types are not acted on.
5. No new config key — skipping work you can't start has no downside.

## Implementation

### 1. Models (`src/models.rs`)

Add two small summary structs and surface them on `IssueListing`, all
`#[serde(default)]` so repos/older GitHub without the fields (or `gh`
deserialising a sparse sub-issue object) still parse and read as
not-blocked / not-tracking — graceful degradation, no hard dependency on the
preview feature.

```rust
#[derive(Deserialize, Serialize, Default)]
pub struct IssueDependenciesSummary {
    // Count of *open* issues currently blocking this one. Closed blockers are
    // excluded here (they live in `total_blocked_by`), so this is exactly
    // "currently blocked".
    #[serde(default)]
    pub blocked_by: u64,
}

#[derive(Deserialize, Serialize, Default)]
pub struct SubIssuesSummary {
    // Number of sub-issues; > 0 marks a tracking issue.
    #[serde(default)]
    pub total: u64,
}
```

On `IssueListing` add:

```rust
#[serde(default)]
pub state: String,                                   // sub_issues lists closed children too
#[serde(default)]
pub issue_dependencies_summary: IssueDependenciesSummary,
#[serde(default)]
pub sub_issues_summary: SubIssuesSummary,
```

and helpers:

```rust
impl IssueListing {
    /// Blocked by at least one still-open issue.
    pub fn is_blocked(&self) -> bool { self.issue_dependencies_summary.blocked_by > 0 }
    /// Has sub-issues — a tracking issue we shouldn't work directly.
    pub fn is_tracking(&self) -> bool { self.sub_issues_summary.total > 0 }
    /// Open, per the listing's `state` (defaults to open for the repo-wide
    /// `?state=open` listing, where the field may be elided in tests).
    pub fn is_open(&self) -> bool { self.state.is_empty() || self.state == "open" }
}
```

The repo-wide listing always returns open issues, so `state` is only load-bearing
when filtering sub-issue children; treating empty as open keeps existing tests
and the repo-wide path unaffected.

### 2. `next` selection (`src/next.rs`)

Extend `Selection` with two more reported lists:

```rust
struct Selection<'a> {
    picked: Option<&'a IssueListing>,
    skipped_started: Vec<u64>,
    skipped_blocked: Vec<u64>,
    skipped_tracking: Vec<u64>,
}
```

In `select`'s loop, after the PR / assigned-to-other exclusions (which stay
silent), add the new skips with precedence **started → blocked → tracking** (an
issue qualifying for several is reported once, in that order):

```rust
if already_started(issue.number) { skipped_started.push(...); continue; }
if issue.is_blocked()           { skipped_blocked.push(...); continue; }
if issue.is_tracking()          { skipped_tracking.push(...); continue; }
candidates.push(issue);
```

Blockedness/trackingness come straight off the `IssueListing`, so `select`'s
signature is unchanged (no new predicate). `announce_pick` prints the new lists
alongside the existing started line:

```
Skipping #N — blocked by an open issue.
Skipping #N — a tracking issue (has sub-issues); resume a sub-issue with `ghwf work-on <sub>`.
```

Factor the sort key out for reuse by the redirect (below):

```rust
fn sort_key(issue: &IssueListing, me: &str, priority_labels: &[String]) -> (bool, usize, u64) {
    (!assigned_to(issue, me), label_rank(issue, priority_labels), issue.number)
}
```

and have `select`'s `sort_by_key` call it.

### 3. Tracking → child redirect (`src/next.rs` + `src/launch.rs`)

New public entry in `next.rs`, called by the launcher:

```rust
/// Resolve the issue a launch should actually work. For a normal issue this is
/// the number itself; for a tracking issue it is a workable descendant chosen
/// by the same ordering `select` uses, preferring an already-started one.
///
/// Best-effort: any API failure (offline, feature absent) warns and returns
/// `number` unchanged, preserving the launcher's offline path.
pub fn resolve_workable(owner: &str, repo: &str, number: u64) -> Result<u64>
```

Algorithm:

1. **Collect descendant leaves via DFS.** Starting at `number`, list children
   with `github::list_sub_issues`. A node with no (open) children is a *leaf*
   → its `IssueListing` is a candidate. A node with children is a tracking node
   → recurse into its open children. Guard with a `visited: BTreeSet<u64>` so a
   malformed cycle can't loop forever. One `sub_issues` call per tracking node.
   - The top-level non-tracking case short-circuits: `sub_issues` returns `[]`,
     so the only "leaf" is `number` itself → return it immediately (after the
     one cheap call; see best-effort note).
2. **Prefer resume.** Among collected leaves with existing local ghwf state
   (`state::load_if_exists(..).is_some()`, the same test as `started_fn`), pick
   the best by `sort_key` and return it. Started leaves are eligible even if now
   blocked — work is already underway.
3. **Else pick fresh.** Among the remaining leaves, drop blocked ones
   (`is_blocked()`), pick the best by `sort_key`, return it.
4. **Else error**, e.g. *"#55 is a tracking issue but none of its sub-issues are
   workable (all are blocked, closed, or already complete)."*

`me` and `priority_labels` are resolved exactly as `pick` does
(`github::authenticated_user`, `config::find`).

Add a thin github helper:

```rust
/// The sub-issues (children) of an issue, as listing entries.
pub fn list_sub_issues(owner: &str, repo: &str, number: u64) -> Result<Vec<IssueListing>>
```

— `gh api --paginate repos/{owner}/{repo}/issues/{number}/sub_issues`, parsed
into `Vec<IssueListing>` (the objects carry every field the helpers read).

**Wire into `launch::run`** (`src/launch.rs`), right after
`resolve_issue_ref` yields `(owner, repo, number)`:

```rust
let number = next::resolve_workable(&owner, &repo, number)?;
```

with a one-line notice when it differs from the requested number
(*"Issue #55 is a tracking issue; working sub-issue #60 instead."*). Everything
downstream — state load, worktree, `$GHWF_ISSUE` — then uses the resolved child,
so the launched session is anchored to the real work and the in-session
`work-on` loop needs no change. Resuming a started child reuses the launcher's
existing worktree/session-resume path unchanged.

Only the launcher needs this: `next` already excludes tracking issues, and an
in-session bare `work-on` resolves to the worktree's concrete leaf, never a
tracking issue.

### 4. Tests

`next.rs`:
- Extend the `issue()` test builder (or add a builder) to set `blocked_by`,
  `sub_issues_summary.total`, and `state`.
- `select`: blocked issue skipped + reported; tracking issue skipped + reported;
  closed-only blockers (`blocked_by == 0`, `total_blocked_by > 0` modelled as
  `blocked_by: 0`) stay pickable; missing summaries default to eligible;
  precedence started > blocked > tracking.
- `resolve_workable`: inject the `sub_issues` lookup and `already_started`
  predicate (mirror `claim_pick`'s injected-closure test style) to cover —
  non-tracking returns the number unchanged; ordering picks the right child;
  recursion descends through a tracking child to a leaf; a started descendant is
  preferred over a fresh one; blocked fresh leaves are skipped; no workable
  descendant errors.

`models.rs`: `is_blocked` / `is_tracking` / `is_open` truth tables, including the
default (all-absent) case.

### 5. Docs

- `README.md` "Picking an issue automatically": note that blocked and tracking
  issues are skipped, and that `work-on <tracking>` redirects to a workable
  sub-issue.
- Doc comments on `pick` / `wait_for_pick` / `resolve_workable` and the eligible
  paragraph in `next.rs`.
- No `ghwf.toml` / `init` / config changes (no new key).

## Out of scope / non-goals

- "blocking" (the inverse direction), milestone, or project relationships.
- A blocked *tracking* issue does not propagate its block to its children; each
  child is judged on its own blocked state (we never work the tracking issue
  itself anyway).
- Cross-machine coordination — local state is the source of truth by design.
