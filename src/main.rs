mod clone;
mod collect_garbage;
mod config;
mod git;
mod github;
mod implement;
mod init;
mod install;
mod labels;
mod launch;
mod models;
mod next;
mod prep;
mod render;
mod seen;
mod state;
mod stop_hook;
mod store;
mod wait;
mod worktree;

use std::io::Read;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
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
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred from $GHWF_ISSUE (set by the
        /// outside-Claude launcher) or the worktree containing the current
        /// directory.
        issue: Option<String>,
        /// Work without a dedicated branch/worktree/PR (just write the plan file).
        #[arg(long)]
        no_branch: bool,
    },
    /// Pick the next issue to work on, claim it, and start work on it, as
    /// `work-on` would.
    ///
    /// Picks from the repo's open issues: ones assigned to you first, then by
    /// the configured `priority_labels` (earlier in the list wins), then the
    /// lowest issue number. Issues assigned to someone else or already started
    /// by a ghwf session are passed over. The pick is claimed (reserved against
    /// concurrent runs) and assigned to you on GitHub.
    ///
    /// With `--wait`, block until an eligible issue appears rather than erroring
    /// when there is none — run one per terminal as a pool of single-use
    /// workers that each grab and start the next issue to come along.
    Next {
        /// Work without a dedicated branch/worktree/PR (just write the plan file).
        #[arg(long)]
        no_branch: bool,
        /// Block until an eligible issue appears, claim it, then start work —
        /// run one per terminal as a pool of single-use workers.
        #[arg(long)]
        wait: bool,
        /// With --wait: give up after this many seconds, exiting with code 2.
        /// Omit to wait indefinitely.
        #[arg(long, requires = "wait")]
        timeout: Option<u64>,
    },
    /// Clone a GitHub repo into ghwf's preferred layout: a container
    /// directory holding the bare repo (as `<name>.git`), a generated
    /// `ghwf.toml`, and an empty worktrees directory.
    Clone {
        /// The repo to clone: `owner/repo` or a full GitHub URL (HTTPS or
        /// SSH).
        repo: String,
        /// Directory to create (the container). Defaults to the repo name
        /// under the current directory.
        directory: Option<PathBuf>,
        /// Borrow objects from this existing local clone instead of fetching
        /// them from the network (via `git clone --reference`); the new repo
        /// is dissociated from it afterwards, so it can be deleted safely.
        #[arg(long)]
        reference: Option<PathBuf>,
    },
    /// Delete branches and worktrees for PRs that have already been merged.
    ///
    /// A branch (local and remote) is collected only when its tip is exactly
    /// what got merged into the default branch; its worktree only when the
    /// working tree is clean. Anything suspicious is warned about and left
    /// alone.
    #[command(alias = "gc")]
    CollectGarbage {
        /// Report what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Post a comment to an issue (or PR), reading the body from stdin.
    ///
    /// The comment is prefixed with a "Claude says" header and tagged with hidden
    /// metadata identifying the authoring Claude session.
    CreateIssueComment {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
    },
    /// Post Claude's hand-off comment, reading the body from stdin, and flip
    /// the workflow to waiting-on-user.
    ///
    /// ghwf appends the phase-appropriate next-step prompt (the approval
    /// command, or the ready-for-review button in the implement phase) — the
    /// body should not include one.
    HandOff {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
    },
    /// Configure ghwf: subcommands that create or extend `ghwf.toml`.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Print the absolute path of the worktree recorded for an issue.
    WorktreePath {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
    },
    /// Install (or update) ghwf's user-global Claude Code integration: the
    /// `/work-on` skill and the Stop hook that keeps a session working an
    /// issue until its workflow is done.
    Install {
        /// Overwrite an existing skill file even when ghwf didn't write it.
        #[arg(long)]
        force: bool,
    },
    /// The Stop-hook entry point Claude Code invokes (configured by
    /// `ghwf install`); not for humans.
    #[command(hide = true)]
    ClaudeStopHook,
    /// Block until new activity appears on an issue (or its PR), or the timeout
    /// elapses.
    ///
    /// Exits 0 when activity is detected (run `ghwf work-on` to process it),
    /// 2 on timeout (nothing new — run `wait` again), and 1 on error.
    Wait {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
        /// Give up after this many seconds, with exit code 2.
        #[arg(long, default_value_t = 540)]
        timeout: u64,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Interactively create or extend `ghwf.toml`: the essentials when
    /// missing, then optional extras.
    Init,
    /// Set up workflow status labels: create them in the GitHub repo and add
    /// a `[labels]` section to `ghwf.toml`.
    Labels,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::WorkOn { issue, no_branch } => work_on(&resolve_issue_arg(issue)?, no_branch),
        Commands::Next {
            no_branch,
            wait,
            timeout,
        } => {
            let number = if wait {
                next::wait_for_pick(timeout)?
            } else {
                next::pick()?
            };
            work_on(&number.to_string(), no_branch)
        }
        Commands::Clone {
            repo,
            directory,
            reference,
        } => clone::run(&repo, directory.as_deref(), reference.as_deref()),
        Commands::CollectGarbage { dry_run } => collect_garbage::run(dry_run),
        Commands::CreateIssueComment { issue } => create_issue_comment(&resolve_issue_arg(issue)?),
        Commands::HandOff { issue } => hand_off(&resolve_issue_arg(issue)?),
        Commands::Config { command } => match command {
            ConfigCommands::Init => init::run(),
            ConfigCommands::Labels => labels::configure(),
        },
        Commands::WorktreePath { issue } => worktree_path(&resolve_issue_arg(issue)?),
        Commands::Install { force } => install::run(force),
        Commands::ClaudeStopHook => stop_hook::run(),
        Commands::Wait { issue, timeout } => wait::run(&resolve_issue_arg(issue)?, timeout),
    }
}

/// Resolve the issue a command operates on: the explicit argument when given,
/// else $GHWF_ISSUE (set on the session by the outside-Claude launcher), else
/// the issue whose recorded worktree contains the current directory. An
/// explicit argument always wins — the fallbacks are defaults, not locks.
fn resolve_issue_arg(arg: Option<String>) -> Result<String> {
    if let Some(arg) = arg {
        return Ok(arg);
    }
    if let Ok(value) = std::env::var(store::ISSUE_ENV) {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    let issues_root = store::data_dir()?.join("issues");
    let cwd = std::env::current_dir().context("failed to determine the current directory")?;
    if let Some((owner, repo, number)) = state::find_issue_for_dir(&issues_root, &cwd)? {
        return Ok(format!("https://github.com/{owner}/{repo}/issues/{number}"));
    }
    bail!(
        "no issue given and none could be inferred (${} is unset and the current \
         directory is not inside a recorded worktree); pass an issue number or URL, \
         e.g. `ghwf work-on 13`.",
        store::ISSUE_ENV
    );
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

    // Record whether the workflow is finished, for the Stop hook: a closed
    // issue means a bound session may end instead of being nudged to keep
    // waiting.
    issue_state.issue_closed = issue_data.state != "open";

    // Approval directives are honoured from the issue thread and, once a PR
    // exists, its conversation thread too — fetched now, before directive
    // processing, and reused for the digest below.
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let early_pr_comments = match pr_number {
        Some(pr) => Some(github::fetch_comments(&pr.to_string(), repo_ctx.as_ref())?),
        None => None,
    };
    // Recompute how (or whether) the PR left the open state, remembering the
    // previous value so a fresh conclusion can be announced exactly once.
    // Recomputing rather than latching lets a closed-then-reopened PR resume
    // the workflow. The PR object is kept: its draft flag drives the
    // implement → review transition below.
    let pr_object = match pr_number {
        Some(pr) => Some(github::fetch_pr(&owner, &repo, pr)?),
        None => None,
    };
    let prior_pr_outcome = issue_state.pr_outcome;
    issue_state.pr_outcome = pr_object.as_ref().and_then(state::pr_outcome);
    let new_conclusion = (issue_state.pr_outcome != prior_pr_outcome)
        .then_some(issue_state.pr_outcome)
        .flatten();
    // 👍 reactions on ghwf prompt comments are approvals too; fetch the
    // reaction details for prompts whose rollup shows any.
    let mut prompt_thumbs = collect_prompt_thumbs(&owner, &repo, &issue_comments, "issue")?;
    if let Some(comments) = early_pr_comments.as_deref() {
        prompt_thumbs.extend(collect_prompt_thumbs(&owner, &repo, comments, "PR")?);
    }
    let mut outcome = advance_phase(
        &mut issue_state,
        &issue_comments,
        early_pr_comments.as_deref(),
        &prompt_thumbs,
    );
    // The implement phase has no approval command: the user marking the draft
    // PR ready for review is what advances it.
    advance_on_pr_ready(&mut issue_state, pr_object.as_ref(), &mut outcome);

    // The phase-specific banner body. Prep-and-plan does real work here (and
    // hard-errors if it needs a config that's missing); implement/review are light.
    // A concluded PR replaces the phase body wholesale: the workflow is over,
    // so no phase work runs (in particular review's draft→ready flip, which
    // would fail against a merged or closed PR).
    let phase = issue_state.phase;
    // The project's PR instructions file, when one exists; the implement and
    // review banners point Claude at it.
    let pr_instructions = config::find()?
        .map(|located| located.pr_instructions_path())
        .filter(|path| path.is_file());
    let body = match issue_state.pr_outcome {
        Some(pr_outcome) => render::concluded_body(
            pr_outcome,
            pr_number
                .map(|pr| format!("https://github.com/{owner}/{repo}/pull/{pr}"))
                .as_deref(),
            number,
        ),
        None => match phase {
            state::Phase::PrePlan => render::pre_plan_body(number),
            state::Phase::PrepAndPlan => {
                // The worktree-creation stretch is ghwf's own slow work; let
                // the labels say so while it runs. The end-of-run settle
                // below hands the ball back to Claude.
                let needs_worktree = !no_branch
                    && issue_state
                        .prep
                        .as_ref()
                        .is_none_or(|p| !p.no_branch && p.branch.is_none());
                if needs_worktree {
                    issue_state.attention = state::Attention::WaitingOnGhwf;
                    labels::sync(&owner, &repo, number, pr_number, &mut issue_state);
                }
                prep::run(
                    &issue_data,
                    &owner,
                    &repo,
                    number,
                    no_branch,
                    &mut issue_state,
                )?
            }
            state::Phase::Implement => implement::run(
                &issue_data,
                &owner,
                &repo,
                number,
                &issue_state,
                pr_instructions.as_deref(),
            )?,
            state::Phase::Review => implement::review(
                &owner,
                &repo,
                number,
                &issue_state,
                pr_instructions.as_deref(),
            ),
        },
    };

    // Detect a merge conflict with the moved-on base, for the implement and
    // review phases of an open PR. Detection is local (a git fetch plus an
    // in-memory merge-tree, no GitHub API), and surfaced by leading the banner
    // with a resolve-it-now instruction. The notice is never posted to a
    // thread and clears itself once Claude pushes the merge.
    let conflict_base = match (issue_state.pr_outcome, phase) {
        (None, state::Phase::Implement | state::Phase::Review) => issue_state
            .prep
            .as_ref()
            .and_then(implement::detect_conflict),
        _ => None,
    };
    let body = match &conflict_base {
        Some(base) => format!("{}\n\n{body}", render::conflict_notice(base, number)),
        None => body,
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
        new_conclusion,
    );
    let status_posted = status.is_some();
    // The status comments posted to each thread this run, kept so the
    // reaction watches below can target them — a just-posted prompt is the
    // likeliest 👍 target.
    let mut posted_issue: Option<models::Comment> = None;
    let mut posted_pr: Option<models::Comment> = None;
    if let Some(text) = status {
        let status_body = render::build_status_comment_body(&text);
        match pr_number {
            // No PR yet: the issue is the only thread.
            None => {
                posted_issue = post_status(
                    &number.to_string(),
                    &status_body,
                    repo_ctx.as_ref(),
                    "issue",
                );
            }
            // Full update on the phase's primary thread; the other thread
            // gets a one-line stub linking to it — or the full body when the
            // primary post failed, so nothing is lost.
            Some(pr) => {
                let primary_is_pr = render::status_primary_is_pr(phase);
                let (primary, primary_noun, secondary, secondary_noun) = if primary_is_pr {
                    (pr.to_string(), "PR", number.to_string(), "issue")
                } else {
                    (number.to_string(), "issue", pr.to_string(), "PR")
                };
                let full = post_status(&primary, &status_body, repo_ctx.as_ref(), primary_noun);
                let secondary_body = match &full {
                    Some(comment) => {
                        render::build_status_comment_body(&render::render_status_stub(
                            &outcome.transitions,
                            primary_noun,
                            &comment.html_url,
                        ))
                    }
                    None => status_body,
                };
                let stub = post_status(
                    &secondary,
                    &secondary_body,
                    repo_ctx.as_ref(),
                    secondary_noun,
                );
                // Stubs never mention an approval command, so only the full
                // update can become a thread's reaction watch below.
                (posted_issue, posted_pr) = if primary_is_pr {
                    (stub, full)
                } else {
                    (full, stub)
                };
            }
        }
        // Remember the newest own post for feed-lag self-calibration in
        // `wait`. The secondary-thread post lands last, so compare timestamps
        // rather than assuming a thread.
        let newest = match (posted_issue.as_ref(), posted_pr.as_ref()) {
            (Some(a), Some(b)) => Some(if a.created_at >= b.created_at { a } else { b }),
            (a, b) => a.or(b),
        };
        if let Some(comment) = newest {
            issue_state.last_posted = Some(state::PostedRef {
                id: comment.id,
                created_at: comment.created_at.clone(),
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
        // A PR opened during this run's phase body has no fetched object yet;
        // its baseline starts on the next run (it was just created as a
        // draft, so no flip can be missed).
        pr_draft: pr_object.as_ref().map(|pr| pr.draft),
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

    // Watch the latest approval prompt per thread for 👍 reactions — `wait`
    // is otherwise blind to them (a reaction bumps neither the comment's
    // `updated_at` nor the events feed). A status comment posted this run is
    // the newest prompt on its thread.
    if let Some(watch) = latest_prompt_watch(&issue_comments, posted_issue.as_ref(), &prompt_thumbs)
    {
        wait_state
            .reaction_watches
            .insert("issue".to_string(), watch);
    }
    if let Some(watch) = latest_prompt_watch(
        pr_comments.as_deref().unwrap_or(&[]),
        posted_pr.as_ref(),
        &prompt_thumbs,
    ) {
        wait_state.reaction_watches.insert("pr".to_string(), watch);
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

    // Anything new arriving — a directive (fired or noted), the issue body
    // changing, or fresh digest content — resets the Stop hook's
    // consecutive-nudge counter, so its cap only counts stops where nothing
    // had changed (see stop_hook.rs).
    let activity = !outcome.transitions.is_empty()
        || !outcome.notes.is_empty()
        || body_changed
        || !new_issue.is_empty()
        || !new_pr.is_empty()
        || !new_review.is_empty()
        // A standing conflict keeps the ball with Claude until it's resolved.
        || conflict_base.is_some();
    if activity {
        issue_state.stop_nudges = 0;
    }

    // Settle the attention axis. Review leaves nothing for Claude; otherwise
    // activity puts the ball in Claude's court (it now has something to act
    // on). A quiet re-run changes nothing — it must not steal the ball back
    // from the user after a hand-off. A concluded workflow waits on nobody;
    // the label sync below drops the attention label in that case.
    if issue_state.pr_outcome.is_none() {
        if issue_state.phase == state::Phase::Review {
            issue_state.attention = state::Attention::WaitingOnUser;
        } else if activity {
            issue_state.attention = state::Attention::WaitingOnClaude;
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
    // Mirror the workflow state onto GitHub labels, best-effort, when
    // configured. Mutates `labels_synced`, so it runs before the save.
    labels::sync(&owner, &repo, number, pr_number, &mut issue_state);
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

/// The reaction watch for one thread: its latest ghwf-authored approval
/// prompt, baselined with the 👍 reaction ids already fetched this run (a
/// just-posted prompt has none).
fn latest_prompt_watch(
    comments: &[models::Comment],
    posted: Option<&models::Comment>,
    prompt_thumbs: &[PromptThumbs],
) -> Option<state::ReactionWatch> {
    let prompt = comments
        .iter()
        .chain(posted)
        .filter(|comment| render::extract_marker(&comment.body).is_some())
        .filter(|comment| state::parse_prompted_directive(&comment.body).is_some())
        .max_by(|a, b| a.created_at.cmp(&b.created_at))?;
    let plus_one_ids = prompt_thumbs
        .iter()
        .find(|p| p.comment_id == prompt.id)
        .map(|p| {
            p.reactions
                .iter()
                .filter(|r| r.is_thumbs_up())
                .map(|r| r.id)
                .collect()
        })
        .unwrap_or_default();
    Some(state::ReactionWatch {
        comment_id: prompt.id,
        plus_one_ids,
    })
}

/// Whether this phase requires Claude to be inside the issue's worktree.
///
/// Only branch-mode phases that operate on a created worktree qualify:
/// prep-and-plan (Claude must write the plan there) and implement (Claude codes
/// there). Pre-plan, review, `--no-branch` work, and a concluded workflow
/// (merged or closed PR) don't need it.
fn needs_worktree_guard(phase: state::Phase, issue_state: &state::IssueState) -> bool {
    if issue_state.pr_outcome.is_some() {
        return false;
    }
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

/// A ghwf-authored approval prompt comment with the reactions currently on it,
/// prefetched so `advance_phase` stays network-free.
struct PromptThumbs {
    comment_id: u64,
    /// The approval the prompt's body asks for.
    directive: state::Directive,
    /// Which conversation thread the prompt is on ("issue" / "PR").
    source: &'static str,
    reactions: Vec<models::Reaction>,
}

/// Collect one thread's approval prompts that carry 👍 reactions: ghwf-marked
/// comments whose body prompts for an approval and whose reaction rollup shows
/// at least one 👍 — only those warrant the per-comment detail fetch.
fn collect_prompt_thumbs(
    owner: &str,
    repo: &str,
    comments: &[models::Comment],
    source: &'static str,
) -> Result<Vec<PromptThumbs>> {
    let mut prompts = Vec::new();
    for comment in comments {
        if render::extract_marker(&comment.body).is_none() {
            continue;
        }
        let Some(directive) = state::parse_prompted_directive(&comment.body) else {
            continue;
        };
        if comment.reactions.as_ref().is_none_or(|r| r.plus_one == 0) {
            continue;
        }
        prompts.push(PromptThumbs {
            comment_id: comment.id,
            directive,
            source,
            reactions: github::fetch_comment_reactions(owner, repo, comment.id)?,
        });
    }
    Ok(prompts)
}

/// One approval event awaiting classification: a typed directive comment, or
/// a 👍 reaction on a ghwf prompt comment.
enum ApprovalEvent<'a> {
    Directive {
        comment: &'a models::Comment,
        source: &'static str,
    },
    Thumb {
        reaction: &'a models::Reaction,
        directive: state::Directive,
        source: &'static str,
    },
}

impl ApprovalEvent<'_> {
    /// When the event happened, for the chronological merge.
    fn created_at(&self) -> &str {
        match self {
            ApprovalEvent::Directive { comment, .. } => &comment.created_at,
            ApprovalEvent::Thumb { reaction, .. } => &reaction.created_at,
        }
    }
}

/// Process any new approval events — typed directives on the issue and PR
/// conversation threads, and 👍 reactions on ghwf prompt comments — advancing
/// the issue's phase in `issue_state`.
///
/// Every event is consumed exactly once (comment id or reaction id); one that
/// doesn't approve the current phase is recorded as a note (stale, premature,
/// or retired `/proceed`) instead of firing.
fn advance_phase(
    issue_state: &mut state::IssueState,
    issue_comments: &[models::Comment],
    pr_comments: Option<&[models::Comment]>,
    prompt_thumbs: &[PromptThumbs],
) -> AdvanceOutcome {
    // Merge both threads and both event kinds chronologically, so successive
    // approvals fire in the order they were given.
    let mut events: Vec<ApprovalEvent> = issue_comments
        .iter()
        .map(|comment| ApprovalEvent::Directive {
            comment,
            source: "issue",
        })
        .chain(
            pr_comments
                .into_iter()
                .flatten()
                .map(|comment| ApprovalEvent::Directive {
                    comment,
                    source: "PR",
                }),
        )
        .chain(prompt_thumbs.iter().flat_map(|prompt| {
            prompt
                .reactions
                .iter()
                .filter(|reaction| reaction.is_thumbs_up())
                .map(|reaction| ApprovalEvent::Thumb {
                    reaction,
                    directive: prompt.directive,
                    source: prompt.source,
                })
        }))
        .collect();
    events.sort_by(|a, b| a.created_at().cmp(b.created_at()));

    let mut outcome = AdvanceOutcome::default();
    for event in events {
        let (directive, by, source, via_reaction) = match event {
            ApprovalEvent::Directive { comment, source } => {
                // Only the user's comments are directives; skip Claude/ghwf-
                // authored ones (status updates mention approval commands in
                // their prose).
                if render::extract_marker(&comment.body).is_some() {
                    continue;
                }
                if issue_state.consumed_directives.contains(&comment.id) {
                    continue;
                }
                let Some(directive) = state::parse_directive(&comment.body) else {
                    continue;
                };
                // Consume the directive whatever happens next, so it never
                // re-fires.
                issue_state.consumed_directives.insert(comment.id);
                (directive, &comment.user.login, source, false)
            }
            ApprovalEvent::Thumb {
                reaction,
                directive,
                source,
            } => {
                if issue_state.consumed_reactions.contains(&reaction.id) {
                    continue;
                }
                issue_state.consumed_reactions.insert(reaction.id);
                (directive, &reaction.user.login, source, true)
            }
        };

        let phase = issue_state.phase;
        let kind = match directive.approves() {
            // Approves the current phase: advance.
            Some(approved) if approved == phase => {
                let to = phase.next().expect("approvable phases have a successor");
                issue_state.phase = to;
                outcome.transitions.push(render::Transition {
                    from: phase,
                    to,
                    trigger: render::Trigger::Directive {
                        command: directive.command(),
                        by: by.clone(),
                        via_reaction,
                    },
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
            by: by.clone(),
            source,
            phase_at: phase,
            via_reaction,
        });
    }
    outcome
}

/// Advance implement → review when the user has marked the draft PR ready for
/// review. Only an open PR counts: a concluded workflow runs no phase work,
/// and `!draft` on a merged or closed PR means nothing.
fn advance_on_pr_ready(
    issue_state: &mut state::IssueState,
    pr: Option<&models::PullRequest>,
    outcome: &mut AdvanceOutcome,
) {
    if issue_state.phase != state::Phase::Implement || issue_state.pr_outcome.is_some() {
        return;
    }
    let Some(pr) = pr else {
        return;
    };
    if pr.draft {
        return;
    }
    issue_state.phase = state::Phase::Review;
    outcome.transitions.push(render::Transition {
        from: state::Phase::Implement,
        to: state::Phase::Review,
        trigger: render::Trigger::PrReady,
    });
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

/// Post Claude's hand-off comment (body from stdin) with the current phase's
/// next-step prompt appended by ghwf, flip the workflow to waiting-on-user,
/// and sync labels. The prompt makes the hand-off the thread's 👍 target
/// where an approval command applies.
fn hand_off(issue: &str) -> Result<()> {
    let mut user_body = String::new();
    std::io::stdin()
        .read_to_string(&mut user_body)
        .map_err(anyhow::Error::from)?;
    if user_body.trim().is_empty() {
        bail!("no comment body provided on stdin");
    }

    let repo_ctx = github::config_repo()?;
    let (owner, repo, thread_number) = github::resolve_issue_ref(issue, repo_ctx.as_ref())?;
    // The argument may name the PR thread; map back to the workflow issue.
    let Some((number, mut issue_state)) = state::find_workflow_issue(&owner, &repo, thread_number)?
    else {
        bail!(
            "no workflow state recorded for issue #{thread_number}; run \
             `ghwf work-on {thread_number}` first."
        );
    };

    if issue_state.pr_outcome.is_some() {
        bail!(
            "the workflow for issue #{number} has concluded (its PR was merged or \
             closed); there is nothing to hand off."
        );
    }
    let phase = issue_state.phase;
    let no_branch = issue_state.prep.as_ref().is_some_and(|p| p.no_branch);
    let Some(prompt) = render::hand_off_prompt(phase, no_branch) else {
        bail!(
            "the workflow for issue #{number} is in the review phase — the PR is \
             already with the user; there is nothing to hand off."
        );
    };

    // Tag the comment with the authoring session when running under Claude Code.
    let token = match std::env::var(store::SESSION_ID_ENV) {
        Ok(session_id) if !session_id.is_empty() => Some(store::session_token(&session_id)?),
        _ => None,
    };
    let full_body = render::build_comment_body(
        &format!("{}\n\n{prompt}", user_body.trim()),
        token.as_deref(),
    );

    // Full comment on the phase's primary thread; when a PR exists, the other
    // thread gets a one-line stub linking to it, best-effort.
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let primary_is_pr = pr_number.is_some() && render::status_primary_is_pr(phase);
    let primary = if primary_is_pr {
        pr_number.expect("primary_is_pr requires a PR")
    } else {
        number
    };
    let comment = github::post_issue_comment(&primary.to_string(), &full_body, repo_ctx.as_ref())?;
    if let Some(pr) = pr_number {
        let (secondary, secondary_noun, primary_noun) = if primary_is_pr {
            (number, "issue", "PR")
        } else {
            (pr, "PR", "issue")
        };
        let stub = render::build_status_comment_body(&format!(
            "Hand-off posted on the {primary_noun}: {}",
            comment.html_url
        ));
        if let Err(err) =
            github::post_issue_comment(&secondary.to_string(), &stub, repo_ctx.as_ref())
        {
            eprintln!("warning: failed to post the hand-off stub to the {secondary_noun}: {err:#}");
        }
    }

    // The ball is with the user now. Remember the post for feed-lag
    // self-calibration, and watch it for 👍s when its prompt maps one.
    issue_state.attention = state::Attention::WaitingOnUser;
    issue_state.last_posted = Some(state::PostedRef {
        id: comment.id,
        created_at: comment.created_at.clone(),
    });
    if state::parse_prompted_directive(&full_body).is_some() {
        if let Some(wait) = issue_state.wait.as_mut() {
            let thread = if primary_is_pr { "pr" } else { "issue" };
            wait.reaction_watches.insert(
                thread.to_string(),
                state::ReactionWatch {
                    comment_id: comment.id,
                    plus_one_ids: Default::default(),
                },
            );
            // The watch endpoint's URL changed with it; drop the stale ETag.
            wait.etags.remove(&format!("reactions_{thread}"));
        }
    }
    labels::sync(&owner, &repo, number, pr_number, &mut issue_state);
    state::save(&owner, &repo, number, &issue_state)?;

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
    let (owner, repo, thread_number) = github::resolve_issue_ref(issue, repo_ctx)?;
    let Some((number, mut state)) = state::find_workflow_issue(&owner, &repo, thread_number)?
    else {
        // No workflow has engaged this thread yet; nothing to calibrate.
        return Ok(());
    };
    state.last_posted = Some(state::PostedRef {
        id: comment.id,
        created_at: comment.created_at.clone(),
    });
    // When the post prompts for an approval it becomes its thread's reaction
    // watch: it is now the newest prompt there, and the likeliest 👍 target.
    if state::parse_prompted_directive(&comment.body).is_some() {
        if let Some(wait) = state.wait.as_mut() {
            let thread = if thread_number == number {
                "issue"
            } else {
                "pr"
            };
            wait.reaction_watches.insert(
                thread.to_string(),
                state::ReactionWatch {
                    comment_id: comment.id,
                    plus_one_ids: Default::default(),
                },
            );
            // The watch endpoint's URL changed with it; drop the stale ETag.
            wait.etags.remove(&format!("reactions_{thread}"));
        }
    }
    state::save(&owner, &repo, number, &state)
}

#[cfg(test)]
mod tests {
    use super::{
        advance_on_pr_ready, advance_phase, latest_prompt_watch, needs_worktree_guard,
        AdvanceOutcome, PromptThumbs,
    };
    use crate::models::{Comment, PullRequest, Reaction, User};
    use crate::render::{NoteKind, Transition, Trigger};
    use crate::state::{Directive, IssueState, Phase, PrOutcome, PrepState};

    /// Unpack a directive-fired transition, panicking on a PR-ready one.
    fn directive_parts(transition: &Transition) -> (&'static str, &str, bool) {
        match &transition.trigger {
            Trigger::Directive {
                command,
                by,
                via_reaction,
            } => (command, by.as_str(), *via_reaction),
            Trigger::PrReady => panic!("expected a directive trigger"),
        }
    }

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
            reactions: None,
        }
    }

    fn reaction(id: u64, content: &str, created_at: &str) -> Reaction {
        Reaction {
            id,
            user: User {
                login: "reactor".to_string(),
            },
            content: content.to_string(),
            created_at: created_at.to_string(),
        }
    }

    fn thumbs(comment_id: u64, directive: Directive, reactions: Vec<Reaction>) -> PromptThumbs {
        PromptThumbs {
            comment_id,
            directive,
            source: "issue",
            reactions,
        }
    }

    fn state_in(phase: Phase) -> IssueState {
        IssueState {
            phase,
            ..Default::default()
        }
    }

    #[test]
    fn worktree_guard_skipped_once_pr_concluded() {
        let mut state = IssueState {
            phase: Phase::Implement,
            prep: Some(PrepState {
                worktree_path: Some("/wt".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(needs_worktree_guard(Phase::Implement, &state));
        state.pr_outcome = Some(PrOutcome::Merged);
        assert!(!needs_worktree_guard(Phase::Implement, &state));
    }

    #[test]
    fn matching_directive_advances_and_consumes() {
        let mut state = state_in(Phase::PrePlan);
        let comments = [comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None, &[]);
        assert_eq!(state.phase, Phase::PrepAndPlan);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(
            directive_parts(&outcome.transitions[0]).0,
            "/approve-pre-plan"
        );
        assert!(state.consumed_directives.contains(&1));
        assert!(outcome.notes.is_empty());
    }

    #[test]
    fn pr_thread_directive_advances() {
        let mut state = state_in(Phase::PrepAndPlan);
        let pr = [comment(2, "/approve-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &[], Some(&pr), &[]);
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(directive_parts(&outcome.transitions[0]).1, "user");
    }

    #[test]
    fn duplicate_across_threads_is_stale() {
        let mut state = state_in(Phase::PrepAndPlan);
        let issue = [comment(1, "/approve-plan", "2026-01-01T00:00:00Z")];
        let pr = [comment(2, "/approve-plan", "2026-01-01T00:01:00Z")];
        let outcome = advance_phase(&mut state, &issue, Some(&pr), &[]);
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
        let comments = [comment(1, "/approve-plan", "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None, &[]);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(matches!(outcome.notes[0].kind, NoteKind::Premature));
        // Consumed: it must never fire later once the phase catches up.
        assert!(state.consumed_directives.contains(&1));
    }

    #[test]
    fn retired_commands_are_noted_not_fired() {
        for command in ["/proceed", "/approve-implementation"] {
            let mut state = state_in(Phase::PrePlan);
            let comments = [comment(1, command, "2026-01-01T00:00:00Z")];
            let outcome = advance_phase(&mut state, &comments, None, &[]);
            assert_eq!(state.phase, Phase::PrePlan);
            assert!(outcome.transitions.is_empty());
            assert!(matches!(outcome.notes[0].kind, NoteKind::Retired));
            assert!(state.consumed_directives.contains(&1));
        }
    }

    fn pull_request(draft: bool, state: &str, merged: bool) -> PullRequest {
        PullRequest {
            number: 18,
            state: state.to_string(),
            merged,
            draft,
            html_url: "https://github.com/o/r/pull/18".to_string(),
        }
    }

    #[test]
    fn pr_ready_advances_implement_to_review() {
        let mut state = state_in(Phase::Implement);
        let mut outcome = AdvanceOutcome::default();
        advance_on_pr_ready(
            &mut state,
            Some(&pull_request(false, "open", false)),
            &mut outcome,
        );
        assert_eq!(state.phase, Phase::Review);
        assert_eq!(outcome.transitions.len(), 1);
        assert!(matches!(outcome.transitions[0].trigger, Trigger::PrReady));
    }

    #[test]
    fn draft_pr_does_not_advance_implement() {
        let mut state = state_in(Phase::Implement);
        let mut outcome = AdvanceOutcome::default();
        advance_on_pr_ready(
            &mut state,
            Some(&pull_request(true, "open", false)),
            &mut outcome,
        );
        assert_eq!(state.phase, Phase::Implement);
        assert!(outcome.transitions.is_empty());
        // No PR recorded at all: likewise nothing fires.
        advance_on_pr_ready(&mut state, None, &mut outcome);
        assert_eq!(state.phase, Phase::Implement);
    }

    #[test]
    fn pr_ready_fires_only_from_implement_and_only_while_open() {
        // Other phases: a ready PR means nothing (it is ready throughout review).
        for phase in [Phase::PrePlan, Phase::PrepAndPlan, Phase::Review] {
            let mut state = state_in(phase);
            let mut outcome = AdvanceOutcome::default();
            advance_on_pr_ready(
                &mut state,
                Some(&pull_request(false, "open", false)),
                &mut outcome,
            );
            assert_eq!(state.phase, phase);
            assert!(outcome.transitions.is_empty());
        }
        // A concluded workflow runs no transition either.
        let mut state = state_in(Phase::Implement);
        state.pr_outcome = Some(PrOutcome::Merged);
        let mut outcome = AdvanceOutcome::default();
        advance_on_pr_ready(
            &mut state,
            Some(&pull_request(false, "closed", true)),
            &mut outcome,
        );
        assert_eq!(state.phase, Phase::Implement);
        assert!(outcome.transitions.is_empty());
    }

    #[test]
    fn status_comments_are_not_directives() {
        let mut state = state_in(Phase::PrePlan);
        // A status comment may mention an approval command at line start.
        let body = crate::render::build_status_comment_body("/approve-pre-plan");
        let comments = [comment(1, &body, "2026-01-01T00:00:00Z")];
        let outcome = advance_phase(&mut state, &comments, None, &[]);
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
        let outcome = advance_phase(&mut state, &comments, None, &[]);
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
        let outcome = advance_phase(&mut state, &issue, Some(&pr), &[]);
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 2);
        assert_eq!(
            directive_parts(&outcome.transitions[0]).0,
            "/approve-pre-plan"
        );
        assert_eq!(directive_parts(&outcome.transitions[1]).0, "/approve-plan");
        assert!(outcome.notes.is_empty());
    }

    #[test]
    fn thumbs_up_advances_and_consumes_by_reaction_id() {
        let mut state = state_in(Phase::PrePlan);
        let prompts = [thumbs(
            10,
            Directive::ApprovePrePlan,
            vec![reaction(100, "+1", "2026-01-01T00:00:00Z")],
        )];
        let outcome = advance_phase(&mut state, &[], None, &prompts);
        assert_eq!(state.phase, Phase::PrepAndPlan);
        assert_eq!(outcome.transitions.len(), 1);
        let (command, by, via_reaction) = directive_parts(&outcome.transitions[0]);
        assert!(via_reaction);
        assert_eq!(command, "/approve-pre-plan");
        assert_eq!(by, "reactor");
        assert!(state.consumed_reactions.contains(&100));
        // Re-running with the same reaction is a no-op.
        let outcome = advance_phase(&mut state, &[], None, &prompts);
        assert_eq!(state.phase, Phase::PrepAndPlan);
        assert!(outcome.transitions.is_empty());
        assert!(outcome.notes.is_empty());
    }

    #[test]
    fn thumbs_up_after_equivalent_directive_is_stale() {
        let mut state = state_in(Phase::PrePlan);
        let comments = [comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z")];
        let prompts = [thumbs(
            10,
            Directive::ApprovePrePlan,
            vec![reaction(100, "+1", "2026-01-01T00:01:00Z")],
        )];
        let outcome = advance_phase(&mut state, &comments, None, &prompts);
        assert_eq!(state.phase, Phase::PrepAndPlan);
        assert_eq!(outcome.transitions.len(), 1);
        assert!(!directive_parts(&outcome.transitions[0]).2);
        assert_eq!(outcome.notes.len(), 1);
        assert!(matches!(outcome.notes[0].kind, NoteKind::Stale));
        assert!(outcome.notes[0].via_reaction);
        assert!(state.consumed_reactions.contains(&100));
    }

    #[test]
    fn premature_thumbs_up_is_noted_not_fired() {
        let mut state = state_in(Phase::PrePlan);
        let prompts = [thumbs(
            10,
            Directive::ApprovePlan,
            vec![reaction(100, "+1", "2026-01-01T00:00:00Z")],
        )];
        let outcome = advance_phase(&mut state, &[], None, &prompts);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(matches!(outcome.notes[0].kind, NoteKind::Premature));
        assert!(state.consumed_reactions.contains(&100));
    }

    #[test]
    fn non_thumbs_up_reactions_are_ignored() {
        let mut state = state_in(Phase::PrePlan);
        let prompts = [thumbs(
            10,
            Directive::ApprovePrePlan,
            vec![
                reaction(100, "heart", "2026-01-01T00:00:00Z"),
                reaction(101, "-1", "2026-01-01T00:01:00Z"),
            ],
        )];
        let outcome = advance_phase(&mut state, &[], None, &prompts);
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert!(outcome.notes.is_empty());
        assert!(state.consumed_reactions.is_empty());
    }

    #[test]
    fn thumbs_up_and_directive_interleave_chronologically() {
        let mut state = state_in(Phase::PrePlan);
        // A 👍 approves pre-plan first; a later typed command approves the
        // plan. Both must fire, in written order.
        let comments = [comment(1, "/approve-plan", "2026-01-01T00:01:00Z")];
        let prompts = [thumbs(
            10,
            Directive::ApprovePrePlan,
            vec![reaction(100, "+1", "2026-01-01T00:00:00Z")],
        )];
        let outcome = advance_phase(&mut state, &comments, None, &prompts);
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 2);
        let (command, _, via_reaction) = directive_parts(&outcome.transitions[0]);
        assert!(via_reaction);
        assert_eq!(command, "/approve-pre-plan");
        let (command, _, via_reaction) = directive_parts(&outcome.transitions[1]);
        assert!(!via_reaction);
        assert_eq!(command, "/approve-plan");
    }

    #[test]
    fn latest_prompt_watch_picks_newest_prompt_with_baseline() {
        let status_old = crate::render::build_status_comment_body(
            "Next: comment `/approve-pre-plan` to advance.",
        );
        let status_new =
            crate::render::build_status_comment_body("Next: comment `/approve-plan` to advance.");
        // A non-prompt ghwf comment and a plain user comment never qualify.
        let status_terminal =
            crate::render::build_status_comment_body("There is no further approval command.");
        let comments = [
            comment(1, &status_old, "2026-01-01T00:00:00Z"),
            comment(2, &status_new, "2026-01-01T00:01:00Z"),
            comment(3, &status_terminal, "2026-01-01T00:02:00Z"),
            comment(4, "/approve-plan", "2026-01-01T00:03:00Z"),
        ];
        let prompts = [thumbs(
            2,
            Directive::ApprovePlan,
            vec![
                reaction(100, "+1", "2026-01-01T00:02:00Z"),
                reaction(101, "heart", "2026-01-01T00:02:30Z"),
            ],
        )];
        let watch = latest_prompt_watch(&comments, None, &prompts).unwrap();
        assert_eq!(watch.comment_id, 2);
        // Baseline holds only the 👍 ids.
        assert!(watch.plus_one_ids.contains(&100));
        assert!(!watch.plus_one_ids.contains(&101));
        // A just-posted status comment outranks everything on the thread.
        let posted = comment(5, &status_new, "2026-01-01T00:04:00Z");
        let watch = latest_prompt_watch(&comments, Some(&posted), &prompts).unwrap();
        assert_eq!(watch.comment_id, 5);
        assert!(watch.plus_one_ids.is_empty());
        // No prompts at all: nothing to watch.
        assert!(latest_prompt_watch(&comments[3..], None, &[]).is_none());
    }
}
