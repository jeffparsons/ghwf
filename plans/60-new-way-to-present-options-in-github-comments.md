# Plan — present options as GitHub checkboxes (#60)

When Claude has a discrete menu of options for the user, it currently has only
the prose path: `hand-off --question` posts free text and the user types a
reply. #60 adds a structured path — ghwf renders the options as a GitHub task
list (clickable checkboxes), the user ticks their choices plus a final
**Submit** box, and only the submit tick wakes Claude. ghwf owns the
formatting, the submit-gate, and the parse/map-back so Claude can't get the
mechanics wrong.

This reuses the machinery built for 👍-as-approval (#22): a per-thread *watch*
recorded in `WaitState`, a dedicated poll endpoint that the events feed is
blind to, and a recompute-from-comments-each-run discipline in `work-on`. The
options watch is the checkbox analogue of `ReactionWatch`.

Design decisions locked on the issue thread:

1. **ghwf owns the formatting.** New `ghwf ask` subcommand: Claude supplies the
   question (stdin) and option labels (`--option` flags). ghwf assigns each a
   hidden id, renders the task list, auto-appends the submit box, and posts it.
   Claude never hand-writes checkbox markdown.
2. **Multi-select only.** No radio enforcement. A self-contradictory tick set
   is surfaced as-is; Claude notices and starts a conversation rather than ghwf
   rejecting it.
3. **Submit-gated, one-shot.** `wait` wakes only when the submit box flips
   unticked → ticked; ticking an individual option does nothing. On wake,
   `work-on` rewrites the submit line in place to `_Answers submitted at …_`,
   so it reads as done and can't re-fire.
4. **Free-text escape hatch stays open.** A plain prose reply is an ordinary
   user comment — it wakes Claude through the existing conversation path with
   no new code. It signals *responded, not resolved*. Claude is also guided to
   include an "other / none of these" option where it fits; ticking it is
   surfaced like any selection and Claude treats the question as
   responded-but-not-resolved.

## 1. Rendering and parsing the options comment (`render.rs`)

The option/submit markers deliberately do **not** use the `<!-- ghwf:v1 … -->`
family. `MARKER_SCAN_PREFIX` (`<!-- ghwf:v1 `) drives `strip_ghwf_marker`,
which truncates a body at its first occurrence; an inline `ghwf:v1` marker on
every option line would corrupt that. So:

```rust
const OPT_MARKER_PREFIX: &str = "<!-- ghwf-opt:";
const SUBMIT_MARKER: &str = "<!-- ghwf-submit -->";
// MARKER_SUFFIX (" -->") already exists.
```

The trailing session marker (`build_comment_body`) is still the only `ghwf:v1`
marker in the body, so `extract_marker`, `hidden_from_digest`, and
`strip_ghwf_marker` keep working unchanged.

**`build_options_body(question: &str, options: &[String]) -> String`** — the
inner body (before `build_comment_body` wraps it with the "Claude says" header
and session marker):

```
<question>

- [ ] <label-1> <!-- ghwf-opt:opt1 -->
- [ ] <label-2> <!-- ghwf-opt:opt2 -->
…

- [ ] **Submit my answers** <!-- ghwf-submit -->
```

Ids are `opt1`, `opt2`, … in flag order. The hidden id anchors each line as a
ghwf-managed option (distinct from any `- [ ]` a label itself contains) and
lets Claude reference an option unambiguously.

**`parse_options_comment(body: &str) -> ParsedOptions`** where

```rust
pub struct ParsedOptions {
    pub options: Vec<ParsedOption>,   // id, label, checked
    pub submit: Option<bool>,         // None = no submit marker (already consumed)
}
pub struct ParsedOption { pub id: String, pub label: String, pub checked: bool }
```

A line is an option iff it matches `- [ ]`/`- [x]`/`- [X]` (leading whitespace
tolerated) **and** contains `OPT_MARKER_PREFIX`; the id is the text between the
prefix and `MARKER_SUFFIX`, the label is the text between `]␠` and the marker,
trimmed. The submit line is the checkbox line containing `SUBMIT_MARKER`.

**`rewrite_submitted_body(body: &str, when: &str, selected: &[String]) ->
String`** — replaces the submit checkbox line with a plain (non-checkbox) line
`_Answers submitted at {when}._`, leaving option lines (and their ticks)
intact and the session marker in place. Dropping the `- [ ]` means the box can
no longer be toggled, and `parse_options_comment` then returns `submit: None`,
which is what makes the rewrite the consumption record (§4).

## 2. State: the options watch (`state.rs`)

`WaitState` gains, mirroring `reaction_watches`:

```rust
// Thread key (`issue` / `pr`) -> the outstanding options comment whose submit
// box `wait` polls. Only an un-submitted comment is watched; the latest per
// thread wins.
#[serde(default)]
pub options_watches: BTreeMap<String, OptionsWatch>,
```

```rust
/// A ghwf-posted options comment whose submit box `wait` polls. Recorded only
/// while its submit box is unticked, so any ticked submit on it wakes.
#[derive(Clone, Serialize, Deserialize)]
pub struct OptionsWatch {
    pub comment_id: u64,
}
```

No `consumed` set and no stored id→label map: the comment body is
self-describing (re-parsed each run), and the §1 rewrite removes the submit
marker, so a submitted comment is never re-watched or re-surfaced.

## 3. Waking on submit (`wait.rs`)

- `EndpointKind::Options(&'static str)` (thread key), alongside `Reactions`.
- `options_endpoints(owner, repo, &wait.options_watches)` mirrors
  `reaction_endpoints`: one endpoint per watch, key `options_issue` /
  `options_pr`, URL `repos/{owner}/{repo}/issues/comments/{id}` (the single
  comment object — the `?since=` comment lists can't be relied on to include a
  ghwf-authored, edit-only change). `poll_endpoints` extends with it.
- `evaluate_fresh` for `Options(thread)`: parse the `Comment`, run
  `parse_options_comment`; if `submit == Some(true)` push a wake reason
  `"Answers submitted to your options question ({noun})."`. `Some(false)` /
  `None` → nothing. Ticking an option (not submit) only changes non-submit
  lines, so no wake.
- Like reaction endpoints, options endpoints are polled in **every** cycle
  including feed mode — a checkbox edit on ghwf's own comment is exactly the
  kind of activity the events feed doesn't surface to us.

## 4. Consuming a submission and arming the watch (`main.rs`, `work_on`)

After the conversation comments are fetched (`issue_comments`, `pr_comments`)
and before `wait_state` is finalised, run a per-thread pass over ghwf-authored
options comments (identified by `SUBMIT_MARKER` presence; own-token check via
the existing marker helpers):

- **Submitted** (`parse_options_comment(...).submit == Some(true)`): collect an
  `OptionSubmission { thread_noun, url, selected: Vec<String>, unselected:
  Vec<String> }`, then `github::update_issue_comment(owner, repo, id,
  &render::rewrite_submitted_body(&body, &comment.updated_at, &selected))`
  (best-effort; `comment.updated_at` is the submit-tick time, no new time
  dependency). After the rewrite the comment has no submit marker, so it is
  neither re-surfaced nor re-watched on later runs.
- **Outstanding** (`submit == Some(false)`): record the latest such per thread
  into `wait_state.options_watches`, dropping the stale `options_{thread}`
  ETag — the analogue of the `latest_prompt_watch` arming for reactions.

`work-on` already rebuilds `wait_state` from scratch each run, so this
recompute-from-comments approach needs no cross-run bookkeeping (same shape as
the reaction watches at main.rs:692–708).

Thread the submissions into the digest:

- Extend the `activity` OR-chain with `|| !submissions.is_empty()` so a
  submission flips attention to `WaitingOnClaude` and resets the stop-nudge
  counter.
- `render_work_on` gains a `submissions: &[OptionSubmission]` parameter and an
  **Answers submitted** section: the question's URL, the selected labels, and
  the unselected ones, plus a one-line nudge that a contradictory set or an
  "other/none" pick means responded-but-not-resolved — keep the conversation
  going. ghwf does the parse/map-back; Claude reads labels.

The free-text escape hatch needs no code here: a prose reply is a non-hidden
user comment already surfaced by `collect_new_comments` and woken by the
`Conversation` endpoint.

## 5. The `ghwf ask` subcommand (`main.rs`, `github.rs`)

`Commands::Ask { issue: Option<String>, option: Vec<String> }` — `--option`
repeatable, question body from stdin. `fn ask(issue, options)` mirrors
`hand_off` minus the approval prompt / reaction watch, plus the options watch:

1. Resolve issue + `owner`/`repo`, load `IssueState`, read the question from
   stdin (error if empty; error if `options` is empty).
2. Pick the primary thread with the existing `status_primary_is_pr(phase)` +
   `pr_number` logic (issue thread in pre-plan; the PR once it exists).
3. `body = render::build_comment_body(&render::build_options_body(&question,
   &options), token.as_deref())`; post to the primary thread. If a PR exists,
   post the same hidden stub to the secondary thread as `hand_off` does
   ("Question posted on the {thread}: {url}").
4. `attention = WaitingOnUser`; set `last_posted`; insert the posted comment
   into `wait.options_watches[thread]` and drop the stale `options_{thread}`
   ETag (so the immediately-following `ghwf wait` polls it) — the direct
   analogue of `hand_off` arming the reaction watch at main.rs:1492–1505.
5. `labels::sync` (drives the `waiting-on-user` label), `state::save`, print the
   comment JSON.

`github.rs` gains `update_issue_comment(owner, repo, comment_id, body)`:
`PATCH repos/{owner}/{repo}/issues/comments/{id}` with the body as JSON on
stdin, mirroring `update_pr`.

## 6. Guidance and docs (`render.rs`, `install.rs`, `README.md`)

- `question_instruction` (shared across banners): add that for a **discrete set
  of choices**, Claude can use `ghwf ask {number} --option "…" --option "…"`
  (question on stdin) to present checkboxes; ghwf appends the submit box and
  only wakes once it's ticked. Keep `--question` as the prose path. Mention the
  optional "other / none of these" option for a not-yet-resolved answer.
- `pre_plan_body` and the prep/implement banners that mention `--question` gain
  the same one-liner where natural.
- `install.rs` `SKILL_CONTENT`: document `ghwf ask` next to `hand-off
  --question`; the `--question`-mentions test gains an `ask` assertion.
- `README.md`: the question paragraph that introduces `hand-off --question`
  gains the `ghwf ask` checkbox alternative (formatting, hidden ids, submit
  gate, one-shot rewrite, prose escape hatch). No `Config` key is added, so the
  config-wizard / `ghwf.toml` note in `CLAUDE.md` doesn't apply.

## 7. Tests

- `render.rs`: `build_options_body` shape (ids in order, submit appended);
  round-trip `parse_options_comment` (mixed ticked/unticked, leading
  whitespace, `[x]`/`[X]`, label with its own `- [ ]`, the trailing session
  marker ignored); `submit: None` once the marker is gone;
  `rewrite_submitted_body` drops the checkbox, keeps option ticks + session
  marker; `strip_ghwf_marker`/`extract_marker` unaffected by option markers.
- `state.rs`: serde back-compat — an old `WaitState` without `options_watches`
  loads.
- `wait.rs`: `Options` evaluation — ticked submit wakes, unticked doesn't, a
  ticked option without submit doesn't, a consumed (marker-removed) body
  doesn't.
- `main.rs`: the submitted/outstanding pass — a submitted comment yields one
  `OptionSubmission` with the right selected/unselected split and arms no
  watch; an outstanding comment arms the latest watch and yields no submission;
  re-run after rewrite is a no-op.
- `install.rs`: `SKILL_CONTENT` mentions `ghwf ask`.

Build order: 1 (render render/parse/rewrite) → 2 (state) → 5 (`ask` +
`update_issue_comment`, so options comments exist to test against) → 3 (wait) →
4 (work-on consume/arm/digest) → 6, 7 alongside.

## Out of scope / punted

- Single-select / radio semantics — multi-select only, by decision.
- Enforcing or validating the tick set (e.g. rejecting contradictions) — left
  to Claude's judgement in conversation.
- Editing options after posting, or multiple concurrent outstanding questions
  per thread beyond "latest wins" for the watch (all *submitted* ones are still
  surfaced via the rewrite-idempotent pass).
- Reactions/👎 on options comments; treating an un-tick after submit as a
  revocation (one-shot, by decision).
