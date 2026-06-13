# Plan — #113: Add `convert` subcommand to convert a non-bare repo into our preferred layout

## Goal

Add `ghwf convert [PATH]` — turn an existing ordinary (non-bare) clone into
ghwf's preferred layout (a container directory holding `<name>.git`,
`ghwf.toml`, and `worktrees/`), keeping the original clone untouched as a
backup so any local-only branches, stashes, and uncommitted work are never
lost.

Per the pre-plan discussion the build strategy is **pristine re-clone**
(Option A): the new bare repo is a fresh `--single-branch` clone, identical to
what `ghwf clone` produces, but with `--reference <old> --dissociate` so it
reuses the old clone's objects (fast, no re-download) while ending up
self-contained. `PATH` defaults to the current directory, so the conversion
must move directories out from under the running process safely.

## Background — how things work today

- `ghwf clone` (`src/clone.rs`) builds the preferred layout. The reusable core
  is `populate(container, name, url, reference) -> Result<Option<String>>`: it
  runs `git::clone_bare(url, container/<name>.git, reference)`
  (`git clone --bare --single-branch`, with `--reference … --dissociate` when a
  reference is given), `git::setup_conventional_remote` (sets the conventional
  fetch refspec, runs `fetch --prune origin`, and `remote set-head origin
  --auto`), writes `ghwf.toml` (`config_text`), creates `worktrees/`, and
  checks out the default branch into `worktrees/<default>` via
  `create_default_worktree`. It returns the default branch name (or `None` if
  only the default-worktree step failed). `populate` is currently module-private
  to `clone.rs`.
- `git::clone_bare` runs git with cwd `"."` but takes an explicit `dest`; as
  long as `dest` (and the `--reference` path) are **absolute**, the process cwd
  is irrelevant to where the clone lands. `setup_conventional_remote`,
  `add_worktree_for_branch`, etc. all run git with cwd set to an explicit repo
  path, so they too are cwd-independent given absolute paths.
- Helpers available in `src/git.rs`: `is_inside_work_tree(dir)` (false for a
  bare repo or non-repo), `toplevel(dir)` (`rev-parse --show-toplevel`),
  `remote_url(repo)` (`git remote get-url origin`), `default_remote_branch`,
  `add_worktree_for_branch`.
- Subcommands are declared in the `Commands` enum in `src/main.rs` and
  dispatched in the big `match` (e.g. `Commands::Clone { … } => clone::run(…)`),
  with `mod clone;` at the top.
- The README documents `ghwf clone` (the "Cloning into the layout" material near
  the top); `convert` belongs alongside it.

## The cwd-safety problem

With `PATH` defaulting to cwd, the conversion renames the user's current
directory. On POSIX, renaming a process's cwd is safe — the cwd is held by
inode, so the process keeps working and absolute paths keep resolving — **as
long as we never rely on cwd-relative paths after the move.** The design below
removes the risk almost entirely by doing all the heavy, fallible work in a
temporary sibling directory *before* any rename, leaving only two quick renames
at the end.

## Design

New module `src/convert.rs` with `pub fn run(path: Option<&Path>) -> Result<()>`.

### 1. Resolve and validate (no mutations yet)

- Start dir = `path` or `"."`. Error early if it's not inside a git repo.
- Require `git::is_inside_work_tree(start)` — otherwise it's a bare repo (or
  already a ghwf container, whose root is not a work tree); bail with a clear
  message ("already bare / not an ordinary clone").
- `let top = git::toplevel(start)?` then **canonicalize** it to an absolute,
  symlink-resolved path. Everything downstream uses absolute paths derived from
  this; the process cwd never matters again.
- Refuse a **linked worktree** (one that belongs to another repo): detect via
  `git rev-parse --git-common-dir` differing from `--git-dir` (or `.git` being a
  file, not a directory). Converting a single worktree of an existing layout is
  not the intent; bail with guidance.
- Derive:
  - `name` = `top.file_name()` (bail if it has no final component, e.g. a
    filesystem root).
  - `parent` = `top.parent()` (bail if none).
  - `backup` = `parent/<name>.pre-ghwf`.
  - `temp` = `parent/<name>.ghwf-converting` (a build scratch dir in the same
    parent, so the final rename into place is a cheap same-filesystem move).
- Require that **neither** `backup` nor `temp` already exists; bail naming the
  offender if so.
- `let url = git::remote_url(&top)?` — bail if there's no `origin`.

### 2. Build the new layout in `temp` (original still fully intact)

- Create `temp` (and remember to clean it up on any failure below).
- Call the shared `clone::populate(&temp, &name, &url, Some(&top))`. This is the
  identical path `ghwf clone` uses, with the old clone as the dissociating
  reference. After it returns, `temp` is a complete, self-contained layout that
  no longer depends on `top`'s objects.
- On **any** error here: `std::fs::remove_dir_all(&temp)` (best-effort) and
  return the error. The original clone is untouched — a failed convert is a
  no-op. (`populate`'s own default-worktree step is already best-effort and only
  warns, matching `clone`.)

### 3. Swap into place (the only cwd-moving step)

Two renames, smallest possible risk window:

1. `std::fs::rename(&top, &backup)` — move the original aside.
2. `std::fs::rename(&temp, &top)` — move the new layout into the original path.

If step 2 fails, roll back step 1 (`rename(&backup, &top)`), best-effort remove
`temp`, and return an error explaining that the original was restored. (Both
renames are within `parent`, i.e. same filesystem, so `EXDEV` is not a concern.)

### 4. Report

Print the resulting layout and next steps (mirroring `clone::report`), and
explicitly call out that the original clone is preserved at
`<name>.pre-ghwf/` — including the reminder that any local-only branches /
stashes / uncommitted changes live there. Next steps: `cd <name>`,
`ghwf config init`, `ghwf work-on <issue>`.

### Refactor to enable reuse

- Make `clone::populate` `pub(crate)` so `convert.rs` can call it. (Its helpers
  `create_default_worktree` / `config_text` stay private; only `populate` is
  exposed.) `create_container`'s empty-dir acceptance isn't needed here since
  `convert` creates `temp` itself, so it stays private to `clone.rs`.
- Add `mod convert;` and the `Commands::Convert { path: Option<PathBuf> }`
  variant + dispatch (`=> convert::run(path.as_deref())`) in `src/main.rs`, with
  a doc comment matching the `Clone` variant's style.

## Tests

In `src/convert.rs`, following the fixture style of `clone.rs`'s tests
(`fixture_origin`, `scratch`, `git_stdout`, etc.):

- **Happy path:** clone a `fixture_origin` into a non-bare working clone, run the
  conversion on it, then assert:
  - the container now sits at the original path with a bare `<name>.git`
    (`rev-parse --is-bare-repository` = true), the conventional remote-tracking
    refs (`refs/remotes/origin/{main,extra,HEAD}`), a loadable `ghwf.toml`, and a
    `worktrees/<default>` checkout associated with the default branch;
  - the backup exists at `<name>.pre-ghwf/`, is still a non-bare repo, and still
    contains the working-tree file(s) — i.e. the original is preserved intact.
- **Local-only work is retained in the backup:** create a local-only branch /
  uncommitted file in the working clone before converting; assert it survives in
  the backup (and, per Option A, the pristine bare repo only has origin
  branches).
- **Rollback on populate failure:** force step 2 to fail (e.g. point at a repo
  whose `origin` URL is unreachable so `setup_conventional_remote`'s fetch
  fails) and assert the original clone is left exactly in place, with no `temp`
  and no `backup` left behind.
- **Precondition errors:** bail when run against a bare repo / non-repo, and when
  `<name>.pre-ghwf` already exists.

Factor the inner mechanics into a testable helper if it keeps the tests from
depending on cwd (the resolution step uses `"."` when no path is given; tests
pass an explicit absolute `PATH` to stay cwd-independent).

## Docs

- README: add a short `ghwf convert` paragraph next to the `ghwf clone`
  material — what it does, that it defaults to cwd, and that the original is
  retained at `<name>.pre-ghwf/`.

No new config fields, so the `CLAUDE.md` "Adding a config option" checklist
doesn't apply.

## Out of scope

- Windows (the rename-the-cwd technique relies on POSIX semantics, consistent
  with the rest of the tool).
- Offline / preserve-all-local-branches conversion (Option B), and carrying
  local branches into the new layout — the backup covers that need.
