# Plan — efficiently wait for approval / feedback (#7)

After `work-on`, the agent has nothing to do but wait for a human to comment
(an approval directive, or feedback). Today there is no waiting machinery:
banners say "wait for `/approve-X`" and rely on someone manually re-running
`work-on`. This plan adds `ghwf wait <issue>` — a command that blocks until
something new happens on the issue or its PR, polling cheaply with conditional
requests, so the user can walk away and drive the workflow from another
device.

Design decisions locked on the issue thread:

1. **A separate `wait` subcommand**, not a `work-on` flag. `wait` has a
   predictable contract (blocks until activity or `--timeout`), which an agent
   can set a shell timeout against; `work-on` stays the fast, idempotent
   re-entry point. The loop is `work-on` → do the work, post hand-off →
   `wait` → on wake, `work-on` again.
2. **Conditional GETs with stored ETags, not the events feed.** The events API
   is documented as non-real-time (latency up to hours) and repo-wide.
   Instead: `If-None-Match` against the same endpoints `work-on` reads. A 304
   means nothing changed, with no body to download or process. Verified
   empirically: `gh api -i` exposes the `Etag` header, replaying it yields
   `HTTP 304` (gh exits 1, but the status line is still on stdout), and the
   304 *does* count against the rate limit (`X-Ratelimit-Used` incremented) —
   which is why decision 6's backoff matters.
3. **No self-wakes.** ghwf status comments and this session's own comments
   (the existing marker machinery) don't count as activity; otherwise the
   agent's own hand-off post would wake it instantly.
4. **`work-on` records the baseline**, so a comment that lands while the agent
   is working (between `work-on` and `wait`) is detected on `wait`'s first
   poll rather than silently baselined away.
5. **Exit contract:** exit 0 = activity detected → run `work-on`; exit 2 =
   `--timeout` elapsed (default 540 s, under Claude's 10-minute shell limit)
   → re-run `wait`; exit 1 = error. Banners in every waiting phase spell out
   the loop.
6. **Backoff:** polling starts at 5 s, doubles while idle, caps at 60 s, and
   resets to the floor whenever a poll sees change. Worst case at the cap is
   ~240 req/hr, comfortably inside the 5000/hr limit.

## 1. Conditional request helper (`github.rs`)

```rust
/// The outcome of a conditional `gh api` GET.
pub enum Conditional {
    /// 304 — unchanged since the presented ETag.
    NotModified,
    /// 200 — fresh body, with the response's ETag for the next poll.
    Fresh { etag: Option<String>, body: String },
}

pub fn gh_api_conditional(endpoint: &str, etag: Option<&str>) -> Result<Conditional>
```

- Runs `gh api -i` (headers included), adding `-H "If-None-Match: {etag}"`
  when an ETag is presented. Single-page requests only — no `--paginate`; the
  `since` filter in section 4 keeps responses small.
- A pure helper parses the `-i` output: status line, headers (lookup
  case-insensitive — the server sends `Etag:`), and body split at the first
  blank line. Testable without a subprocess.
- `gh` exits 1 on a 304 but still prints the response head to stdout, so
  classification goes by the parsed status line: 304 → `NotModified`; 2xx →
  `Fresh` (ETag header stored verbatim — the server may answer a weak
  validator with a strong one, so no normalization, just replay what it
  sent). No parseable status line → the normal error path.
- The status code is also surfaced for non-2xx/304 (rate-limit handling in
  section 4 needs to distinguish 403/429 from other failures).

## 2. Wait baseline state (`state.rs`)

```rust
/// What `work-on` last observed, recorded for `wait` to poll against.
#[derive(Default, Serialize, Deserialize)]
pub struct WaitState {
    // Watermark for `?since=`: the max `updated_at` across everything the
    // recording run fetched. Server-side timestamps, so local clock skew
    // can't lose activity; ISO-8601 Zulu strings compare lexicographically.
    pub since: String,
    // Content hash of the issue's title + body + state.
    pub issue_fingerprint: String,
    // Comment id → body hash, for both conversation threads merged (ids are
    // globally unique) and inline review comments separately.
    pub comments: BTreeMap<u64, String>,
    pub review_comments: BTreeMap<u64, String>,
    // Endpoint key → last ETag, updated by `wait` as it polls.
    #[serde(default)]
    pub etags: BTreeMap<String, String>,
}
```

- `IssueState` gains `#[serde(default, skip_serializing_if = "Option::is_none")]
  pub wait: Option<WaitState>`. Pre-existing state files deserialize to
  `None`; `wait` without a baseline exits 1 telling the user to run
  `work-on` first.
- Issue-scoped (not session-scoped, unlike the seen cache) deliberately:
  directives are issue-scoped, the ETags describe the issue's endpoints, and
  any session's `wait` should wake on the same things.

## 3. `work-on` records the baseline (`main.rs`)

At the end of `work_on`, where the seen record is already being assembled
from the same data, build and store `WaitState`:

- `since` = max `updated_at` over the issue and every comment fetched this
  run (conversation on both threads, plus inline review comments when the
  digest fetched them).
- `comments` covers the issue thread and, when fetched for directive
  scanning or the digest, the PR conversation thread. Hashes are recorded
  *unfiltered* (status and Claude comments included) — hiding is wake-time
  logic (section 4), not baseline logic.
- `review_comments` is empty in the phases that don't fetch them; a
  since-filtered poll returning any unknown inline comment then wakes, which
  is correct.
- `etags` starts empty: the poll URLs embed the new `since`, so ETags from a
  previous baseline are stale by construction.

`create-issue-comment` needs no change: the hand-off comment posted *after*
`work-on` is absent from the baseline, but wake evaluation filters
own-session and status comments by marker (decision 3), not by baseline
membership.

## 4. The `wait` subcommand (new `wait.rs`, CLI in `main.rs`)

`ghwf wait <issue> [--timeout <secs>]` (default 540).

Setup: resolve the issue, load `IssueState`, bail without a baseline. Print
one startup line ("Waiting for new activity on issue #7 / PR #18; timeout
540 s…") so the long-running command isn't silent. The session token comes
from the env when present; `hidden_from_digest` is hoisted from `main.rs`
into `render.rs` and generalized to `Option<&str>` (outside a Claude session
only status comments are hidden).

Each poll cycle hits up to four endpoints, each with its stored ETag:

| Key | Endpoint | Wakes when |
|-----|----------|------------|
| `issue` | `…/issues/{n}` | fingerprint (title+body+state) differs from baseline |
| `issue_comments` | `…/issues/{n}/comments?per_page=100&since={since}` | see below |
| `pr_comments` | `…/issues/{pr}/comments?…&since={since}` | see below |
| `pr_review_comments` | `…/pulls/{pr}/comments?…&since={since}` | see below |

- The PR endpoints poll only when prep state records a `pr_number` (PRs are
  only opened by `work-on`, so the set is static for the life of one `wait`).
- Comments wake rule: any returned comment that is not hidden and whose
  `(id, body-hash)` is absent from the baseline map. `since` returns items
  updated *at or after* the watermark, so the newest baselined item always
  reappears — the hash map filters it. New comments, edits to baselined
  comments, and other-session Claude comments all wake; this session's own
  comments and ghwf status updates never do.
- The issue object's `updated_at` (and hence ETag) bumps on mere comment
  activity; the fingerprint compare keeps that endpoint quiet and leaves
  comment decisions to the comment endpoints. It exists to catch issue
  body/title edits and open/closed transitions, which no comment list shows.
- On 304: nothing. On 200: persist the new ETag into state (best-effort
  write), evaluate the wake rule, and reset the backoff to the floor — even
  a self-caused change means things are moving.
- Wake: print one reason line per finding ("new comment on the PR thread
  from jeffparsons", "the issue was edited"), exit 0.
- Idle: sleep `min(backoff, time-to-deadline)`; at the deadline print "no
  new activity" and exit 2 (via `std::process::exit` — clap's `Result` path
  is reserved for exit 1).
- Failures: 403/429 sleep at the cap (rate-limited — don't hammer, don't
  die); other errors warn and back off; three *consecutive* failures exit 1.

## 5. Banner instructions (`render.rs`, `prep.rs`, `implement.rs`)

A shared `render::wait_instruction(number)` paragraph, appended to every
waiting-state banner body:

> When you have posted your comment(s) and have nothing else to do, run
> `ghwf wait {n}` — it blocks until there is new activity (up to ~9 minutes;
> give it a 10-minute command timeout). Exit 0 means new activity arrived:
> run `ghwf work-on {n}`. Exit 2 means nothing yet: run `ghwf wait {n}`
> again. Do not poll with your own sleep loops.

Applied to: `PRE_PLAN_BODY` (becomes a `pre_plan_body(number)` function — it
needs the issue number now), `prep::complete_body`, `implement::branch_body`
and `no_branch_body`, and both review bodies. The status-comment prose is
unchanged — `wait` is agent-side machinery, invisible to the user on GitHub.

## 6. README

A short subsection documenting the wait loop and the exit-code contract.

## 7. Tests

- HTTP parse helper: status/headers/body split; case-insensitive `Etag`
  lookup; 304 and 200 classification; garbage input errors.
- Wake evaluation (pure functions over baseline + fetched data): unknown
  comment wakes; baselined re-delivery (the `since` overlap) doesn't; edited
  baselined comment wakes; own-session and status comments never wake;
  another session's Claude comment does; fingerprint change wakes; no-token
  mode hides only status comments.
- Backoff: 5, 10, 20, 40, 60, 60…; reset on change; deadline clamps the last
  sleep.
- `WaitState` serde: old `IssueState` files load with `wait: None`;
  round-trip with ETags.
- Baseline assembly: `since` is the max `updated_at` across sources; hashes
  recorded unfiltered; empty `review_comments` when not fetched.
- Banners: the wait instruction appears in pre-plan, prep-complete,
  implement, and review bodies.

## Build order

1 → 2 → 3 → 4 → 5/6, tests alongside each.

## Out of scope / punted

- PR review *summaries* (a submitted review with no inline comments) — not
  surfaced by `work-on` today either; a future PR-object poll could cover
  them.
- Watching pushes, CI, or anything beyond the conversation surfaces.
- A `work-on --wait` convenience flag (trivial to add atop `wait` later).
- The events feed, webhooks, long-polling, and `X-Poll-Interval` (only the
  events feed sends it).
- A cross-issue shared poller; each `wait` watches one issue.
