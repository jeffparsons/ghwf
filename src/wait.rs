use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::github::{self, Conditional};
use crate::models::{Comment, Issue, Reaction, ReviewComment, User};
use crate::state::{self, PostedRef, ReactionWatch, WaitState};
use crate::{render, store};

/// Exit code when the timeout elapses with nothing new (exit 0 = activity
/// detected, exit 1 = error).
pub const EXIT_TIMEOUT: i32 = 2;

/// Direct polling starts here…
const BACKOFF_FLOOR: Duration = Duration::from_secs(5);
/// …doubles while idle, and caps here. Reaching the cap hands over to
/// feed-first idle mode.
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// In feed mode, a full direct cycle runs this often as the lag backstop.
const FEED_SWEEP_INTERVAL: Duration = Duration::from_secs(300);
/// Feed cadence floor; raised by the `X-Poll-Interval` header when larger.
const FEED_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// This many consecutive failed cycles aborts the wait.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Block until new activity appears on the issue (or its PR), or the timeout
/// elapses. Returns normally on activity (exit 0); exits the process with
/// `EXIT_TIMEOUT` on timeout; errors otherwise.
pub fn run(issue: &str, timeout_secs: u64) -> Result<()> {
    let repo_ctx = github::config_repo()?;
    let (owner, repo, number) = github::resolve_issue_ref(issue, repo_ctx.as_ref())?;
    let mut issue_state = state::load(&owner, &repo, number)?;
    let Some(mut wait_state) = issue_state.wait.take() else {
        bail!("no wait baseline recorded for issue #{number}; run `ghwf work-on {number}` first.");
    };
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let last_posted = issue_state.last_posted.clone();

    // Outside a Claude session there is no token; only status comments hide.
    let my_token = match std::env::var(store::SESSION_ID_ENV) {
        Ok(id) if !id.is_empty() => Some(store::session_token(&id)?),
        _ => None,
    };

    let endpoints = poll_endpoints(&owner, &repo, number, pr_number, &wait_state);
    // The reaction watches again, on their own: the events feed is
    // structurally blind to reactions, so feed mode polls these directly
    // every cycle rather than leaving them to the backstop sweep. They share
    // ETag keys with their twins in `endpoints`.
    let watch_endpoints = reaction_endpoints(&owner, &repo, &wait_state.reaction_watches);
    let feed_endpoint = format!("repos/{owner}/{repo}/events?per_page=100");

    match pr_number {
        Some(pr) => println!(
            "Waiting for new activity on issue #{number} or PR #{pr} (timeout {timeout_secs} s)…"
        ),
        None => {
            println!("Waiting for new activity on issue #{number} (timeout {timeout_secs} s)…")
        }
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut backoff = BACKOFF_FLOOR;
    let mut consecutive_failures: u32 = 0;
    let mut mode = Mode::Direct;
    // Set once direct polling has gone quiet and the trust gate has failed,
    // so the gate isn't re-fetched every cycle at the cap.
    let mut feed_distrusted = false;

    loop {
        // One cycle in the current mode; its result decides reasons and pace.
        let cycle = match &mut mode {
            Mode::Direct => direct_cycle(&endpoints, &mut wait_state, my_token.as_deref()),
            Mode::Feed {
                last_sweep,
                interval,
            } => {
                if last_sweep.elapsed() >= FEED_SWEEP_INTERVAL {
                    // The lag backstop: a full direct cycle, on schedule.
                    *last_sweep = Instant::now();
                    direct_cycle(&endpoints, &mut wait_state, my_token.as_deref())
                } else {
                    feed_cycle(
                        &feed_endpoint,
                        &mut wait_state,
                        number,
                        pr_number,
                        my_token.as_deref(),
                        interval,
                    )
                    .and_then(|mut outcome| {
                        // The feed can't show reactions; poll the watches too.
                        let reactions =
                            direct_cycle(&watch_endpoints, &mut wait_state, my_token.as_deref())?;
                        outcome.reasons.extend(reactions.reasons);
                        outcome.fresh |= reactions.fresh;
                        Ok(outcome)
                    })
                }
            }
        };

        match cycle {
            Ok(outcome) => {
                consecutive_failures = 0;
                if !outcome.reasons.is_empty() {
                    persist(&owner, &repo, number, &mut issue_state, &wait_state);
                    for reason in &outcome.reasons {
                        println!("{reason}");
                    }
                    return Ok(());
                }
                if outcome.fresh {
                    // Something changed, even if nothing meaningful (e.g. our
                    // own post): things are moving, poll eagerly again.
                    backoff = BACKOFF_FLOOR;
                    feed_distrusted = false;
                }
            }
            Err(err) if github::is_rate_limited(&err) => {
                eprintln!("warning: rate limited; backing off: {err:#}");
                backoff = BACKOFF_CAP;
                if matches!(mode, Mode::Feed { .. }) {
                    mode = Mode::Direct;
                }
            }
            Err(err) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(err.context("giving up after repeated polling failures"));
                }
                eprintln!("warning: poll failed (attempt {consecutive_failures}): {err:#}");
                if matches!(mode, Mode::Feed { .. }) {
                    // A flaky feed isn't worth waiting on; direct polling at
                    // the cap is cheap enough.
                    mode = Mode::Direct;
                    backoff = BACKOFF_CAP;
                }
            }
        }

        // Quiet at the cap: try handing over to feed-first idle mode.
        if matches!(mode, Mode::Direct) && backoff >= BACKOFF_CAP && !feed_distrusted {
            match enter_feed_mode(
                &feed_endpoint,
                &mut wait_state,
                number,
                pr_number,
                last_posted.as_ref(),
                my_token.as_deref(),
            ) {
                Ok(FeedEntry::Wake(reasons)) => {
                    persist(&owner, &repo, number, &mut issue_state, &wait_state);
                    for reason in &reasons {
                        println!("{reason}");
                    }
                    return Ok(());
                }
                Ok(FeedEntry::Trusted(interval)) => {
                    mode = Mode::Feed {
                        last_sweep: Instant::now(),
                        interval,
                    };
                }
                Ok(FeedEntry::Lagging) => {
                    // The feed is behind right now; don't re-check until
                    // direct polling sees movement again.
                    feed_distrusted = true;
                }
                Err(err) => {
                    eprintln!(
                        "warning: events feed unavailable; staying with direct polling: {err:#}"
                    );
                    feed_distrusted = true;
                }
            }
        }

        let pace = match &mode {
            Mode::Direct => {
                let sleep = backoff;
                backoff = (backoff * 2).min(BACKOFF_CAP);
                sleep
            }
            Mode::Feed { interval, .. } => *interval,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(pace.min(remaining));
        if Instant::now() >= deadline {
            break;
        }
    }

    persist(&owner, &repo, number, &mut issue_state, &wait_state);
    println!(
        "No new activity within {timeout_secs} s. Run `ghwf wait {number}` again to keep waiting."
    );
    std::process::exit(EXIT_TIMEOUT);
}

/// Which polling strategy the loop is in.
enum Mode {
    /// Conditional GETs against the issue/PR endpoints, with backoff.
    Direct,
    /// One conditional GET of the events feed per cycle, plus a periodic
    /// direct sweep as the lag backstop.
    Feed {
        last_sweep: Instant,
        interval: Duration,
    },
}

/// What one poll cycle observed.
#[derive(Default)]
struct CycleOutcome {
    /// Wake reasons; non-empty means exit 0.
    reasons: Vec<String>,
    /// Whether any endpoint returned a fresh (200) response.
    fresh: bool,
}

/// The outcome of attempting to enter feed mode.
enum FeedEntry {
    /// The entry fetch itself found something to wake on.
    Wake(Vec<String>),
    /// The feed passed the trust gate; poll it at this cadence.
    Trusted(Duration),
    /// Our own recent post is missing from the feed — it is lagging.
    Lagging,
}

/// A polled endpoint: its ETag key in `WaitState::etags`, URL, and how to
/// evaluate a fresh response.
struct Endpoint {
    key: &'static str,
    url: String,
    kind: EndpointKind,
}

enum EndpointKind {
    /// The issue object; wakes on a fingerprint change.
    IssueObject,
    /// A conversation comment list; the noun names the thread in reasons.
    Conversation(&'static str),
    /// The PR's inline review comment list.
    ReviewComments,
    /// The reactions on one thread's watched approval prompt; the key
    /// (`issue` / `pr`) selects the baseline in `WaitState::reaction_watches`.
    Reactions(&'static str),
}

/// The fixed set of endpoints one `wait` invocation polls. The PR endpoints
/// exist only when prep state records a PR (only `work-on` opens PRs, so the
/// set can't change mid-wait); the reaction watches are likewise fixed at
/// entry.
fn poll_endpoints(
    owner: &str,
    repo: &str,
    number: u64,
    pr: Option<u64>,
    wait: &WaitState,
) -> Vec<Endpoint> {
    let since = &wait.since;
    let mut endpoints = vec![
        Endpoint {
            key: "issue",
            url: format!("repos/{owner}/{repo}/issues/{number}"),
            kind: EndpointKind::IssueObject,
        },
        Endpoint {
            key: "issue_comments",
            url: format!(
                "repos/{owner}/{repo}/issues/{number}/comments?per_page=100&since={since}"
            ),
            kind: EndpointKind::Conversation("issue thread"),
        },
    ];
    if let Some(pr) = pr {
        endpoints.push(Endpoint {
            key: "pr_comments",
            url: format!("repos/{owner}/{repo}/issues/{pr}/comments?per_page=100&since={since}"),
            kind: EndpointKind::Conversation("PR thread"),
        });
        endpoints.push(Endpoint {
            key: "pr_review_comments",
            url: format!("repos/{owner}/{repo}/pulls/{pr}/comments?per_page=100&since={since}"),
            kind: EndpointKind::ReviewComments,
        });
    }
    endpoints.extend(reaction_endpoints(owner, repo, &wait.reaction_watches));
    endpoints
}

/// One endpoint per recorded reaction watch: the watched prompt comment's
/// reactions list. A reaction bumps neither the comment's `updated_at` nor
/// the events feed, so this is the only way a 👍 can wake a wait.
fn reaction_endpoints(
    owner: &str,
    repo: &str,
    watches: &BTreeMap<String, ReactionWatch>,
) -> Vec<Endpoint> {
    ["issue", "pr"]
        .into_iter()
        .filter_map(|thread| {
            let watch = watches.get(thread)?;
            Some(Endpoint {
                key: match thread {
                    "issue" => "reactions_issue",
                    _ => "reactions_pr",
                },
                url: format!(
                    "repos/{owner}/{repo}/issues/comments/{}/reactions?per_page=100",
                    watch.comment_id
                ),
                kind: EndpointKind::Reactions(thread),
            })
        })
        .collect()
}

/// One direct cycle: a conditional GET per endpoint, evaluating fresh bodies
/// against the baseline.
fn direct_cycle(
    endpoints: &[Endpoint],
    wait: &mut WaitState,
    my_token: Option<&str>,
) -> Result<CycleOutcome> {
    let mut outcome = CycleOutcome::default();
    for endpoint in endpoints {
        let etag = wait.etags.get(endpoint.key).cloned();
        match github::gh_api_conditional(&endpoint.url, etag.as_deref())? {
            Conditional::NotModified { .. } => {}
            Conditional::Fresh { etag, body, .. } => {
                outcome.fresh = true;
                if let Some(etag) = etag {
                    wait.etags.insert(endpoint.key.to_string(), etag);
                }
                evaluate_fresh(endpoint, &body, wait, my_token, &mut outcome.reasons)?;
            }
        }
    }
    Ok(outcome)
}

/// Evaluate one fresh response body against the baseline, appending wake
/// reasons.
fn evaluate_fresh(
    endpoint: &Endpoint,
    body: &str,
    wait: &WaitState,
    my_token: Option<&str>,
    reasons: &mut Vec<String>,
) -> Result<()> {
    match &endpoint.kind {
        EndpointKind::IssueObject => {
            let issue: Issue = serde_json::from_str(body)
                .with_context(|| format!("failed to parse issue JSON from {}", endpoint.url))?;
            // `updated_at` (and so the ETag) bumps on mere comment activity;
            // only a content change is a reason. Comment endpoints decide
            // comment activity.
            let fingerprint =
                state::issue_fingerprint(&issue.title, issue.body.as_deref(), &issue.state);
            if fingerprint != wait.issue_fingerprint {
                reasons.push("The issue was edited (title, body, or state changed).".to_string());
            }
        }
        EndpointKind::Conversation(noun) => {
            let comments: Vec<Comment> = serde_json::from_str(body)
                .with_context(|| format!("failed to parse comments JSON from {}", endpoint.url))?;
            comment_reasons(&comments, noun, &wait.comments, my_token, reasons);
        }
        EndpointKind::Reactions(thread) => {
            let reactions: Vec<Reaction> = serde_json::from_str(body)
                .with_context(|| format!("failed to parse reactions JSON from {}", endpoint.url))?;
            let noun = match *thread {
                "pr" => "PR thread",
                _ => "issue thread",
            };
            // Only a 👍 the baseline hasn't seen is activity; other reaction
            // kinds carry no workflow meaning.
            let baseline = wait.reaction_watches.get(*thread).map(|w| &w.plus_one_ids);
            for reaction in &reactions {
                if !reaction.is_thumbs_up() {
                    continue;
                }
                if baseline.is_some_and(|ids| ids.contains(&reaction.id)) {
                    continue;
                }
                reasons.push(format!(
                    "New 👍 reaction from {} on the approval prompt ({noun}).",
                    reaction.user.login
                ));
            }
        }
        EndpointKind::ReviewComments => {
            let comments: Vec<ReviewComment> = serde_json::from_str(body).with_context(|| {
                format!("failed to parse review comments JSON from {}", endpoint.url)
            })?;
            for comment in &comments {
                if render::hidden_from_digest(&comment.body, my_token) {
                    continue;
                }
                let hash = store::content_hash(&comment.body);
                match wait.review_comments.get(&comment.id) {
                    Some(known) if *known == hash => {}
                    Some(_) => reasons.push(format!(
                        "Inline review comment from {} updated on `{}`.",
                        comment.user.login,
                        comment.location()
                    )),
                    None => reasons.push(format!(
                        "New inline review comment from {} on `{}`.",
                        comment.user.login,
                        comment.location()
                    )),
                }
            }
        }
    }
    Ok(())
}

/// Append a wake reason for each conversation comment that is neither hidden
/// nor already in the baseline with the same content. The `?since=` overlap
/// re-delivers the newest baselined comment by design; the hash map filters
/// it.
fn comment_reasons(
    comments: &[Comment],
    noun: &str,
    baseline: &BTreeMap<u64, String>,
    my_token: Option<&str>,
    reasons: &mut Vec<String>,
) {
    for comment in comments {
        if render::hidden_from_digest(&comment.body, my_token) {
            continue;
        }
        let hash = store::content_hash(&comment.body);
        match baseline.get(&comment.id) {
            Some(known) if *known == hash => {}
            Some(_) => reasons.push(format!(
                "Comment from {} updated on the {noun}.",
                comment.user.login
            )),
            None => reasons.push(format!(
                "New comment on the {noun} from {}.",
                comment.user.login
            )),
        }
    }
}

/// An entry in the repo events feed, trimmed to the fields the wake rule and
/// trust gate need.
#[derive(Deserialize)]
struct FeedEvent {
    #[serde(rename = "type")]
    kind: String,
    created_at: String,
    #[serde(default)]
    payload: FeedPayload,
}

#[derive(Deserialize, Default)]
struct FeedPayload {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    issue: Option<FeedSubject>,
    #[serde(default)]
    pull_request: Option<FeedSubject>,
    #[serde(default)]
    comment: Option<FeedComment>,
}

#[derive(Deserialize)]
struct FeedSubject {
    number: u64,
}

#[derive(Deserialize)]
struct FeedComment {
    id: u64,
    body: String,
    user: User,
}

/// Attempt the handover to feed mode: one unconditional feed fetch, evaluated
/// both for an immediate wake and for the trust gate.
fn enter_feed_mode(
    feed_endpoint: &str,
    wait: &mut WaitState,
    number: u64,
    pr: Option<u64>,
    last_posted: Option<&PostedRef>,
    my_token: Option<&str>,
) -> Result<FeedEntry> {
    // Unconditional: the trust gate needs page content, not a 304.
    let Conditional::Fresh {
        etag,
        poll_interval,
        body,
    } = github::gh_api_conditional(feed_endpoint, None)?
    else {
        bail!("events feed returned 304 to an unconditional request");
    };
    if let Some(etag) = etag {
        wait.etags.insert("events".to_string(), etag);
    }
    let events: Vec<FeedEvent> =
        serde_json::from_str(&body).context("failed to parse events feed JSON")?;

    let reasons = feed_wake_reasons(&events, number, pr, &wait.since, my_token);
    if !reasons.is_empty() {
        return Ok(FeedEntry::Wake(reasons));
    }
    if !feed_trusted(&events, last_posted) {
        return Ok(FeedEntry::Lagging);
    }
    Ok(FeedEntry::Trusted(feed_interval(poll_interval)))
}

/// One feed-mode cycle: a conditional GET of the events feed.
fn feed_cycle(
    feed_endpoint: &str,
    wait: &mut WaitState,
    number: u64,
    pr: Option<u64>,
    my_token: Option<&str>,
    interval: &mut Duration,
) -> Result<CycleOutcome> {
    let etag = wait.etags.get("events").cloned();
    match github::gh_api_conditional(feed_endpoint, etag.as_deref())? {
        Conditional::NotModified { poll_interval } => {
            *interval = feed_interval(poll_interval);
            Ok(CycleOutcome::default())
        }
        Conditional::Fresh {
            etag,
            poll_interval,
            body,
        } => {
            *interval = feed_interval(poll_interval);
            if let Some(etag) = etag {
                wait.etags.insert("events".to_string(), etag);
            }
            let events: Vec<FeedEvent> =
                serde_json::from_str(&body).context("failed to parse events feed JSON")?;
            Ok(CycleOutcome {
                reasons: feed_wake_reasons(&events, number, pr, &wait.since, my_token),
                // Fresh here means *some* repo activity, not necessarily ours;
                // don't let unrelated churn reset the direct backoff.
                fresh: false,
            })
        }
    }
}

/// The feed polling cadence: the advertised `X-Poll-Interval`, floored at one
/// minute.
fn feed_interval(poll_interval: Option<u64>) -> Duration {
    FEED_MIN_INTERVAL.max(Duration::from_secs(poll_interval.unwrap_or(0)))
}

/// Wake reasons from a feed page: events after the baseline watermark that
/// touch our issue or PR. Comment payloads are embedded, so the hidden filter
/// applies directly — our own posts never wake us, even via the feed.
fn feed_wake_reasons(
    events: &[FeedEvent],
    number: u64,
    pr: Option<u64>,
    since: &str,
    my_token: Option<&str>,
) -> Vec<String> {
    let ours = |subject: &Option<FeedSubject>| {
        subject
            .as_ref()
            .is_some_and(|s| s.number == number || Some(s.number) == pr)
    };
    let mut reasons = Vec::new();
    for event in events {
        if event.created_at.as_str() <= since {
            continue;
        }
        match event.kind.as_str() {
            "IssueCommentEvent" if ours(&event.payload.issue) => {
                let Some(comment) = event.payload.comment.as_ref() else {
                    continue;
                };
                if render::hidden_from_digest(&comment.body, my_token) {
                    continue;
                }
                let noun = match (&event.payload.issue, pr) {
                    (Some(subject), Some(pr)) if subject.number == pr => "PR thread",
                    _ => "issue thread",
                };
                reasons.push(format!(
                    "New comment on the {noun} from {} (via the events feed).",
                    comment.user.login
                ));
            }
            "PullRequestReviewCommentEvent" if ours(&event.payload.pull_request) => {
                let Some(comment) = event.payload.comment.as_ref() else {
                    continue;
                };
                if render::hidden_from_digest(&comment.body, my_token) {
                    continue;
                }
                reasons.push(format!(
                    "New inline review comment from {} (via the events feed).",
                    comment.user.login
                ));
            }
            "IssuesEvent" if ours(&event.payload.issue) => {
                let action = event.payload.action.as_deref().unwrap_or("changed");
                reasons.push(format!("The issue was {action} (via the events feed)."));
            }
            _ => {}
        }
    }
    reasons
}

/// The feed trust gate: our own most recent post must be visible in the page,
/// or old enough to have scrolled out of it. Newer than the page's oldest
/// event yet absent means the feed is lagging right now.
fn feed_trusted(events: &[FeedEvent], last_posted: Option<&PostedRef>) -> bool {
    let Some(post) = last_posted else {
        // Nothing to calibrate against; the periodic direct sweep bounds the
        // risk.
        return true;
    };
    if events
        .iter()
        .any(|e| e.payload.comment.as_ref().is_some_and(|c| c.id == post.id))
    {
        return true;
    }
    // The feed is newest-first; the last entry is the oldest visible.
    match events.last() {
        Some(oldest) => post.created_at < oldest.created_at,
        // An empty feed despite a recorded post: lagging (or wiped) — don't
        // trust it.
        None => false,
    }
}

/// Persist the wait state (ETags) back onto the issue state, best-effort: a
/// failed write only costs the next invocation a few uncached polls.
fn persist(
    owner: &str,
    repo: &str,
    number: u64,
    issue_state: &mut state::IssueState,
    wait: &WaitState,
) {
    issue_state.wait = Some(wait.clone());
    if let Err(err) = state::save(owner, repo, number, issue_state) {
        eprintln!("warning: failed to persist wait state: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        comment_reasons, evaluate_fresh, feed_interval, feed_trusted, feed_wake_reasons, Endpoint,
        EndpointKind, FeedComment, FeedEvent, FeedPayload, FeedSubject,
    };
    use crate::models::{Comment, User};
    use crate::state::{PostedRef, ReactionWatch, WaitState};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn comment(id: u64, body: &str) -> Comment {
        Comment {
            id,
            user: User {
                login: "user".to_string(),
            },
            body: body.to_string(),
            created_at: "2026-06-06T12:00:00Z".to_string(),
            updated_at: "2026-06-06T12:00:00Z".to_string(),
            html_url: format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
            author_association: "OWNER".to_string(),
            reactions: None,
        }
    }

    fn baseline(entries: &[(u64, &str)]) -> BTreeMap<u64, String> {
        entries
            .iter()
            .map(|(id, body)| (*id, crate::store::content_hash(body)))
            .collect()
    }

    #[test]
    fn unknown_comment_wakes() {
        let mut reasons = Vec::new();
        comment_reasons(
            &[comment(5, "hello")],
            "issue thread",
            &baseline(&[]),
            Some("tok"),
            &mut reasons,
        );
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("New comment on the issue thread from user"));
    }

    #[test]
    fn baselined_redelivery_does_not_wake() {
        // The `?since=` overlap re-delivers the newest baselined comment.
        let mut reasons = Vec::new();
        comment_reasons(
            &[comment(5, "hello")],
            "issue thread",
            &baseline(&[(5, "hello")]),
            Some("tok"),
            &mut reasons,
        );
        assert!(reasons.is_empty());
    }

    #[test]
    fn edited_baselined_comment_wakes() {
        let mut reasons = Vec::new();
        comment_reasons(
            &[comment(5, "hello, edited")],
            "PR thread",
            &baseline(&[(5, "hello")]),
            Some("tok"),
            &mut reasons,
        );
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("Comment from user updated on the PR thread"));
    }

    #[test]
    fn own_and_status_comments_never_wake() {
        let own = crate::render::build_comment_body("done!", Some("tok"));
        let status = crate::render::build_status_comment_body("update");
        let other_session = crate::render::build_comment_body("hi", Some("other"));
        let mut reasons = Vec::new();
        comment_reasons(
            &[
                comment(1, &own),
                comment(2, &status),
                comment(3, &other_session),
            ],
            "issue thread",
            &baseline(&[]),
            Some("tok"),
            &mut reasons,
        );
        // Only the other session's comment is activity.
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("New comment"));
    }

    fn reaction_json(id: u64, content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "user": { "login": "reactor" },
            "content": content,
            "created_at": "2026-06-06T13:00:00Z",
        })
    }

    /// Run `evaluate_fresh` for an issue-thread reactions endpoint against a
    /// baseline of already-seen 👍 ids, returning the wake reasons.
    fn reaction_reasons(body: serde_json::Value, baseline: &[u64]) -> Vec<String> {
        let endpoint = Endpoint {
            key: "reactions_issue",
            url: "repos/o/r/issues/comments/9/reactions?per_page=100".to_string(),
            kind: EndpointKind::Reactions("issue"),
        };
        let wait = WaitState {
            reaction_watches: [(
                "issue".to_string(),
                ReactionWatch {
                    comment_id: 9,
                    plus_one_ids: baseline.iter().copied().collect(),
                },
            )]
            .into(),
            ..Default::default()
        };
        let mut reasons = Vec::new();
        evaluate_fresh(
            &endpoint,
            &body.to_string(),
            &wait,
            Some("tok"),
            &mut reasons,
        )
        .unwrap();
        reasons
    }

    #[test]
    fn unknown_thumbs_up_wakes() {
        let reasons = reaction_reasons(serde_json::json!([reaction_json(100, "+1")]), &[]);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("New 👍 reaction from reactor"));
        assert!(reasons[0].contains("issue thread"));
    }

    #[test]
    fn baselined_thumbs_up_does_not_wake() {
        let reasons = reaction_reasons(serde_json::json!([reaction_json(100, "+1")]), &[100]);
        assert!(reasons.is_empty());
    }

    #[test]
    fn non_thumbs_up_reactions_do_not_wake() {
        let body = serde_json::json!([reaction_json(100, "heart"), reaction_json(101, "rocket")]);
        assert!(reaction_reasons(body, &[]).is_empty());
    }

    fn feed_comment_event(issue: u64, comment_id: u64, body: &str, at: &str) -> FeedEvent {
        FeedEvent {
            kind: "IssueCommentEvent".to_string(),
            created_at: at.to_string(),
            payload: FeedPayload {
                action: Some("created".to_string()),
                issue: Some(FeedSubject { number: issue }),
                pull_request: None,
                comment: Some(FeedComment {
                    id: comment_id,
                    body: body.to_string(),
                    user: User {
                        login: "user".to_string(),
                    },
                }),
            },
        }
    }

    const SINCE: &str = "2026-06-06T12:00:00Z";

    #[test]
    fn feed_wakes_on_matching_comment_event() {
        let events = [feed_comment_event(7, 1, "hi", "2026-06-06T13:00:00Z")];
        let reasons = feed_wake_reasons(&events, 7, Some(18), SINCE, Some("tok"));
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("issue thread"));
        // The PR thread is named when the event is for the PR's number.
        let events = [feed_comment_event(18, 1, "hi", "2026-06-06T13:00:00Z")];
        let reasons = feed_wake_reasons(&events, 7, Some(18), SINCE, Some("tok"));
        assert!(reasons[0].contains("PR thread"));
    }

    #[test]
    fn feed_ignores_other_issues_old_events_and_own_comments() {
        let own = crate::render::build_comment_body("mine", Some("tok"));
        let events = [
            // A different issue.
            feed_comment_event(99, 1, "hi", "2026-06-06T13:00:00Z"),
            // Ours, but before the watermark.
            feed_comment_event(7, 2, "hi", "2026-06-06T11:00:00Z"),
            // Ours and fresh, but our own post.
            feed_comment_event(7, 3, &own, "2026-06-06T13:00:00Z"),
        ];
        assert!(feed_wake_reasons(&events, 7, Some(18), SINCE, Some("tok")).is_empty());
    }

    #[test]
    fn feed_wakes_on_issue_state_event() {
        let events = [FeedEvent {
            kind: "IssuesEvent".to_string(),
            created_at: "2026-06-06T13:00:00Z".to_string(),
            payload: FeedPayload {
                action: Some("closed".to_string()),
                issue: Some(FeedSubject { number: 7 }),
                pull_request: None,
                comment: None,
            },
        }];
        let reasons = feed_wake_reasons(&events, 7, None, SINCE, None);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("closed"));
    }

    fn posted(id: u64, at: &str) -> PostedRef {
        PostedRef {
            id,
            created_at: at.to_string(),
        }
    }

    #[test]
    fn trust_gate_visible_post_trusts() {
        let events = [feed_comment_event(7, 42, "status", "2026-06-06T13:00:00Z")];
        assert!(feed_trusted(
            &events,
            Some(&posted(42, "2026-06-06T13:00:00Z"))
        ));
    }

    #[test]
    fn trust_gate_missing_recent_post_distrusts() {
        let events = [feed_comment_event(7, 1, "old", "2026-06-06T10:00:00Z")];
        assert!(!feed_trusted(
            &events,
            Some(&posted(42, "2026-06-06T13:00:00Z"))
        ));
    }

    #[test]
    fn trust_gate_scrolled_out_post_trusts() {
        // Our post predates the oldest visible event: it fell out of the
        // window, which says nothing about lag.
        let events = [feed_comment_event(7, 1, "newer", "2026-06-06T13:00:00Z")];
        assert!(feed_trusted(
            &events,
            Some(&posted(42, "2026-06-06T09:00:00Z"))
        ));
    }

    #[test]
    fn trust_gate_without_recorded_post_trusts() {
        let events = [feed_comment_event(7, 1, "x", "2026-06-06T13:00:00Z")];
        assert!(feed_trusted(&events, None));
        // But an empty feed with a recorded post does not.
        assert!(!feed_trusted(
            &[],
            Some(&posted(42, "2026-06-06T13:00:00Z"))
        ));
    }

    #[test]
    fn feed_interval_floors_at_one_minute() {
        assert_eq!(feed_interval(None), Duration::from_secs(60));
        assert_eq!(feed_interval(Some(30)), Duration::from_secs(60));
        assert_eq!(feed_interval(Some(120)), Duration::from_secs(120));
    }
}
