# Plan — ghwf posts phase/status updates to the threads (#6)

The phase banner — current phase, what just happened, what to do next — only
appears in Claude's terminal. A user driving the workflow from GitHub gets no
record on the issue of where things stand or what their next approval command
will trigger. This plan has ghwf itself post status comments to the issue (and
the PR, once it exists), so the workflow is legible from GitHub alone.

Design decisions locked on the issue thread:

1. **Post on transitions, not every run** — a status comment is posted on each
   phase transition, plus one initial status the first time `work-on` engages
   an issue, so the thread is legible from the very start. Quiet runs post
   nothing.
2. **Post on misfired directives too** — today a consumed stale/premature/
   retired directive is only explained in the terminal; a user driving from
   GitHub gets silence. ghwf posts the explanation (what was ignored and why,
   and the correct next command) as a status comment. Refinement: a *stale*
   note whose command also fired a transition in the same run is the
   duplicate-across-threads echo — skip it; the transition status already
   tells the story.
3. **Distinct attribution** — status comments get a `**ghwf:**` header (vs
   `**Claude says:**`) and a distinct marker, `<!-- ghwf:v1 status -->`.
   Rationale: digest filtering is per-session-token today, so a status comment
   posted from session A would resurface in session B's digest; status
   comments are pure machinery and must be filtered unconditionally, which
   needs a marker distinguishable from Claude-authored comments. Directive
   scanning skips both marker kinds.
4. **Content** — what just happened (trigger and side effects, e.g. "PR
   flipped to ready-for-review"), the current phase, and explicitly what the
   next approval command is and what posting it will trigger. The terminal
   banner for Claude is unchanged in role; the status comment gets
   user-facing wording.
5. **Terminal phase** — the implement → review status states the PR is ready
   for human review and that no further approval command exists (merging or
   closing the PR concludes the workflow).
6. **Best-effort** — a failed status post warns on stderr but never fails
   `work-on`.

## 1. Markers (`render.rs`)

Generalize the marker machinery to two kinds:

```rust
/// Hidden metadata marker embedded in ghwf-posted comments.
pub enum Marker {
    /// Claude-authored via `create-issue-comment`, tagged with the session.
    Session(String),
    /// A ghwf-authored status update.
    Status,
}
```

- New constant `STATUS_MARKER: &str = "<!-- ghwf:v1 status -->"`; the shared
  scan prefix is `<!-- ghwf:v1 `.
- `extract_marker(body) -> Option<Marker>` recognises both forms;
  `extract_session_token` becomes a thin wrapper (or callers match on
  `Marker::Session` directly — prefer the latter and drop the wrapper).
- `strip_ghwf_marker` matches the shared prefix so both marker kinds strip.
- `build_status_comment_body(text) -> String` assembles
  `**ghwf:**\n<hr>\n\n{text}\n\n<!-- ghwf:v1 status -->` (same `<hr>` trick as
  `build_comment_body`, for the same setext-heading reason).

Call-site updates in `main.rs`:

- `advance_phase` (the directive scan): skip on *any* marker — already the
  intent ("only the user's comments are directives"), now covering status
  comments, whose prose mentions approval commands.
- Digest filtering (both conversation and inline-review loops): skip
  `Marker::Status` unconditionally; skip `Marker::Session(token)` only when it
  matches this session's token, as today.

## 2. Status comment rendering (`render.rs`)

A pure function that builds the status comment text, or `None` when there is
nothing worth posting:

```rust
pub fn render_status_comment(
    phase: Phase,                 // the phase after this run's processing
    transitions: &[Transition],
    notes: &[DirectiveNote],      // already filtered per decision 2
    intro: bool,                  // first-engagement greeting
    pr_url: Option<&str>,         // for phase-state prose (e.g. review)
) -> Option<String>
```

- Returns `Some` iff `intro`, or any transition, or any surviving note.
- Body shape: one line per transition ("Phase advanced: prep-and-plan →
  implement (triggered by `/approve-plan` from jeffatstile)."), one line per
  note (reuse `render_note` — its prose is already neutral third-person),
  then the phase-state paragraph, then the next-step paragraph.
- Per-phase prose (new private helpers, the single user-facing source of
  "what the next approval triggers"):
  - **pre-plan**: in pre-plan, Claude gathers the information needed to write
    a plan and posts its understanding here. Next: comment
    `/approve-pre-plan` (alias `/approve-preplan`) to advance to
    prep-and-plan, where a branch and worktree are created and Claude writes
    an implementation plan, opened as a draft PR.
  - **prep-and-plan**: Claude is writing the implementation plan; ghwf opens
    it as a draft PR. Next: comment `/approve-plan` (on this issue or the PR)
    to advance to implement, where Claude codes the change.
  - **implement**: Claude codes the change in the worktree, pushing to the
    draft PR as it goes. Next: comment `/approve-implementation` (on this
    issue or the PR) to advance to review — the PR flips to
    ready-for-review.
  - **review** (terminal): the work is awaiting human review (naming the PR
    as marked ready when `pr_url` is present). No further approval command;
    merging or closing the PR concludes the workflow.
- The intro variant prefixes a short "ghwf is tracking this issue" line; the
  rest is the same phase-state + next-step prose, so an issue picked up
  mid-flight reads correctly too.

## 3. Posting (`main.rs`)

- `IssueState` gains `#[serde(default)] pub intro_posted: bool`. Existing
  state files deserialize to `false`, so issues already mid-flight get one
  (phase-aware) intro status on their next `work-on` — desirable: their
  threads become legible too.
- Filter notes per decision 2: drop a `Stale` note when a transition with the
  same `command` fired this run.
- Post after the phase body is computed and before `state::save` — side
  effects have happened by then, so the prose states facts (the review flip
  has occurred; the just-created worktree exists), and the `intro_posted`
  flag is persisted by the existing save.
- Re-read `prep.pr_number` after the body (it may have just opened the PR)
  and post the same status body to the issue and, when a PR is recorded, to
  the PR conversation thread via the existing `github::post_issue_comment`
  (PRs share the issues comments endpoint). `--no-branch` issues post to the
  issue only, automatically (no `pr_number`).
- Best-effort wrapper: on `Err`, `eprintln!` a warning naming the thread and
  carry on.
- Banner adjustments in `render_phase_banner`: drop the "Relay the
  ignored-directive notes above to the user" instruction — ghwf now posts
  those itself; add a short line noting a status update was posted, so Claude
  doesn't duplicate it.

The status comment is fetched on the *next* run, where the digest filter
(section 1) hides it and the seen record hashes it like any other comment —
no seen-cache changes needed.

## 4. Tests

- Markers: `extract_marker` round-trips both kinds; session token still
  extracted; `strip_ghwf_marker` strips the status marker;
  `build_status_comment_body` contains header and marker.
- `advance_phase`: a status-marker comment whose body contains a line-start
  approval command is not treated as a directive.
- `render_status_comment`: transition line names command and author;
  premature and retired notes render with the correct next command; stale
  note filtered when its command matches a same-run transition, kept
  otherwise (filtering tested where it lives); intro variant renders for
  every phase; review prose names the PR when present and states no further
  approval exists; returns `None` with nothing to report.
- Banner: relay instruction gone; status-posted line present when expected.

## Build order

1 → 2 → 3, tests alongside each.

## Out of scope / punted

- A status post when the draft PR opens — not a phase transition; GitHub
  already cross-links the PR onto the issue and Claude posts a hand-off
  comment.
- Editing/pinning a single rolling status comment instead of posting new
  ones.
- Posting digests or any Claude-authored content — status comments are
  ghwf-authored machinery only.
