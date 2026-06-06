mod config;
mod git;
mod github;
mod implement;
mod launch;
mod models;
mod prep;
mod render;
mod seen;
mod state;
mod store;
mod wait;
mod worktree;

use std::io::Read;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use render::{CommentView, ReviewCommentView};

#[derive(Parser)]
#[command(name = "ghwf", about = "GitHub WorkFlow")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Advance the workflow on an issue and report what's new and what to do next.
    WorkOn {
        /// An issue number (resolved against the current repo) or a full GitHub issue URL.
        issue: String,
        /// Work without a dedicated branch/worktree/PR (just write the plan file).
        #[arg(long)]
        no_branch: bool,
    },
    /// Post a comment to an issue (or PR), reading the body from stdin.
    ///
    /// The comment is prefixed with a "Claude says" header and tagged with hidden
    /// metadata identifying the authoring Claude session.
    CreateIssueComment {
        /// An issue number (resolved against the current repo) or a full GitHub issue URL.
        issue: String,
    },
    /// Print the absolute path of the worktree recorded for an issue.
    WorktreePath {
        /// An issue number (resolved against the current repo) or a full GitHub issue URL.
        issue: String,
    },
    /// Block until new activity appears on an issue (or its PR), or the timeout
    /// elapses.
    ///
    /// Exits 0 when activity is detected (run `ghwf work-on` to process it),
    /// 2 on timeout (nothing new — run `wait` again), and 1 on error.
    Wait {
        /// An issue number (resolved against the current repo) or a full GitHub issue URL.
        issue: String,
        /// Give up after this many seconds, with exit code 2.
        #[arg(long, default_value_t = 540)]
        timeout: u64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::WorkOn { issue, no_branch } => work_on(&issue, no_branch),
        Commands::CreateIssueComment { issue } => create_issue_comment(&issue),
        Commands::WorktreePath { issue } => worktree_path(&issue),
        Commands::Wait { issue, timeout } => wait::run(&issue, timeout),
    }
}

/// Print the absolute worktree path recorded for an issue (for scripts and the
/// `/work-on` slash command). Errors if no worktree has been created yet.
fn worktree_path(issue: &str) -> Result<()> {
    let repo_ctx = github::config_repo()?;
    let (owner, repo, number) = github::resolve_issue_ref(issue, repo_ctx.as_ref())?;
    let state = state::load(&owner, &repo, number)?;
    match state.prep.as_ref().and_then(|p| p.worktree_path.as_ref()) {
        Some(path) => {
            println!("{}", path.display());
            Ok(())
        }
        None => bail!(
            "no worktree recorded for issue #{number}; run `ghwf work-on {number}` \
             (in branch mode) to create one."
        ),
    }
}

fn work_on(issue: &str, no_branch: bool) -> Result<()> {
    // Identify this Claude session so we can scope the seen cache and suppress
    // our own comments. Without one we're running outside Claude Code: act as a
    // launcher instead, preparing the worktree and starting Claude in it.
    let session_id = match std::env::var(store::SESSION_ID_ENV) {
        Ok(id) if !id.is_empty() => id,
        _ => return launch::run(issue, no_branch),
    };

    // A discovered ghwf.toml is the source of truth for which repo to operate on.
    let repo_ctx = github::config_repo()?;
    let issue_data = github::fetch_issue(issue, repo_ctx.as_ref())?;
    let issue_comments = github::fetch_comments(issue, repo_ctx.as_ref())?;
    let (owner, repo) = github::parse_owner_repo(&issue_data.html_url)?;
    let number = issue_data.number;

    // Load the issue's workflow state once; mutate and save it at the end.
    let mut issue_state = state::load(&owner, &repo, number)?;

    // Approval directives are honoured from the issue thread and, once a PR
    // exists, its conversation thread too — fetched now, before directive
    // processing, and reused for the digest below.
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let early_pr_comments = match pr_number {
        Some(pr) => Some(github::fetch_comments(&pr.to_string(), repo_ctx.as_ref())?),
        None => None,
    };
    let outcome = advance_phase(
        &mut issue_state,
        &issue_comments,
        early_pr_comments.as_deref(),
    );

    // The phase-specific banner body. Prep-and-plan does real work here (and
    // hard-errors if it needs a config that's missing); implement/review are light.
    let phase = issue_state.phase;
    let body = match phase {
        state::Phase::PrePlan => render::pre_plan_body(number),
        state::Phase::PrepAndPlan => prep::run(
            &issue_data,
            &owner,
            &repo,
            number,
            no_branch,
            &mut issue_state,
        )?,
        state::Phase::Implement => {
            implement::run(&issue_data, &owner, &repo, number, &issue_state)?
        }
        state::Phase::Review => implement::review(&owner, &repo, number, &mut issue_state)?,
    };

    // We didn't hard-need a config (or we'd have errored above); still nudge if
    // it's absent.
    config::warn_if_absent();

    // Record this session as the worktree's session when running inside it, so
    // the outside-Claude launcher can later resume it by id.
    if let Some(prep) = issue_state.prep.as_mut() {
        if let Some(worktree) = prep.worktree_path.clone() {
            if worktree::cwd_is_inside(&worktree) {
                prep.worktree_session_id = Some(session_id.clone());
            }
        }
    }

    // Post a status update to the conversation threads when something
    // user-visible happened: first engagement, a phase transition, or a
    // misfired directive. Posted after the phase body ran, so the prose states
    // facts (a review-phase PR has already been flipped to ready).
    // Re-read the PR number: the prep-and-plan phase body may have just opened
    // the PR.
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let status = render::render_status_comment(
        phase,
        &outcome.transitions,
        &outcome.notes,
        !issue_state.intro_posted,
        pr_number
            .map(|pr| format!("https://github.com/{owner}/{repo}/pull/{pr}"))
            .as_deref(),
    );
    let status_posted = status.is_some();
    if let Some(text) = status {
        let status_body = render::build_status_comment_body(&text);
        let mut posted = post_status(
            &number.to_string(),
            &status_body,
            repo_ctx.as_ref(),
            "issue",
        );
        if let Some(pr) = pr_number {
            posted = post_status(&pr.to_string(), &status_body, repo_ctx.as_ref(), "PR").or(posted);
        }
        // Remember the newest own post for feed-lag self-calibration in `wait`.
        if let Some(comment) = posted {
            issue_state.last_posted = Some(state::PostedRef {
                id: comment.id,
                created_at: comment.created_at,
            });
        }
        issue_state.intro_posted = true;
    }

    state::save(&owner, &repo, number, &issue_state)?;

    // Hard-error if this phase needs the issue's worktree but Claude isn't running
    // inside it. Done after saving so a just-created worktree is already persisted.
    if needs_worktree_guard(phase, &issue_state) {
        let worktree = issue_state
            .prep
            .as_ref()
            .and_then(|p| p.worktree_path.as_ref())
            .expect("guard requires a recorded worktree path");
        let config_dir = config::find()?.map(|located| located.dir);
        worktree::ensure_inside(worktree, config_dir.as_deref(), number)?;
    }

    let my_token = store::session_token(&session_id)?;

    // Start the wait baseline while the issue data is still in hand: the
    // fingerprint catches edits no comment list shows, and `since` accumulates
    // the max server-side `updated_at` observed this run (plan §3). Comments
    // fetched later for the digest fold in below.
    let mut wait_state = state::WaitState {
        since: issue_data.updated_at.clone(),
        issue_fingerprint: state::issue_fingerprint(
            &issue_data.title,
            issue_data.body.as_deref(),
            &issue_data.state,
        ),
        ..Default::default()
    };
    for comment in issue_comments
        .iter()
        .chain(early_pr_comments.iter().flatten())
    {
        wait_state
            .comments
            .insert(comment.id, store::content_hash(&comment.body));
        state::fold_since(&mut wait_state.since, &comment.updated_at);
    }

    // The issue is always the digest's primary subject. Once a PR exists, its
    // conversation thread and inline review comments are digested too, in
    // every phase — matching exactly what `wait` polls. The PR object itself
    // (body/title) is never digested.
    let (pr_comments, review_comments) = match pr_number {
        Some(pr) => {
            // Reuse the comments fetched for directive scanning; the fallback
            // only applies if the PR appeared during this run's phase body.
            let pr_comments = match early_pr_comments {
                Some(comments) => comments,
                None => github::fetch_comments(&pr.to_string(), repo_ctx.as_ref())?,
            };
            let pr_review_comments = github::fetch_review_comments(&owner, &repo, pr)?;
            (Some(pr_comments), Some(pr_review_comments))
        }
        None => (None, None),
    };

    // Fold in whatever the digest fetched beyond the early fetches: PR
    // comments when the PR appeared only during this run's phase body, and
    // inline review comments. (Re-inserts are identical no-ops.)
    for comment in pr_comments.iter().flatten() {
        wait_state
            .comments
            .insert(comment.id, store::content_hash(&comment.body));
        state::fold_since(&mut wait_state.since, &comment.updated_at);
    }
    for comment in review_comments.iter().flatten() {
        wait_state
            .review_comments
            .insert(comment.id, store::content_hash(&comment.body));
        state::fold_since(&mut wait_state.since, &comment.updated_at);
    }

    let record = seen::load(&session_id, &owner, &repo, number)?;

    let body_hash = store::content_hash(issue_data.body.as_deref().unwrap_or(""));
    let body_changed = record.issue_body_hash.as_deref() != Some(&body_hash);

    let new_issue = collect_new_comments(&issue_comments, &record.comments, &my_token);
    let new_pr = match pr_comments.as_deref() {
        Some(comments) => collect_new_comments(comments, &record.comments, &my_token),
        None => Vec::new(),
    };

    let mut new_review = Vec::new();
    for comment in review_comments.iter().flatten() {
        // Same filter as conversation comments, for symmetry — though ghwf
        // never authors inline review comments today.
        if render::hidden_from_digest(&comment.body, Some(&my_token)) {
            continue;
        }
        let hash = store::content_hash(&comment.body);
        let previous = record.review_comments.get(&comment.id);
        if previous != Some(&hash) {
            new_review.push(ReviewCommentView {
                comment,
                body: render::strip_ghwf_marker(&comment.body),
                location: comment.location(),
                updated: previous.is_some(),
            });
        }
    }

    println!(
        "{}",
        render::render_phase_banner(
            phase,
            &outcome.transitions,
            &outcome.notes,
            status_posted,
            &body
        )
    );
    println!();
    println!(
        "{}",
        render::render_work_on(
            &issue_data,
            body_changed,
            &new_issue,
            pr_number,
            &new_pr,
            &new_review
        )
    );

    // Record the current state so the next run only surfaces later changes.
    let updated = seen::SeenRecord {
        issue_body_hash: Some(body_hash),
        comments: issue_comments
            .iter()
            .chain(pr_comments.iter().flatten())
            .map(|c| (c.id, store::content_hash(&c.body)))
            .collect(),
        review_comments: match review_comments.as_ref() {
            Some(comments) => comments
                .iter()
                .map(|c| (c.id, store::content_hash(&c.body)))
                .collect(),
            // No PR yet, so not fetched this run; carry the cached map over
            // unchanged.
            None => record.review_comments,
        },
    };
    seen::save(&session_id, &owner, &repo, number, &updated)?;

    // Record the wait baseline last, so it reflects everything this run
    // fetched. A fresh baseline invalidates any stored poll ETags (the poll
    // URLs embed `since`), so they start empty.
    issue_state.wait = Some(wait_state);
    state::save(&owner, &repo, number, &issue_state)?;

    Ok(())
}

/// Post a ghwf status update to a conversation thread, best-effort: a failure
/// warns on stderr but never fails the run. Returns the created comment so the
/// caller can record it for feed-lag self-calibration.
fn post_status(
    subject: &str,
    body: &str,
    repo_ctx: Option<&github::RepoRef>,
    noun: &str,
) -> Option<models::Comment> {
    match github::post_issue_comment(subject, body, repo_ctx) {
        Ok(comment) => Some(comment),
        Err(err) => {
            eprintln!("warning: failed to post the status update to the {noun}: {err:#}");
            None
        }
    }
}

/// Collect one thread's new-or-changed conversation comments, diffed against
/// the seen-record's comment map by content hash. Hidden comments (ghwf status
/// updates, this session's own posts) are skipped.
fn collect_new_comments<'a>(
    comments: &'a [models::Comment],
    seen: &std::collections::BTreeMap<u64, String>,
    my_token: &str,
) -> Vec<CommentView<'a>> {
    let mut new = Vec::new();
    for comment in comments {
        if render::hidden_from_digest(&comment.body, Some(my_token)) {
            continue;
        }
        let hash = store::content_hash(&comment.body);
        let previous = seen.get(&comment.id);
        if previous != Some(&hash) {
            new.push(CommentView {
                comment,
                body: render::strip_ghwf_marker(&comment.body),
                updated: previous.is_some(),
            });
        }
    }
    new
}

/// Whether this phase requires Claude to be inside the issue's worktree.
///
/// Only branch-mode phases that operate on a created worktree qualify:
/// prep-and-plan (Claude must write the plan there) and implement (Claude codes
/// there). Pre-plan, review, and `--no-branch` work don't need it.
fn needs_worktree_guard(phase: state::Phase, issue_state: &state::IssueState) -> bool {
    let Some(prep) = issue_state.prep.as_ref() else {
        return false;
    };
    if prep.no_branch || prep.worktree_path.is_none() {
        return false;
    }
    matches!(phase, state::Phase::PrepAndPlan | state::Phase::Implement)
}

/// What directive processing did this run: phase transitions that fired, and
/// consumed directives that didn't (with why).
#[derive(Default)]
struct AdvanceOutcome {
    transitions: Vec<render::Transition>,
    notes: Vec<render::DirectiveNote>,
}

/// Process any new approval directives on the issue and PR conversation
/// threads, advancing the issue's phase in `issue_state`.
///
/// Every directive in an unconsumed user comment is consumed exactly once; one
/// that doesn't approve the current phase is recorded as a note (stale,
/// premature, or retired `/proceed`) instead of firing.
fn advance_phase(
    issue_state: &mut state::IssueState,
    issue_comments: &[models::Comment],
    pr_comments: Option<&[models::Comment]>,
) -> AdvanceOutcome {
    // Merge both threads chronologically, so successive approvals posted
    // together fire in the order they were written.
    let mut tagged: Vec<(&models::Comment, &'static str)> = issue_comments
        .iter()
        .map(|comment| (comment, "issue"))
        .chain(
            pr_comments
                .into_iter()
                .flatten()
                .map(|comment| (comment, "PR")),
        )
        .collect();
    tagged.sort_by(|a, b| a.0.created_at.cmp(&b.0.created_at));

    let mut outcome = AdvanceOutcome::default();
    for (comment, source) in tagged {
        // Only the user's comments are directives; skip Claude/ghwf-authored
        // ones (status updates mention approval commands in their prose).
        if render::extract_marker(&comment.body).is_some() {
            continue;
        }
        if issue_state.consumed_directives.contains(&comment.id) {
            continue;
        }
        let Some(directive) = state::parse_directive(&comment.body) else {
            continue;
        };
        // Consume the directive whatever happens next, so it never re-fires.
        issue_state.consumed_directives.insert(comment.id);

        let phase = issue_state.phase;
        let kind = match directive.approves() {
            // Approves the current phase: advance.
            Some(approved) if approved == phase => {
                let to = phase.next().expect("approvable phases have a successor");
                issue_state.phase = to;
                outcome.transitions.push(render::Transition {
                    from: phase,
                    to,
                    command: directive.command(),
                    by: comment.user.login.clone(),
                });
                continue;
            }
            Some(approved) if approved < phase => render::NoteKind::Stale,
            Some(_) => render::NoteKind::Premature,
            None => render::NoteKind::Retired,
        };
        outcome.notes.push(render::DirectiveNote {
            kind,
            command: directive.command(),
            by: comment.user.login.clone(),
            source,
            phase_at: phase,
        });
    }
    outcome
}

fn create_issue_comment(issue: &str) -> Result<()> {
    let mut user_body = String::new();
    std::io::stdin()
        .read_to_string(&mut user_body)
        .map_err(anyhow::Error::from)?;
    if user_body.trim().is_empty() {
        bail!("no comment body provided on stdin");
    }

    // Tag the comment with the authoring session when running under Claude Code.
    let token = match std::env::var(store::SESSION_ID_ENV) {
        Ok(session_id) if !session_id.is_empty() => Some(store::session_token(&session_id)?),
        _ => None,
    };

    let repo_ctx = github::config_repo()?;
    let body = render::build_comment_body(&user_body, token.as_deref());
    let comment = github::post_issue_comment(issue, &body, repo_ctx.as_ref())?;

    // Remember the post for feed-lag self-calibration in `wait`, best-effort.
    if let Err(err) = record_last_posted(issue, &comment, repo_ctx.as_ref()) {
        eprintln!("warning: failed to record the post for wait calibration: {err:#}");
    }

    println!("{}", render::comment_json(&comment)?);
    Ok(())
}

/// Record a ghwf-authored comment as the workflow issue's `last_posted`. The
/// thread argument may name the issue itself or its PR; the PR case maps back
/// to the issue whose prep state records that PR number.
fn record_last_posted(
    issue: &str,
    comment: &models::Comment,
    repo_ctx: Option<&github::RepoRef>,
) -> Result<()> {
    let (owner, repo, number) = github::resolve_issue_ref(issue, repo_ctx)?;
    let Some((number, mut state)) = state::find_workflow_issue(&owner, &repo, number)? else {
        // No workflow has engaged this thread yet; nothing to calibrate.
        return Ok(());
    };
    state.last_posted = Some(state::PostedRef {
        id: comment.id,
        created_at: comment.created_at.clone(),
    });
    state::save(&owner, &repo, number, &state)
}

#[cfg(test)]
mod tests {
    use super::advance_phase;
    use crate::models::{Comment, User};
    use crate::render::NoteKind;
    use crate::state::{IssueState, Phase};

    fn comment(id: u64, body: &str, created_at: &str) -> Comment {
        Comment {
            id,
            user: User {
                login: "user".to_string(),
            },
            body: body.to_string(),
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            html_url: format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
            author_association: "OWNER".to_string(),
        }
    }

    fn state_in(phase: Phase) -> IssueState {
        IssueState {
            phase,
            ..Default::default()
        }
    }

    #[test]
    fn matching_directive_advances_and_consumes() {
        let mut state = state_in(Phase::PrePlan);
        let comments = [comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrepAndPlan);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(outcome.transitions[0].command, "/approve-pre-plan");
        assert!(state.consumed_directives.contains(&1));
        assert!(outcome.notes.is_empty());
    }

    #[test]
    fn pr_thread_directive_advances() {
        let mut state = state_in(Phase::PrepAndPlan);
        let pr = [comment(2, "/approve-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &[], Some(&pr));
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(outcome.transitions[0].by, "user");
    }

    #[test]
    fn duplicate_across_threads_is_stale() {
        let mut state = state_in(Phase::PrepAndPlan);
        let issue = [comment(1, "/approve-plan", "2026-01-01T00:00:00Z")];
        let pr = [comment(2, "/approve-plan", "2026-01-01T00:01:00Z")];
        let outcome = advance_phase(&mut state, &issue, Some(&pr));
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(outcome.notes.len(), 1);
        assert!(matches!(outcome.notes[0].kind, NoteKind::Stale));
        assert_eq!(outcome.notes[0].source, "PR");
        assert!(state.consumed_directives.contains(&2));
    }

    #[test]
    fn premature_directive_is_noted_not_fired() {
        let mut state = state_in(Phase::PrePlan);
        let comments = [comment(
            1,
            "/approve-implementation",
            "2026-01-01T00:00:00Z",
        )];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(matches!(outcome.notes[0].kind, NoteKind::Premature));
        // Consumed: it must never fire later once the phase catches up.
        assert!(state.consumed_directives.contains(&1));
    }

    #[test]
    fn retired_proceed_is_noted_not_fired() {
        let mut state = state_in(Phase::PrePlan);
        let comments = [comment(1, "/proceed", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(matches!(outcome.notes[0].kind, NoteKind::Retired));
        assert!(state.consumed_directives.contains(&1));
    }

    #[test]
    fn status_comments_are_not_directives() {
        let mut state = state_in(Phase::PrePlan);
        // A status comment may mention an approval command at line start.
        let body = crate::render::build_status_comment_body("/approve-pre-plan");
        let comments = [comment(1, &body, "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(outcome.notes.is_empty());
        assert!(!state.consumed_directives.contains(&1));
    }

    #[test]
    fn collect_new_comments_diffs_against_seen_map() {
        use super::collect_new_comments;
        use std::collections::BTreeMap;

        let comments = [
            comment(1, "already seen", "2026-01-01T00:00:00Z"),
            comment(2, "now edited", "2026-01-01T00:01:00Z"),
            comment(3, "brand new", "2026-01-01T00:02:00Z"),
            comment(
                4,
                &crate::render::build_status_comment_body("machinery"),
                "2026-01-01T00:03:00Z",
            ),
            comment(
                5,
                &crate::render::build_comment_body("mine", Some("mine")),
                "2026-01-01T00:04:00Z",
            ),
        ];

        let seen: BTreeMap<u64, String> = [
            (1, crate::store::content_hash("already seen")),
            (2, crate::store::content_hash("original")),
        ]
        .into();

        let new = collect_new_comments(&comments, &seen, "mine");
        assert_eq!(new.len(), 2);
        assert_eq!(new[0].comment.id, 2);
        assert!(new[0].updated);
        assert_eq!(new[1].comment.id, 3);
        assert!(!new[1].updated);
    }

    #[test]
    fn digest_hides_status_always_and_own_session_comments_only() {
        use crate::render::hidden_from_digest;
        let status = crate::render::build_status_comment_body("update");
        assert!(hidden_from_digest(&status, Some("mine")));
        let mine = crate::render::build_comment_body("hi", Some("mine"));
        let theirs = crate::render::build_comment_body("hi", Some("theirs"));
        assert!(hidden_from_digest(&mine, Some("mine")));
        assert!(!hidden_from_digest(&theirs, Some("mine")));
        assert!(!hidden_from_digest("plain user comment", Some("mine")));
        // Outside a Claude session only status comments hide.
        assert!(hidden_from_digest(&status, None));
        assert!(!hidden_from_digest(&mine, None));
    }

    #[test]
    fn consumed_and_claude_comments_are_skipped() {
        let mut state = state_in(Phase::PrePlan);
        state.consumed_directives.insert(1);
        let claude_body = crate::render::build_comment_body("/approve-pre-plan", Some("tok"));
        let comments = [
            comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z"),
            comment(2, &claude_body, "2026-01-01T00:01:00Z"),
        ];
        let outcome = advance_phase(&mut state, &comments, None);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(outcome.notes.is_empty());
        assert!(!state.consumed_directives.contains(&2));
    }

    #[test]
    fn successive_approvals_advance_twice_in_chronological_order() {
        let mut state = state_in(Phase::PrePlan);
        // The earlier approval arrives via the PR slice and the later via the
        // issue slice: the chronological merge must fire pre-plan's first.
        let issue = [comment(2, "/approve-plan", "2026-01-01T00:01:00Z")];
        let pr = [comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &issue, Some(&pr));
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 2);
        assert_eq!(outcome.transitions[0].command, "/approve-pre-plan");
        assert_eq!(outcome.transitions[1].command, "/approve-plan");
        assert!(outcome.notes.is_empty());
    }
}
