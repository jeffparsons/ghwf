# Plan: phase + attention status labels for issues and PRs (#28)

## Summary

Two orthogonal axes of workflow state, both persisted in `IssueState` and
mirrored as GitHub labels on the issue and its PR:

- **Phase** (existing): `pre-plan → prep-and-plan → implement → review`.
- **Attention** (new): `waiting-on-user` | `waiting-on-claude` |
  `waiting-on-ghwf` — purely *who* the workflow is waiting for.

Alongside the new axis, three behavioural changes that fall out of it:

1. A new `ghwf hand-off` subcommand posts Claude's hand-off comment with the
   phase-appropriate next-step prompt appended by ghwf, and flips attention to
   `waiting-on-user`. Phase banners instruct Claude to use it instead of
   hand-rolling approval prompts.
2. Status comments posted on phase entry stop prompting approvals. The prompt
   only ever appears on a hand-off comment, so the user is never asked to
   approve something that doesn't exist yet.
3. `/approve-implementation` is retired. The implement → review transition
   fires when the user marks the draft PR ready for review (the GitHub
   button); ghwf detects the draft→ready flip and no longer flips the PR
   itself.

Labels are configured per-repo in `ghwf.toml` (feature off when the section is
absent), bootstrapped by a new `ghwf config labels` subcommand.

## 1. The attention axis (`state.rs`)

- New enum, alongside `Phase`:

  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
  #[serde(rename_all = "kebab-case")]
  pub enum Attention {
      WaitingOnUser,
      #[default]
      WaitingOnClaude,
      WaitingOnGhwf,
  }
  ```

  Default is `WaitingOnClaude`: a `work-on` run always leaves Claude with
  instructions. Add a `label()` helper mirroring `Phase::label()`.

- `IssueState` gains `#[serde(default)] pub attention: Attention`.

- Attention transitions:
  - **→ `WaitingOnClaude`**: set by `work-on` when any phase transition fired
    or the digest found new activity (the same condition that resets
    `stop_nudges`, `main.rs:483`). Unchanged otherwise — re-running `work-on`
    with nothing new must not steal the ball back from the user.
  - **→ `WaitingOnUser`**: set by `ghwf hand-off` (§4), and by `work-on`
    whenever the phase is `Review` (nothing is needed from Claude there).
  - **→ `WaitingOnGhwf`**: set transiently by the prep-and-plan machinery
    (§6) around worktree/PR creation.
  - On a concluded PR (`pr_outcome.is_some()`): attention is moot; the label
    sync removes the attention label (§5) but the field keeps its last value.

## 2. Retire `/approve-implementation` (`state.rs`, `main.rs`, `render.rs`)

- `Directive::ApproveImplementation::approves()` returns `None` (like
  `Proceed`). The spelling stays in `DIRECTIVE_COMMANDS` so stray uses are
  explained, not ignored.
- `parse_prompted_directive` / `collect_prompt_thumbs` skip: replace the
  `directive == Directive::Proceed` check with `directive.approves().is_none()`
  — retired commands never map a 👍.
- `Phase::approval_command()`: `Implement` returns `None`. Add a
  `Phase::advance_hint()` (or similar) returning the "what advances it" prose
  for misfire notes: the approval command where one exists, "marking the draft
  PR ready for review advances it" for `Implement`, "there is nothing further
  to approve" for `Review`.
- `render_note`'s `Retired` arm gets per-command prose: `/proceed` keeps its
  current text; `/approve-implementation` says it is retired and that marking
  the draft PR ready for review is what advances the implement phase.

## 3. Implement → review fires on the draft→ready flip

- `models::PullRequest` gains `#[serde(default)] pub draft: bool`.
- **`work-on` (`main.rs`)**: the existing `fetch_pr` call (~line 207) already
  retrieves the PR each run; keep the parsed object. After `advance_phase`,
  if `phase == Implement`, the PR is open, and `!pr.draft`, advance to
  `Review` and record a transition.
  - `render::Transition` grows a trigger distinction: replace
    `command/by/via_reaction` with a small enum (e.g.
    `Trigger::Directive { command, by, via_reaction }` and
    `Trigger::PrReady`), rendered as
    "Phase advanced: implement → review (the PR was marked ready for
    review)." in both the banner and the status comment.
- **`implement.rs`**: `review()` no longer calls `mark_pr_ready` — the PR is
  already ready by the time the phase is entered. Drop the `pr_ready` flag
  from `PrepState` (serde ignores unknown fields, so old state files keep
  loading) and delete `github::mark_pr_ready`. `review_body` prose: the PR
  *is* ready for review (not "has been marked").
- **`wait.rs`**: wake on the flip so the loop turns it around promptly.
  - `WaitState` gains `#[serde(default)] pub pr_draft: Option<bool>`,
    recorded by `work-on` from the fetched PR.
  - `EndpointKind::PrObject`: in addition to the conclusion wake, wake when
    `pr.draft != baseline` ("The PR was marked ready for review." /
    "The PR was converted back to draft.").
  - Feed mode: `PullRequestEvent` with action `ready_for_review` (and
    `converted_to_draft`) wakes too, alongside the existing `closed`.
- A ready→draft conversion does *not* move the phase back to implement in
  this change; `work-on` just reports it. (Possible follow-up.)

## 4. New `ghwf hand-off` subcommand (`main.rs`, new prose in `render.rs`)

`ghwf hand-off [ISSUE]`, body from stdin (mirrors `create-issue-comment`).

- Loads state; errors in the `Review` phase and on a concluded PR (nothing to
  hand off).
- Builds the comment: Claude's body, then a ghwf-appended prompt paragraph
  for the current phase (single source in `render.rs`):
  - `PrePlan`: comment `/approve-pre-plan` (alias `/approve-preplan`) — or
    react 👍 to this comment — to advance to prep-and-plan.
  - `PrepAndPlan`: comment `/approve-plan` — or react 👍 — to advance to
    implement.
  - `Implement`: when you're happy with the change, mark the draft PR ready
    for review (the "Ready for review" button) to advance to review. No 👍
    equivalence — the button is the signal.
- Tagged with the session marker (like `create-issue-comment`), so the
  existing machinery treats it as a 👍 target automatically: `extract_marker`
  + `parse_prompted_directive` already drive `collect_prompt_thumbs` and
  `latest_prompt_watch`, and `record_last_posted` already installs the
  reaction watch.
- Posting mirrors the status-comment thread routing (`status_primary_is_pr`):
  full comment on the phase's primary thread; when a PR exists, a one-line
  ghwf-status stub linking to it on the other thread.
- Flips `attention = WaitingOnUser`, saves state, syncs labels (§5).
- Banner prose updated to use it:
  - `render::pre_plan_body`: post questions with `create-issue-comment`; post
    the final summary with `ghwf hand-off 28` (no longer "ends by prompting
    the user to comment /approve-pre-plan").
  - `prep::complete_body`: hand off with `ghwf hand-off <n>` (replaces the
    "post a hand-off comment … /approve-plan" instruction).
  - `implement::branch_body` / `no_branch_body`: when the work is complete,
    hand off with `ghwf hand-off <n>` (replaces the `/approve-implementation`
    instruction).
- The `install.rs` skill text gains a line mentioning `ghwf hand-off` for
  hand-off comments (users re-run `ghwf install` to pick it up).

## 5. Status comments stop prompting; labels config and sync

### Status prose (`render.rs`)

`phase_status_prose` drops every "Next: comment `/approve-X`" paragraph.
Each phase instead describes what is happening and how the next advance will
arrive, e.g.:

- pre-plan: Claude gathers information and will post a hand-off comment
  prompting your approval when it's ready to plan.
- prep-and-plan: Claude is writing the plan (opened as a draft PR); it will
  hand off for your approval when the plan is ready.
- implement: Claude codes the change, pushing to the draft PR; when it hands
  off, review the PR and mark it ready for review to advance.
- review: unchanged apart from no longer claiming ghwf flipped the PR.

Status comments are therefore never 👍 targets; the
`status_prose_offers_reaction_in_every_approvable_phase` test inverts into
"status prose never mentions an approval command".

### Config (`config.rs`)

```toml
[labels.phase]
pre-plan = "ghwf:pre-plan"
prep-and-plan = "ghwf:planning"
implement = "ghwf:implementing"
review = "ghwf:review"

[labels.attention]
waiting-on-user = "ghwf:needs-you"
waiting-on-claude = "ghwf:claude-working"
waiting-on-ghwf = "ghwf:preparing"
```

`Config` gains `pub labels: Option<LabelsConfig>`; `LabelsConfig` holds two
structs (`phase`, `attention`) with one required `String` per variant
(kebab-case field renames). All-or-nothing per section keeps the sync simple;
TOML errors name any missing key. Add lookup helpers
`LabelsConfig::for_phase(Phase) -> &str` / `for_attention(Attention) -> &str`.

### Label sync (new `labels.rs`)

- `pub fn sync(cfg, owner, repo, issue_number, pr_number, phase, attention: Option<Attention>)`:
  - Desired set: the phase's label, plus the attention label
    (`None` once concluded — the phase label alone remains).
  - For the issue and (when present) the PR: fetch current labels, remove any
    *configured* ghwf label not in the desired set, add missing desired ones.
    Only names appearing in the config are ever touched — user labels
    (e.g. `priority_labels`) are invisible to the sync.
  - REST via the existing `gh api` plumbing: `GET/POST
    repos/{o}/{r}/issues/{n}/labels`, `DELETE …/labels/{name}` (PRs are
    issues to this endpoint).
  - Entirely best-effort: every failure is a stderr warning; labels are
    decoration derived from state, never the source of truth.
- Skip-if-unchanged: `IssueState` records the last synced
  `(phase, attention, pr_number)` (small serde struct, e.g.
  `labels_synced: Option<LabelSyncRecord>`); `sync` callers compare first so
  the steady-state `work-on`/`wait` loop adds zero API calls. The
  `pr_number` member makes the sync re-run when the draft PR first appears.
- Call sites: end of `work-on` (after state save), `hand-off`, and the
  transient prep flip (§6). All gated on `labels` being configured.

### `ghwf config labels` (new subcommand)

- CLI: `Commands::Config { command: ConfigCommands }` with
  `ConfigCommands::Labels` — the first member of the planned `ghwf config`
  family.
- Requires an existing `ghwf.toml` (`config::require`). Errors if a `[labels]`
  section is already present (edit it by hand instead).
- Creates the seven labels above in the repo via `POST /repos/{o}/{r}/labels`
  with default colours (phases in a blue ramp; `needs-you` orange-red
  `d93f0b`, `claude-working` green `0e8a16`, `preparing` grey `bfbfbf`),
  treating an already-exists 422 as success.
- Appends the `[labels.*]` section (with the default names) to `ghwf.toml`
  and prints what it did. Users can rename afterwards by editing the file and
  the repo's labels together.

## 6. `waiting-on-ghwf` during prep (`prep.rs`, `main.rs`)

Before the prep-and-plan machinery does slow work (worktree creation, plan
commit/push, draft-PR opening), `work-on` sets
`attention = WaitingOnGhwf` and syncs labels; the end-of-run attention logic
(§1) then settles it (typically `WaitingOnClaude` with the "write the plan"
banner). This is the only transient sync; it is skipped when labels aren't
configured, and skipping the *flip* when prep has nothing left to do (state
already has branch + PR) keeps the steady loop quiet.

## 7. Tests

- `state.rs`: `Attention` serde round-trip and default; old state files
  (no `attention`, with `pr_ready`) still load;
  `ApproveImplementation.approves() == None`.
- `render.rs`: status prose never contains an approval-command mention
  (`parse_prompted_directive` is `None` for every phase); hand-off prompt
  builder maps 👍 to the right directive in pre-plan/prep-and-plan and to no
  directive in implement; `Trigger::PrReady` transition prose; retired
  `/approve-implementation` note prose.
- `main.rs`: draft→ready flip advances implement → review exactly once and
  not from other phases / when concluded; `/approve-implementation` comment
  yields a retired note; attention settles per the §1 rules.
- `wait.rs`: `pr_draft` baseline change wakes (both directions); feed
  `ready_for_review` action wakes; existing conclusion wakes unchanged.
- `labels.rs`: desired-set computation (concluded drops attention), and that
  only configured names are removed.
- `config.rs`: labels section parses; absent section → `None`; missing key
  errors.
- `prep.rs`/`implement.rs`: banner bodies name `ghwf hand-off` and no longer
  name `/approve-plan`//`approve-implementation`.

## Out of scope (noted for follow-ups)

- Converting the PR back to draft does not (yet) move review → implement.
- Labelling issues ghwf has never been run on ("not started" = no labels).
- A `ghwf config` wizard beyond the `labels` subcommand.
