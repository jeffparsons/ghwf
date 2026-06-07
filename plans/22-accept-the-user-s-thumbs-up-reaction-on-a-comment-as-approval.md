# Plan — accept a 👍 reaction as approval (#22)

Phase advancement is driven purely by text directives (`/approve-pre-plan`,
`/approve-plan`, `/approve-implementation`) parsed from comment bodies
(`parse_directive` in `state.rs`, consumed by `advance_phase` in `main.rs`).
Reactions are invisible today: the `Comment` model has no reactions field, and
a reaction neither bumps a comment's `updated_at` nor fires any events-feed
event, so neither `work-on` nor `wait` can see one.

Design decisions locked on the issue thread:

1. **Only ghwf-authored prompts are 👍-able** — a 👍 counts only on a
   ghwf-marked comment (status or session marker) whose body mentions an
   approval command. It is equivalent to the user posting that command: same
   stale/premature classification, consumed exactly once (removing the
   reaction later undoes nothing).
2. **Any human user's 👍 counts** — parity with text directives, which have no
   authorization either. Authorization is a separate concern for later.
3. **Bounded polling** — `wait` watches reactions only on the *latest*
   approval-prompting comment per thread. A 👍 on an older prompt is still
   honoured, but only when the next `work-on` runs.

## 1. Mapping a comment to "the approval it prompts for" (`state.rs`)

New `parse_prompted_directive(body) -> Option<Directive>`: the **last**
word-bounded mention of an approval command anywhere in the body (mid-line and
backticked mentions count, unlike `parse_directive`'s line-start rule).
`/proceed` is excluded — only the three approve commands map.

Last-mention-wins is what makes status comments self-describing: a transition
status update mentions the old command first ("triggered by `/approve-X`") and
ends with the next prompt ("Next: comment `/approve-Y` …"); misfire notes end
with "the command that advances it is `/approve-X`". Review-phase status
comments mention no command, so they are not 👍-able. Word boundary: the
character after the match must be end-of-string or non-`[alnum-]` (so
`/approve-plans` doesn't match), and the character before the `/` must be
non-alphanumeric or start-of-line.

Also on `IssueState`: `consumed_reactions: BTreeSet<u64>` (`#[serde(default)]`
for old state files). Reaction ids live in their own id namespace, so they get
their own set rather than sharing `consumed_directives`.

## 2. Models and fetching (`models.rs`, `github.rs`)

- `Comment` gains `#[serde(default)] pub reactions: Option<ReactionRollup>` —
  the summary GitHub embeds in every issue-comment object. `ReactionRollup`
  needs only `total_count` and `#[serde(rename = "+1")] plus_one: u64`. The
  rollup gates detail fetches: no `+1`s, no extra API call. (Test helpers that
  construct `Comment` literals gain the field.)
- New `Reaction { id: u64, user: User, content: String, created_at: String }`.
- New `github::fetch_comment_reactions(owner, repo, comment_id)` →
  `gh api --paginate repos/{owner}/{repo}/issues/comments/{id}/reactions`.
  Issue and PR conversation comments share the issue-comments namespace, so
  one endpoint form serves both threads. (Reactions on *inline review*
  comments are out of scope — ghwf never authors those.)

## 3. Advancing on reactions (`main.rs`)

A new collection step in `work_on`, before `advance_phase`: for each
ghwf-marked comment (status or session marker) in either thread whose body has
a prompted directive and whose rollup shows `+1 > 0`, fetch its reactions.
Result: `Vec<PromptThumbs { comment_id, directive, source, reactions }>`, one
per qualifying comment, passed into `advance_phase` so it stays pure and
testable.

`advance_phase` reworks its scan into a single chronological merge of approval
events from both threads:

- text directives in unmarked user comments (as today, keyed by comment id in
  `consumed_directives`);
- `+1` reactions on prompt comments (keyed by reaction id in
  `consumed_reactions`, ordered by the reaction's `created_at`; non-`+1`
  contents are ignored).

Each unconsumed event is consumed unconditionally, then classified exactly as
today: approves the current phase → transition; earlier → stale note; later →
premature note. The reacting user's login lands in the transition/note `by`
field.

## 4. Reporting (`render.rs`)

- `Transition` and `DirectiveNote` gain `via_reaction: bool`. Rendering keeps
  the canonical command for context: "Phase advanced: pre-plan → prep-and-plan
  (triggered by a 👍 reaction from jeffparsons, equivalent to
  `/approve-pre-plan`)." Notes analogously: "a 👍 reaction (equivalent to
  `/approve-plan`) from jeffparsons (on the issue) was ignored — …".
- `phase_status_prose`: each "Next: comment `/approve-X` …" line gains "— or
  react 👍 to this comment —". (The alias mention stays last where present;
  both spellings map to the same directive, so last-mention-wins is unaffected.)
- Claude guidance prose (`pre_plan_body` in `render.rs`, `prep.rs:156`,
  `implement.rs:81`/`92`): mention that the user may 👍 the prompt comment
  instead of typing the command.

## 5. Waking on reactions (`wait.rs`, `state.rs`)

`WaitState` gains `#[serde(default)] reaction_watches: BTreeMap<String,
ReactionWatch>`, keyed by thread (`"issue"` / `"pr"`), with `ReactionWatch {
comment_id: u64, plus_one_ids: BTreeSet<u64> }` — the baseline of `+1`
reaction ids already seen.

- **`work-on` records the watches**: the latest ghwf-marked prompt comment per
  thread, baseline = its current `+1` ids (from the detail fetch when one
  happened, else empty). When this run posts a status comment that itself
  prompts an approval, that comment (just posted, baseline empty) becomes the
  watch for each thread it was posted to.
- **`create-issue-comment` updates the watch**: Claude's summary comments are
  the likeliest 👍 target and are posted *after* `work-on`. `record_last_posted`
  additionally replaces the destination thread's watch (and drops its stale
  ETag key) when the posted body has a prompted directive.
- **Polling**: each watch becomes an endpoint
  (`repos/{owner}/{repo}/issues/comments/{id}/reactions?per_page=100`, ETag
  key `reactions_issue` / `reactions_pr`; `Endpoint.key` becomes `String`).
  Evaluation of a fresh body: any `+1` reaction id not in the baseline wakes
  with "New 👍 reaction from {login} on the approval prompt." Reaction
  endpoints are polled in **every** cycle, including feed mode — the events
  feed is structurally blind to reactions, so they can't ride the 5-minute
  backstop sweep alone. Cost: one or two conditional GETs per cycle.

## 6. Tests

- `state.rs`: `parse_prompted_directive` — backticked mid-line mention; last
  mention wins; `/proceed` and `/approve-plans` don't match; no-mention →
  `None`; serde back-compat (old state files lack `consumed_reactions` /
  `reaction_watches`).
- `main.rs` (`advance_phase`): a 👍 on a prompt advances and consumes by
  reaction id; re-run is a no-op; 👍 after the equivalent text directive is a
  stale note; premature 👍 is noted, not fired; chronological interleaving of
  a text approval and a 👍 fires both in written order; non-`+1` reactions are
  ignored.
- `render.rs`: via-reaction transition/note wording; status prose mentions the
  👍 option in every non-terminal phase.
- `wait.rs`: reaction-endpoint evaluation — unknown `+1` id wakes, baselined
  id doesn't, non-`+1` content doesn't.

## 7. Docs

README's approval paragraph (lines 8–9, 23) gains the 👍 alternative: reacting
👍 to the ghwf comment that prompts for an approval is equivalent to posting
that command.

Build order: 2 (models/fetch) → 1 (state) → 3 + 4 together (advance/render
link via `via_reaction`) → 5 (wait/watches) → 6, 7 alongside.

## Out of scope / punted

- Authorization (who may approve) — deferred for both text and reaction paths.
- 👎 or other reaction contents — ignored entirely.
- Reactions on inline review comments.
- Treating reaction *removal* as revoking an already-consumed approval.
