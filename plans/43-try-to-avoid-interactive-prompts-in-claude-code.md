# Plan — Avoid interactive prompts in Claude Code (#43)

The goal: stop Claude from popping interactive `AskUserQuestion` (or
prose-and-stop) prompts mid-workflow. They can't be answered asynchronously
or from a phone, which defeats the GitHub-driven point of ghwf. Instead,
Claude should post the question to the issue/PR thread and `ghwf wait`, and
the "ball is in your court" signal — the `needs-you` (`waiting-on-user`)
label — should flip while it waits.

Design decisions locked on the issue thread:

1. **Scope is the clarifying-question popups**, not tool-permission prompts
   (those are already handled by `permission_mode = "auto"`) and not plan
   mode (the skill already forbids it).
2. **Guidance goes into the skill *and* the phase banners** — "never use
   `AskUserQuestion`; post the question and `ghwf wait`."
3. **Generalise `hand-off` to within-phase iterations.** A blocking question
   is a hand-off too — the ball goes to the user — but it must *not* advance
   the phase. So `hand-off` grows a mode that posts the body, flips attention
   to `waiting-on-user`, but omits the advance/approval prompt and does not
   arm the 👍-advances-the-phase reaction watch. The existing end-of-phase
   hand-off is unchanged.
4. **`create-issue-comment` stays the non-blocking post** (FYI/status, no
   label change); question-mode hand-off is the blocking "I need an answer"
   post that flips `needs-you`. Split is informational vs. blocking.

Flag name: **`--question`** on `hand-off` (see "Decision: flag name" below —
this is the one spot worth a quick confirm, but `--question` is the default).

---

## 1. Question-mode `hand-off` (`src/main.rs`)

### Command shape

Add a flag to the `HandOff` variant:

```rust
/// Post Claude's hand-off comment, reading the body from stdin, and flip
/// the workflow to waiting-on-user.
HandOff {
    issue: Option<String>,
    /// Post a blocking question rather than an end-of-phase hand-off:
    /// flips the workflow to waiting-on-user (so the needs-you label is
    /// applied) but stays in the current phase — no advance prompt, and a
    /// 👍 will not advance the workflow. Use this instead of an
    /// interactive prompt whenever you need an answer from the user.
    #[arg(long)]
    question: bool,
},
```

Thread it through the dispatch in `main()`:

```rust
Commands::HandOff { issue, question } => hand_off(&resolve_issue_arg(issue)?, question),
```

### `hand_off(issue, question)` changes (main.rs:1016)

The current body always fetches `render::hand_off_prompt(phase, no_branch)`,
appends it, and (when the resulting body mentions an approval command) arms a
reaction watch. The question path skips the prompt and the watch:

- **Body composition.** When `question` is true, post the user body as-is
  (still wrapped by `build_comment_body` for the "Claude says" header + session
  marker), with *no* appended prompt:

  ```rust
  let full_body = if question {
      render::build_comment_body(user_body.trim(), token.as_deref())
  } else {
      render::build_comment_body(
          &format!("{}\n\n{prompt}", user_body.trim()),
          token.as_deref(),
      )
  };
  ```

- **The `hand_off_prompt` lookup / review-phase guard.** Today, a `None`
  prompt (review phase) hard-errors with "nothing to hand off." In question
  mode we still want to allow asking during review (e.g. Claude has a question
  about review feedback), so only require a prompt in the non-question path:

  ```rust
  let prompt = render::hand_off_prompt(phase, no_branch);
  if !question && prompt.is_none() {
      bail!("the workflow for issue #{number} is in the review phase — …");
  }
  ```

  In question mode `prompt` is unused (the body is posted as-is).

- **Reaction watch.** Already gated on
  `parse_prompted_directive(&full_body).is_some()`. Since question mode
  appends no prompt, that is naturally `None` and no watch arms — but make it
  explicit by guarding the whole block on `!question` too, so a question body
  that *happens* to contain a backticked `/approve-X` can never arm an
  advance watch.

- **Attention + labels.** Unchanged: both paths set
  `attention = WaitingOnUser`, record `last_posted`, call `labels::sync`, and
  save. This is the fix for the label not flipping — posting a blocking
  question now applies `needs-you`.

- **Primary/secondary thread + stub.** Unchanged: question-mode posts to the
  phase's primary thread with the cross-thread stub exactly as the normal
  hand-off does.

### Why not a separate `ghwf ask`

The issue thread explicitly asked to generalise `hand-off` rather than add a
new verb: "ball is in your court" is the shared signal, and overloading the
one command keeps the mental model small. Phase advancement is the only thing
that differs, and that is exactly what `--question` suppresses.

---

## 2. Skill guidance (`src/install.rs`)

`SKILL_CONTENT` is compared byte-for-byte on `ghwf install` (`skill_action`),
so editing it is the whole update mechanism — existing installs pick the new
text up on the next `ghwf install`. (No version number to bump; the doc
comment's "install-version bump" framing from the issue was imprecise.)

Add a bullet to the skill body, near the existing plan-mode / question bullets:

> - Never ask the user with an interactive prompt (no `AskUserQuestion`, and
>   don't ask in prose and stop). If you need an answer to proceed, post the
>   question to the thread with `ghwf hand-off $ARGUMENTS --question` (body
>   from stdin) — that flips the issue to "needs you" — then `ghwf wait
>   $ARGUMENTS` for the reply. If a question is genuinely informational (no
>   answer needed to continue), use `ghwf create-issue-comment $ARGUMENTS`
>   instead.

Keep the wording aligned with the existing "Never enter Claude Code plan
mode" bullet so the two read as a pair.

---

## 3. Phase-banner reinforcement

The skill is generic; the per-phase banners are where Claude actually reads
"here's what to do now," so repeat the rule there. Each gets one sentence:
*"If you need an answer from the user, post it with `ghwf hand-off <n>
--question` and `ghwf wait` — never an interactive prompt."*

- **`src/render.rs` `pre_plan_body`** — already steers questions onto the
  thread via `create-issue-comment`; add the explicit "never an interactive
  prompt" clause and mention `--question` for *blocking* questions (vs.
  `create-issue-comment` for discussion). Pre-plan is the one phase where
  free discussion via `create-issue-comment` is the norm, so keep both.
- **`src/prep.rs` `plan_needed_body` and `no_branch_body`** — add the
  question sentence (these currently only cover writing the plan file).
- **`src/implement.rs` `branch_body` and `no_branch_body`** — add the
  question sentence; this is the phase where #41 hit the prompts.
- **`src/implement.rs` `review_body` / `review_no_branch_body`** — add it too
  for completeness; with the review-phase guard relaxed (§1) `--question`
  works here.

To avoid copy-paste drift, factor the sentence into a small shared helper in
`render.rs` (next to `wait_instruction`), e.g.:

```rust
/// One-line reminder, shared across phase banners, to route questions to the
/// thread rather than an interactive prompt.
pub fn question_instruction(number: u64) -> String {
    format!(
        "If you need an answer from the user to proceed, post the question \
         with `ghwf hand-off {number} --question` (body from stdin) and then \
         `ghwf wait {number}` — never an interactive prompt."
    )
}
```

and interpolate it into each body alongside the existing
`wait_instruction(number)`.

---

## 4. README (`README.md`)

- In the labels / workflow section, note that a blocking question
  (`hand-off --question`) flips the issue to `needs-you` while Claude waits,
  same as an end-of-phase hand-off, but without advancing the phase.
- Wherever `hand-off` / the skill is described, mention the `--question` mode
  and the "no interactive prompts" policy.
- No `ghwf.toml` change — this adds no config key, so the CLAUDE.md
  "adding a config option" checklist does not apply.

---

## 5. Tests

- **`src/main.rs` / wherever `hand_off` is unit-testable.** The function is
  currently I/O-heavy (stdin, network). Keep the change small: if there's no
  seam, lean on the existing `render`/`state` unit tests below plus a manual
  check. If a thin pure helper falls out naturally (e.g. "compose hand-off
  body given question flag + prompt"), unit-test that.
- **`src/render.rs`** — `hand_off_prompt` tests are unaffected. Add a test
  that `question_instruction` mentions `--question` and the issue number, and
  assert `pre_plan_body` / the prep / implement bodies include it (mirroring
  the existing `*_include_wait_instruction` tests).
- **`src/install.rs`** — existing `skill_content_carries_the_marker` etc.
  still hold; optionally assert `SKILL_CONTENT` contains `--question` so the
  guidance can't silently regress.
- **`src/state.rs`** — `parse_prompted_directive` is unchanged; no new test
  needed, but the question-mode "no watch" behaviour rests on it returning
  `None` for a prompt-less body, which existing tests already cover.

---

## 6. Order of work

1. `hand-off --question` plumbing (main.rs) — the behavioural core.
2. `question_instruction` helper + banner edits (render/prep/implement).
3. Skill text (install.rs).
4. README.
5. Tests + `cargo test`, `cargo clippy`.

## Decision: flag name

`--question` reads best at the call site (`ghwf hand-off 43 --question`) and
in the skill. Alternatives considered: `--blocked` (describes Claude's state,
not the action), `--await` (collides with the wait concept), `--no-advance`
(describes the mechanism, not the intent). Going with `--question` unless the
reviewer prefers another on the PR.
