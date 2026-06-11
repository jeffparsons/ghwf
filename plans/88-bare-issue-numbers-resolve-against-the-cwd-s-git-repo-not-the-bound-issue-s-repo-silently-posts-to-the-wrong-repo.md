# Plan: Bare issue numbers must resolve against the bound issue's repo

## Problem

In a ghwf session bound to an issue in repo A (`$GHWF_ISSUE` / worktree
state), passing a **bare number** to a subcommand while the cwd is a checkout
of repo B resolves the number against repo B's git remote, not repo A. When a
same-valued object exists in repo B, the command silently targets the wrong
repo (exit 0; the only tell is the returned `html_url`).

### Root cause

- `resolve_issue_arg` (`src/main.rs:385`) returns an explicit argument
  verbatim and only consults the bound issue (`infer_issue_arg`,
  `src/main.rs:405`) when *no* argument is given. So a bare number passed
  explicitly never picks up the bound issue's repo.
- With no `ghwf.toml` in effect, `issue_endpoint` (`src/github.rs:591`) emits
  `repos/{owner}/{repo}/issues/N` with literal `{owner}`/`{repo}` placeholders,
  which `gh` then fills from the **cwd's** remote.
- The no-argument/inferred form works because `infer_issue_arg` returns a
  fully-qualified `https://github.com/{owner}/{repo}/issues/N` URL, which
  bypasses the placeholder path.

Compounding it: the phase-banner guidance (`src/render.rs` and peers) hardcodes
the bare number (e.g. `ghwf create-issue-comment 15829`), which is exactly the
failing form in a cross-repo session.

## Approach

Two changes, primary + defensive, plus one genuine correctness fix in the
relaunch hint:

1. **Core fix — anchor bare numbers to the bound issue's repo.** Make a bare
   explicit number resolve against the bound issue's repo, never the cwd remote.
2. **Banner sweep (proposal #3) — stop printing bare numbers to the agent.**
   Switch agent-facing command *invocations* in the phase banners / loop
   guidance to the no-argument inferred form.
3. **Relaunch hint — use the canonical URL.** The "exit and relaunch" message
   runs *outside* a bound session, so a bare number there is itself the hazard;
   print the full issue URL.

## Changes

### 1. Core fix: `resolve_issue_arg` (`src/main.rs:385`)

When the explicit argument parses as a bare `u64` **and** a bound issue can be
inferred, rewrite it to that issue's repo before returning:

```rust
fn resolve_issue_arg(arg: Option<String>) -> Result<String> {
    if let Some(arg) = arg {
        // A bare number is ambiguous: left as-is it resolves against the cwd's
        // git remote, which silently targets the wrong repo when the session is
        // bound to an issue in a different repo. Anchor it to the bound issue's
        // repo instead. An explicit URL is left untouched, so it still overrides.
        if arg.parse::<u64>().is_ok() {
            if let Some(bound) = infer_issue_arg()? {
                if let Ok((owner, repo)) = github::parse_owner_repo(&bound) {
                    return Ok(format!(
                        "https://github.com/{owner}/{repo}/issues/{arg}"
                    ));
                }
            }
        }
        return Ok(arg);
    }
    if let Some(inferred) = infer_issue_arg()? {
        return Ok(inferred);
    }
    bail!(/* unchanged */);
}
```

Properties:

- `infer_issue_arg` already returns a URL (from `$GHWF_ISSUE` or worktree
  state); `github::parse_owner_repo` (`src/github.rs:205`, already `pub`) reads
  `owner`/`repo` from it.
- **Best-effort:** if no bound issue is found, or the bound value can't be
  parsed, fall through and return the bare number unchanged — preserving
  today's launcher/no-session behaviour (config repo, then cwd remote).
- Qualifying to a URL routes through the existing URL path in `issue_endpoint`,
  which keeps the `ghwf.toml` / `issue_repos` validation intact (a bound foreign
  issue repo that's properly in `issue_repos` is allowed; the no-config case
  skips the check and returns the right endpoint).
- An explicit *URL* argument is unaffected and still overrides.
- Cost: a bare number in a bound session now goes through `fetch_issue`
  (network) instead of the `resolve_issue_ref` bare+config short-circuit. In a
  bound session we're online and usually fetching anyway — negligible.

This also covers the PR commands (`show-pr`, `update-pr`, `pr-checks`,
`reply-review-comment`): they take the *issue/workflow* number and go through
`resolve_pr` → `resolve_issue_ref`, so anchoring the bare number to the bound
issue's repo is correct for them too.

### 2. Banner sweep: no-argument form (proposal #3)

Switch agent-facing command **invocations** from `ghwf <cmd> {number}` to the
inferred no-argument form (`ghwf wait`, `ghwf work-on`, `ghwf hand-off`,
`ghwf hand-off --question`, `ghwf ask --option …`, `ghwf create-issue-comment`).
These banners are only ever read inside the bound session, where `$GHWF_ISSUE`
makes inference reliable. Leave **descriptive prose** that mentions
`issue #{number}` as-is.

Sites (command literals only):

- `src/render.rs` — `question_instruction` (240–242), `wait_instruction`
  (258–261), `concluded_body` (275–283), `pre_plan_body` (293–302).
- `src/stop_hook.rs:86–88` — the Stop-hook wait/work-on loop guidance.
- `src/implement.rs:155,169,181` — implement-phase hand-off / work-on guidance.
- `src/prep.rs:204,217` — prep-phase work-on / hand-off guidance.
- `src/wait.rs:222` — the timeout "run `ghwf wait` again" line. (Line 36 is an
  error hint, see below.)
- Bound-session error hints in `src/main.rs`: `worktree_path` (433),
  `resolve_pr` (1430), `hand_off` (1723), `ask` (1867) — "run `ghwf work-on`
  first." `implement.rs:200` likewise.

Once the `{number}` interpolation is dropped from a function's command strings,
some helpers may no longer need the `number` parameter; keep it where prose
still uses it, drop it where it becomes unused (let the compiler guide this).

### 3. Relaunch hint: full URL (`src/worktree.rs:45` `relaunch_message`)

`relaunch_message` is printed when the session is in the wrong cwd and must be
relaunched from a fresh shell — *outside* any bound session, so inference won't
help and a bare number is the cross-repo hazard. Change the emitted command to
the canonical issue URL:

```
    ghwf work-on https://github.com/{owner}/{repo}/issues/{number}
```

This requires threading `owner`/`repo` (or a prebuilt URL) into
`relaunch_message`; the caller has them. Update the corresponding test in
`src/worktree.rs` (mod `tests`).

## Out of scope (candidate follow-ups)

- **PR-vs-issue guard (proposal #2, second half).** GitHub's `/issues/N`
  endpoint also returns PRs, so a bare number that is a PR in the *bound* repo
  would still load. The core fix already resolves the reported repro
  (`work-on 15829` now hits repo A, not repo B's PR); the residual case is
  narrow. Offer to file as a follow-up issue.
- **`src/next.rs:336`** tracking-issue hint — this is `ghwf next` selection
  output, not bound-session loop guidance; the bare number there refers to the
  repo being listed. Left as-is.
- **Bare PR-thread number → PR command in a foreign-`issue_repos` session.**
  The PR lives in the code repo, not the issue repo; the inferred form sidesteps
  it. Rare; left as-is.

## Tests

- **Unit test** for the new `resolve_issue_arg` qualification. The cleanest
  seam is to factor the qualification into a small pure helper, e.g.
  `qualify_bare_number(arg: &str, bound: Option<&str>) -> Option<String>`, and
  test: bare number + bound URL → bound repo's issue URL; bare number + no bound
  → `None` (passthrough); explicit URL → not rewritten; non-numeric → passthrough.
- **Adjust** any banner string/snapshot assertions touched by the sweep.
- Update the `relaunch_message` test for the URL form.
- `cargo test` and `cargo clippy` clean.

## Verification

- `cargo test` / `cargo clippy`.
- Manual: in a worktree of repo B with `$GHWF_ISSUE` pointing at repo A,
  `ghwf create-issue-comment <N>` posts to repo A (`html_url` confirms), and
  `ghwf work-on <N>` loads repo A's issue.
