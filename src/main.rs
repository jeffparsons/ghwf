mod access;
mod attach;
mod clone;
mod collect_garbage;
mod config;
mod config_schema;
mod git;
mod github;
mod implement;
mod init;
mod install;
mod labels;
mod launch;
mod models;
mod next;
mod notification_hook;
mod plan_cleanup;
mod prep;
mod render;
mod seen;
mod session_binding;
mod state;
mod stop_hook;
mod store;
mod wait;
mod worktree;

use std::io::Read;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use clap::{Parser, Subcommand, ValueEnum};

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
    /// workers that each grab and start the next issue to come along. To keep
    /// going after each issue concludes, so one terminal works the queue
    /// indefinitely, use `ghwf forever`.
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
        /// Hidden transitional alias for `ghwf forever`: keep working issues one
        /// after another. Implies waiting, so it conflicts with --timeout.
        #[arg(long, hide = true, conflicts_with = "timeout")]
        forever: bool,
    },
    /// Work issues one after another, indefinitely: claim the next eligible
    /// issue, run its session to conclusion, bring it down, and pick again —
    /// parking the worker when the queue is empty.
    ///
    /// This is `ghwf next` in a self-renewing supervised loop: ghwf spawns
    /// Claude as a child and, when the issue's workflow concludes, brings the
    /// session down and claims the next eligible issue. Stops when you quit a
    /// session before its workflow concludes — read as you stepping in and
    /// wanting out.
    Forever {
        /// Work without a dedicated branch/worktree/PR (just write the plan file).
        #[arg(long)]
        no_branch: bool,
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
    /// Passing the issue number targets the issue thread; passing the PR number
    /// targets the PR conversation thread (GitHub treats a PR as an issue). Use
    /// this to answer a question on whichever thread it was asked.
    ///
    /// The comment is prefixed with a "Claude says" header and tagged with hidden
    /// metadata identifying the authoring Claude session.
    CreateIssueComment {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
        /// Attach a local file (repeatable). Each file is committed to the
        /// repo's `ghwf-attachments` branch and linked from the comment (images
        /// embed inline on public repos; on private repos they link).
        #[arg(long = "attach")]
        attach: Vec<PathBuf>,
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
        /// Post a blocking question rather than an end-of-phase hand-off:
        /// flips the workflow to waiting-on-user (so the needs-you label is
        /// applied) but stays in the current phase — no advance prompt, and a
        /// 👍 will not advance the workflow. Use this instead of an
        /// interactive prompt whenever you need an answer from the user.
        #[arg(long)]
        question: bool,
        /// Attach a local file (repeatable). Each file is committed to the
        /// repo's `ghwf-attachments` branch and linked from the comment (images
        /// embed inline on public repos; on private repos they link).
        #[arg(long = "attach")]
        attach: Vec<PathBuf>,
    },
    /// Present the user with a menu of options as a GitHub checklist (the
    /// question is read from stdin), and flip the workflow to waiting-on-user.
    ///
    /// ghwf renders each `--option` as a task-list checkbox with a hidden
    /// stable id, appends a final "Submit my answers" checkbox, and wakes the
    /// workflow only once that box is ticked. Use this instead of `hand-off
    /// --question` when the answer is a choice among discrete options.
    /// Multi-select: the user may tick any number. Consider including an "other
    /// / none of these" option for answers your menu doesn't anticipate —
    /// ticking it signals the question was responded to but not resolved.
    Ask {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
        /// An option to offer, repeatable; presented in the order given.
        #[arg(long = "option")]
        options: Vec<String>,
    },
    /// File a follow-up issue, reading its body from stdin.
    ///
    /// Use this for deferrals and discoveries while planning or implementing —
    /// "defer X to later", "found Y out of scope" — instead of dropping them. By
    /// default the new issue is marked blocked by the issue it was filed from
    /// (the optional `[issue]`, else inferred as `work-on` does), so a worker
    /// won't pick it up until that issue is done; the blocking label is set
    /// atomically at creation, with the native GitHub `blocked_by` dependency set
    /// right after. The new issue is created unassigned and prints as JSON.
    CreateIssue {
        /// The new issue's title.
        #[arg(long)]
        title: String,
        /// The originating issue this follow-up is blocked by: a number
        /// (resolved against the current repo) or a full GitHub issue URL. When
        /// omitted, inferred as `work-on` does. An explicitly given issue that
        /// can't be resolved is an error; a merely inferred one that's absent
        /// just skips blocking.
        issue: Option<String>,
        /// Extra label to attach (repeatable), in addition to the blocked label.
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Create a standalone issue: no blocked label and no dependency.
        #[arg(long)]
        no_block: bool,
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
    /// `/work-on` skill. (The Stop and Notification hooks that keep a session
    /// working — and recover a stuck one — are written per worktree at session
    /// setup, not globally.)
    Install {
        /// Overwrite an existing skill file even when ghwf didn't write it.
        #[arg(long)]
        force: bool,
    },
    /// The Stop-hook entry point Claude Code invokes (configured per worktree
    /// at session setup); not for humans.
    #[command(hide = true)]
    ClaudeStopHook,
    /// The Notification-hook entry point Claude Code invokes (configured per
    /// worktree at session setup); not for humans. Records that the session has
    /// gone idle or parked on a permission prompt, for the supervisor to act on.
    #[command(hide = true)]
    ClaudeNotificationHook {
        /// Which notification fired, set per matcher in the installed settings.
        #[arg(long, value_enum)]
        kind: NotificationKindArg,
    },
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
    /// Print the current title, state, and body of the issue's PR, so Claude
    /// can read it before revising — no need to reach for `gh`.
    ShowPr {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
    },
    /// Update the issue's PR body (read from stdin) and/or its title.
    ///
    /// To change only the title, pass an empty body, e.g.
    /// `ghwf update-pr 49 --title "…" </dev/null`.
    UpdatePr {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
        /// Set the PR title as well as (or instead of) the body.
        #[arg(long)]
        title: Option<String>,
    },
    /// Summarise the CI check status of the issue's PR (wraps `gh pr checks`).
    PrChecks {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
        /// Also dump the failing-job logs for the PR's head commit.
        #[arg(long)]
        log_failed: bool,
    },
    /// Reply to an inline review comment on the issue's PR, reading the reply
    /// body from stdin.
    ///
    /// The reply is prefixed with a "Claude says" header and tagged with the
    /// authoring session, like `create-issue-comment`.
    ReplyReviewComment {
        /// An issue number (resolved against the current repo) or a full GitHub
        /// issue URL. When omitted, inferred as `work-on` does.
        issue: Option<String>,
        /// The id of any comment in the thread to reply to (as surfaced by
        /// `work-on`).
        #[arg(long)]
        id: u64,
    },
}

/// Which notification fired, as the installed Notification-hook entries pass it
/// on the command line. Mirrors [`state::AlertKind`], kept separate so the state
/// type doesn't take a clap dependency.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum NotificationKindArg {
    Idle,
    Permission,
}

impl From<NotificationKindArg> for state::AlertKind {
    fn from(arg: NotificationKindArg) -> Self {
        match arg {
            NotificationKindArg::Idle => state::AlertKind::Idle,
            NotificationKindArg::Permission => state::AlertKind::Permission,
        }
    }
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Interactively create or extend `ghwf.toml`: the essentials when
    /// missing, then optional extras.
    Init,
    /// Set up workflow status labels: create them in the GitHub repo and add
    /// a `[labels]` section to `ghwf.toml`. Re-run once the section exists to
    /// create any configured label still missing from the repo.
    Labels,
    /// List the available config options, each with a one-line summary. Pass a
    /// dotted path (e.g. `labels.phase`) to list the options inside a nested
    /// table.
    Ls {
        /// Dotted path to a nested table whose options to list; omit for the
        /// top level.
        path: Option<String>,
    },
    /// Show the full documentation and type of a single config option, named by
    /// dotted path (e.g. `labels.phase.pre-plan`).
    Info {
        /// Dotted path to the option.
        key: String,
    },
    /// Print a fully-filled, annotated `ghwf.toml` to standard output, showing
    /// every option with its documentation and an example value.
    Example,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::WorkOn { issue, no_branch } => work_on(&resolve_issue_arg(issue)?, no_branch),
        Commands::Next {
            no_branch,
            wait,
            timeout,
            forever,
        } => {
            if forever {
                return next::run_forever(no_branch);
            }
            let number = if wait {
                next::wait_for_pick(timeout)?
            } else {
                next::pick()?
            };
            work_on(&number.to_string(), no_branch)
        }
        Commands::Forever { no_branch } => next::run_forever(no_branch),
        Commands::Clone {
            repo,
            directory,
            reference,
        } => clone::run(&repo, directory.as_deref(), reference.as_deref()),
        Commands::CollectGarbage { dry_run } => collect_garbage::run(dry_run),
        Commands::CreateIssueComment { issue, attach } => {
            create_issue_comment(&resolve_issue_arg(issue)?, &attach)
        }
        Commands::CreateIssue {
            title,
            issue,
            labels,
            no_block,
        } => create_issue(&title, issue, &labels, no_block),
        Commands::HandOff {
            issue,
            question,
            attach,
        } => hand_off(&resolve_issue_arg(issue)?, question, &attach),
        Commands::Ask { issue, options } => ask(&resolve_issue_arg(issue)?, &options),
        Commands::Config { command } => match command {
            ConfigCommands::Init => init::run(),
            ConfigCommands::Labels => labels::configure(),
            ConfigCommands::Ls { path } => config_schema::ls(path.as_deref()),
            ConfigCommands::Info { key } => config_schema::info(&key),
            ConfigCommands::Example => config_schema::example(),
        },
        Commands::WorktreePath { issue } => worktree_path(&resolve_issue_arg(issue)?),
        Commands::Install { force } => install::run(force),
        Commands::ClaudeStopHook => stop_hook::run(),
        Commands::ClaudeNotificationHook { kind } => notification_hook::run(kind.into()),
        Commands::Wait { issue, timeout } => wait::run(&resolve_issue_arg(issue)?, timeout),
        Commands::ShowPr { issue } => show_pr(&resolve_issue_arg(issue)?),
        Commands::UpdatePr { issue, title } => {
            update_pr(&resolve_issue_arg(issue)?, title.as_deref())
        }
        Commands::PrChecks { issue, log_failed } => {
            pr_checks(&resolve_issue_arg(issue)?, log_failed)
        }
        Commands::ReplyReviewComment { issue, id } => {
            reply_review_comment(&resolve_issue_arg(issue)?, id)
        }
    }
}

/// Resolve the issue a command operates on: the explicit argument when given,
/// else $GHWF_ISSUE (set on the session by the outside-Claude launcher), else
/// the issue whose recorded worktree contains the current directory. An
/// explicit argument always wins — the fallbacks are defaults, not locks.
///
/// An explicit *bare number* is anchored to the bound issue's repo (see
/// `qualify_bare_number`) rather than the cwd's git remote, so a session bound
/// to one repo can't silently act on a same-numbered object in another.
fn resolve_issue_arg(arg: Option<String>) -> Result<String> {
    if let Some(arg) = arg {
        // A bare number is ambiguous: left as-is it resolves against the cwd's
        // git remote, which silently targets the wrong repo when the session is
        // bound to an issue in a different repo. Anchor it to the bound issue's
        // repo instead. An explicit URL is left untouched, so it still wins.
        if arg.parse::<u64>().is_ok() {
            if let Some(qualified) = qualify_bare_number(&arg, infer_issue_arg()?.as_deref()) {
                return Ok(qualified);
            }
        }
        return Ok(arg);
    }
    if let Some(inferred) = infer_issue_arg()? {
        return Ok(inferred);
    }
    bail!(
        "no issue given and none could be inferred (${} is unset and the current \
         directory is not inside a recorded worktree); pass an issue number or URL, \
         e.g. `ghwf work-on 13`.",
        store::ISSUE_ENV
    );
}

/// The issue inferred from the session environment, without an explicit
/// argument: $GHWF_ISSUE (set by the outside-Claude launcher) first, else the
/// issue whose recorded worktree contains the current directory. `None` when
/// neither applies — for callers (like `create-issue`) where an absent
/// inference is a soft skip rather than an error.
fn infer_issue_arg() -> Result<Option<String>> {
    if let Ok(value) = std::env::var(store::ISSUE_ENV) {
        if !value.is_empty() {
            return Ok(Some(value));
        }
    }
    let issues_root = store::data_dir()?.join("issues");
    let cwd = std::env::current_dir().context("failed to determine the current directory")?;
    if let Some((owner, repo, number)) = state::find_issue_for_dir(&issues_root, &cwd)? {
        return Ok(Some(format!(
            "https://github.com/{owner}/{repo}/issues/{number}"
        )));
    }
    Ok(None)
}

/// Anchor a bare issue number to the bound issue's repo.
///
/// Given the bound issue (`bound`, a URL from `$GHWF_ISSUE` or worktree state),
/// rewrite a bare number into that repo's issue URL so it can't resolve against
/// the cwd's git remote. Returns `None` — leave the argument untouched — when it
/// isn't a bare number, there's no bound issue, or the bound value can't be
/// parsed. Pure, so the resolution rule is unit-testable without a session.
fn qualify_bare_number(arg: &str, bound: Option<&str>) -> Option<String> {
    arg.parse::<u64>().ok()?;
    let (owner, repo) = github::parse_owner_repo(bound?).ok()?;
    Some(format!("https://github.com/{owner}/{repo}/issues/{arg}"))
}

/// Parse a ghwf issue (or PR) URL into `(owner, repo, number)`. Pure: a bound
/// issue is always a full URL produced by ghwf, so no network call is needed.
/// Returns `None` when the value isn't a URL we can read all three parts from.
fn parse_issue_url(url: &str) -> Option<(String, String, u64)> {
    let (owner, repo) = github::parse_owner_repo(url).ok()?;
    // The number is the final path segment (`…/issues/N` or `…/pull/N`).
    let number = url.rsplit('/').next()?.parse::<u64>().ok()?;
    Some((owner, repo, number))
}

/// Guard against an in-session command acting on a thread outside the bound
/// issue's workflow — the wrong-repo footgun from #90.
///
/// `target` is the workflow issue the command resolved to act on (a PR thread is
/// already mapped back to its issue by `find_workflow_issue`), or `None` when the
/// named thread is not a tracked workflow at all. `bound` is the bound issue URL
/// (`$GHWF_ISSUE` or worktree state). A no-op when there is no bound issue
/// (nothing to compare against) or it can't be parsed (stay lenient, as
/// `qualify_bare_number` does); otherwise the two must name the same issue or the
/// command bails.
fn ensure_target_matches_bound(
    target: Option<(&str, &str, u64)>,
    bound: Option<&str>,
) -> Result<()> {
    let Some((b_owner, b_repo, b_number)) = bound.and_then(parse_issue_url) else {
        return Ok(());
    };
    if target == Some((b_owner.as_str(), b_repo.as_str(), b_number)) {
        return Ok(());
    }
    let target_desc = match target {
        Some((owner, repo, number)) => format!("`{owner}/{repo}#{number}`"),
        None => "a thread with no recorded ghwf workflow".to_string(),
    };
    bail!(
        "explicit target {target_desc} does not match this session's issue \
         `{b_owner}/{b_repo}#{b_number}`; in-session commands act on the bound \
         issue — omit the argument, or pass the bound issue's URL or number."
    )
}

/// Format the one-line resolved-target banner printed before a mutation.
/// `title_state` carries the issue's title and `OPEN`/`CLOSED` state when a fetch
/// succeeded; absent, the line degrades to just the identity.
fn format_target_line(
    owner: &str,
    repo: &str,
    number: u64,
    title_state: Option<(&str, &str)>,
) -> String {
    match title_state {
        Some((title, state)) => {
            format!(
                "→ {owner}/{repo}#{number} \"{title}\" ({})",
                state.to_uppercase()
            )
        }
        None => format!("→ {owner}/{repo}#{number}"),
    }
}

/// Echo the resolved target to stderr before a mutation, so a misroute is visible
/// to both the model and a watching human (#90). Best-effort: a failed title/state
/// fetch downgrades the line to the bare identity rather than blocking the post.
/// Always stderr, so it never pollutes the JSON the comment commands write to
/// stdout.
fn echo_target(owner: &str, repo: &str, number: u64, repo_ctx: Option<&github::RepoRef>) {
    let url = format!("https://github.com/{owner}/{repo}/issues/{number}");
    let title_state = github::fetch_issue(&url, repo_ctx)
        .ok()
        .map(|issue| (issue.title, issue.state));
    let line = format_target_line(
        owner,
        repo,
        number,
        title_state.as_ref().map(|(t, s)| (t.as_str(), s.as_str())),
    );
    eprintln!("{line}");
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
            "no worktree recorded for issue #{number}; run `ghwf work-on` \
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
    // The issue's own repo (which may be a foreign `issue_repos` repo) and the
    // code repo where the worktree, branch, and PR live (the configured
    // `main_repo`, or the issue repo when there's no config). They coincide for
    // the common single-repo case.
    let (owner, repo) = github::parse_owner_repo(&issue_data.html_url)?;
    let (code_owner, code_repo) = github::code_repo(&(owner.clone(), repo.clone()))?;
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
        Some(pr) => Some(github::fetch_pr(&code_owner, &code_repo, pr)?),
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
        // PR reactions live in the code repo, where the PR is.
        prompt_thumbs.extend(collect_prompt_thumbs(
            &code_owner,
            &code_repo,
            comments,
            "PR",
        )?);
    }
    // Only allow-listed authors (the operator, `allowed_users`, and repo
    // collaborators) may drive the workflow or feed the session. Resolve the
    // policy once per run; `allowed_users` comes from the config, if any.
    let allowed_users = config::find()?
        .map(|located| located.config.allowed_users)
        .unwrap_or_default();
    let mut access = access::AccessList::resolve(&allowed_users)?;
    let issue_repo_ref = (owner.clone(), repo.clone());
    let code_repo_ref = (code_owner.clone(), code_repo.clone());
    // A 👍's author carries no association, so the collaborator rule for it
    // needs the repo's collaborator list. Fetch it only when an unconsumed 👍
    // from a not-already-accepted user actually hinges on it (usually none, so
    // no extra API call).
    for prompt in &prompt_thumbs {
        let repo = if prompt.source == "PR" {
            &code_repo_ref
        } else {
            &issue_repo_ref
        };
        for reaction in &prompt.reactions {
            if reaction.is_thumbs_up()
                && !issue_state.consumed_reactions.contains(&reaction.id)
                && access.reaction_needs_collaborators(&reaction.user.login)
            {
                access.ensure_collaborators(repo)?;
            }
        }
    }
    let mut outcome = advance_phase(
        &mut issue_state,
        &issue_comments,
        early_pr_comments.as_deref(),
        &prompt_thumbs,
        &access,
        &issue_repo_ref,
        &code_repo_ref,
    );
    // The implement phase has no approval command: the user marking the draft
    // PR ready for review is what advances it.
    advance_on_pr_ready(&mut issue_state, pr_object.as_ref(), &mut outcome);

    // A merged PR is terminal: the workflow has finished. Stamp the phase (after
    // directive processing, so a merge never suppresses a transition this run
    // wants to show) so the labels and status collapse to the single `finished`
    // state. Closed-without-merge is not "finished" — it keeps its phase as a
    // record of how far the work got.
    if issue_state.pr_outcome == Some(state::PrOutcome::Merged) {
        issue_state.phase = state::Phase::Finished;
    }

    // The phase-specific banner body. Prep-and-plan does real work here (and
    // hard-errors if it needs a config that's missing); implement/review are light.
    // A concluded PR replaces the phase body wholesale: the workflow is over,
    // so no phase work runs (in particular review's draft→ready flip, which
    // would fail against a merged or closed PR).
    let phase = issue_state.phase;
    let located = config::find()?;
    // The project's PR instructions file, when one exists; the implement and
    // review banners point Claude at it.
    let pr_instructions = located
        .as_ref()
        .map(|located| located.pr_instructions_path())
        .filter(|path| path.is_file());

    // The instant a PR's merge is first detected, bring the local default-branch
    // worktree (if any) up to the merged tip, so a long-lived main checkout
    // benefits from the merge without anyone working on it directly. Gated on
    // `new_conclusion` so it fires once per merge, not on every later run over an
    // already-finished issue.
    if new_conclusion == Some(state::PrOutcome::Merged) {
        if let Some(located) = located.as_ref() {
            update_main_worktree_after_merge(located, &code_owner, &code_repo);
        }
    }

    // If configured, erase the plan commit now that the implementation has been
    // approved — i.e. the draft PR was just marked ready for review this run
    // (advance_on_pr_ready pushed the transition). It fires at most once: on
    // later runs the phase is already Review and the transition won't recur.
    let delete_plan_on_approval = located
        .as_ref()
        .is_some_and(|located| located.config.delete_plan_on_approval);
    if delete_plan_on_approval
        && outcome
            .transitions
            .iter()
            .any(|transition| matches!(transition.trigger, render::Trigger::PrReady))
    {
        maybe_delete_plan(&issue_data, number, &issue_state);
    }
    let body = match issue_state.pr_outcome {
        Some(pr_outcome) => render::concluded_body(
            pr_outcome,
            pr_number
                .map(|pr| format!("https://github.com/{code_owner}/{code_repo}/pull/{pr}"))
                .as_deref(),
            number,
        ),
        None => match phase {
            state::Phase::PrePlan => render::pre_plan_body(),
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
                    labels::sync(
                        &(owner.clone(), repo.clone()),
                        &(code_owner.clone(), code_repo.clone()),
                        number,
                        pr_number,
                        &mut issue_state,
                    );
                }
                // The worktree, branch, and PR live in the code repo.
                prep::run(
                    &issue_data,
                    &code_owner,
                    &code_repo,
                    number,
                    no_branch,
                    &mut issue_state,
                )?
            }
            state::Phase::Implement => implement::run(
                &issue_data,
                &code_owner,
                &code_repo,
                number,
                &issue_state,
                pr_instructions.as_deref(),
            )?,
            state::Phase::Review => implement::review(
                &code_owner,
                &code_repo,
                number,
                &issue_state,
                pr_instructions.as_deref(),
            ),
            // `Finished` implies a merged PR, so `pr_outcome` is `Some` and the
            // outer match took the `concluded_body` branch above.
            state::Phase::Finished => {
                unreachable!("finished phase implies a merged PR (pr_outcome is Some)")
            }
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
            .map(|pr| format!("https://github.com/{code_owner}/{code_repo}/pull/{pr}"))
            .as_deref(),
        new_conclusion,
    );
    let status_posted = status.is_some();
    // The issue thread is addressed by its full URL so a foreign `issue_repos`
    // issue resolves to its own repo (a bare number would resolve against the
    // configured repo); the PR thread is addressed by its bare number, which
    // resolves against the configured (code) repo where the PR lives.
    let issue_subject = issue_data.html_url.as_str();
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
                posted_issue = post_status(issue_subject, &status_body, repo_ctx.as_ref(), "issue");
            }
            // Full update on the phase's primary thread; the other thread
            // gets a one-line stub linking to it — or the full body when the
            // primary post failed, so nothing is lost.
            Some(pr) => {
                let primary_is_pr = render::status_primary_is_pr(phase);
                let (primary, primary_noun, secondary, secondary_noun) = if primary_is_pr {
                    (pr.to_string(), "PR", issue_subject.to_string(), "issue")
                } else {
                    (issue_subject.to_string(), "issue", pr.to_string(), "PR")
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
        worktree::ensure_inside(worktree, config_dir.as_deref(), &owner, &repo, number)?;
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
            let pr_review_comments = github::fetch_review_comments(&code_owner, &code_repo, pr)?;
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

    // Surface and consume any `ask` answers the user has just submitted, and
    // keep watching any still-open question per thread. A submission ticks the
    // submit box on a ghwf-authored (hidden) options comment; rewriting that
    // box to a confirmation marks it consumed, so it neither re-surfaces nor is
    // re-watched on later runs.
    let mut submissions = Vec::new();
    let (issue_subs, issue_options_watch) =
        scan_options(&issue_comments, &my_token, "issue thread", &issue_repo_ref);
    submissions.extend(issue_subs);
    if let Some(watch) = issue_options_watch {
        wait_state
            .options_watches
            .insert("issue".to_string(), watch);
    }
    if let Some(comments) = pr_comments.as_deref() {
        let (pr_subs, pr_options_watch) =
            scan_options(comments, &my_token, "PR thread", &code_repo_ref);
        submissions.extend(pr_subs);
        if let Some(watch) = pr_options_watch {
            wait_state.options_watches.insert("pr".to_string(), watch);
        }
    }
    for sub in &submissions {
        if let Err(err) = github::update_issue_comment(
            &sub.repo.0,
            &sub.repo.1,
            sub.comment_id,
            &sub.rewritten_body,
        ) {
            eprintln!(
                "warning: recorded the submitted answers, but couldn't mark the \
                 question comment as submitted: {err:#}"
            );
        }
    }
    let submission_views: Vec<render::OptionSubmission> =
        submissions.into_iter().map(|s| s.view).collect();

    let record = seen::load(&session_id, &owner, &repo, number)?;

    let body_hash = store::content_hash(issue_data.body.as_deref().unwrap_or(""));
    let body_changed = record.issue_body_hash.as_deref() != Some(&body_hash);

    let (new_issue, mut ignored_comments) = collect_new_comments(
        &issue_comments,
        &record.comments,
        &my_token,
        &access,
        "issue",
    );
    let new_pr = match pr_comments.as_deref() {
        Some(comments) => {
            let (views, ignored) =
                collect_new_comments(comments, &record.comments, &my_token, &access, "PR");
            ignored_comments.extend(ignored);
            views
        }
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
        if previous == Some(&hash) {
            continue;
        }
        // Anyone can leave an inline review on a public PR, so gate these too.
        if !access.accepts_comment(&comment.user.login, &comment.author_association) {
            ignored_comments.push(render::IgnoredInput {
                by: comment.user.login.clone(),
                source: "PR",
                kind: render::IgnoredKind::Comment,
            });
            continue;
        }
        new_review.push(ReviewCommentView {
            comment,
            body: render::strip_ghwf_marker(&comment.body),
            location: comment.location(),
            updated: previous.is_some(),
        });
    }
    // Fold the ignored comments in with any ignored reactions, so the banner
    // reports them together. Ignored input is informational only — it must not
    // count as `activity` below (it shouldn't hand the ball back to Claude).
    outcome.ignored.extend(ignored_comments);

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
        || !submission_views.is_empty()
        // A standing conflict keeps the ball with Claude until it's resolved.
        || conflict_base.is_some();
    if activity {
        issue_state.stop_nudges = 0;
        // The session is working again, so any idle/blocked alert the
        // Notification hook left is stale — clear it so the supervisor doesn't
        // act on a resolved one.
        issue_state.session_alert = None;
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
            &outcome.ignored,
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
            &new_review,
            &submission_views
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
    labels::sync(
        &issue_repo_ref,
        &code_repo_ref,
        number,
        pr_number,
        &mut issue_state,
    );
    state::save(&owner, &repo, number, &issue_state)?;

    Ok(())
}

/// Best-effort: after a PR merge is first detected, fetch and fast-forward the
/// local default-branch worktree to the merged tip. Reuses
/// [`prep::update_default_worktree`], which only touches a worktree that is
/// clean and strictly behind `origin/<default>` and otherwise just notes the
/// skip. A failure to fetch or resolve the default branch is a warning, never a
/// hard error — a merged PR's workflow is already over.
fn update_main_worktree_after_merge(located: &config::Located, code_owner: &str, code_repo: &str) {
    let main_repo = located.main_repo_path();
    if let Err(err) = git::fetch(&main_repo) {
        eprintln!("warning: failed to fetch before updating the default-branch worktree: {err:#}");
        return;
    }
    let default = match github::default_branch(code_owner, code_repo) {
        Ok(default) => default,
        Err(err) => {
            eprintln!("warning: failed to resolve the default branch: {err:#}");
            return;
        }
    };
    prep::update_default_worktree(&main_repo, &default);
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
/// updates, this session's own posts) are skipped. Comments from authors who
/// aren't allow-listed are not surfaced; each newly-seen one is reported as an
/// ignored input instead (the seen-record dedupes them to one report apiece).
/// `source` is the thread the comments belong to ("issue" / "PR").
fn collect_new_comments<'a>(
    comments: &'a [models::Comment],
    seen: &std::collections::BTreeMap<u64, String>,
    my_token: &str,
    access: &access::AccessList,
    source: &'static str,
) -> (Vec<CommentView<'a>>, Vec<render::IgnoredInput>) {
    let mut new = Vec::new();
    let mut ignored = Vec::new();
    for comment in comments {
        if render::hidden_from_digest(&comment.body, Some(my_token)) {
            continue;
        }
        let hash = store::content_hash(&comment.body);
        let previous = seen.get(&comment.id);
        if previous == Some(&hash) {
            continue;
        }
        if !access.accepts_comment(&comment.user.login, &comment.author_association) {
            ignored.push(render::IgnoredInput {
                by: comment.user.login.clone(),
                source,
                kind: render::IgnoredKind::Comment,
            });
            continue;
        }
        new.push(CommentView {
            comment,
            body: render::strip_ghwf_marker(&comment.body),
            updated: previous.is_some(),
        });
    }
    (new, ignored)
}

/// An `ask` answer the user has just submitted, ready to surface (`view`) and
/// to consume (`comment_id` + `rewritten_body`, used to mark the comment done).
/// `repo` is where the options comment lives — the issue's repo for an
/// issue-thread question, the code repo for a PR-thread one — so it's edited in
/// the right place when the issue and PR repos differ.
struct PendingSubmission {
    repo: github::RepoRef,
    comment_id: u64,
    rewritten_body: String,
    view: render::OptionSubmission,
}

/// Scan one thread's comments for this session's `ask` options comments.
/// Returns the answers whose submit box is now ticked — each with the rewritten
/// body that marks it consumed — and the latest still-open question on the
/// thread, to keep watching. `repo` is the thread's repo, recorded on each
/// submission so it's consumed in the right place.
fn scan_options(
    comments: &[models::Comment],
    my_token: &str,
    thread_noun: &'static str,
    repo: &github::RepoRef,
) -> (Vec<PendingSubmission>, Option<state::OptionsWatch>) {
    let mut submissions = Vec::new();
    let mut outstanding: Option<&models::Comment> = None;
    for comment in comments {
        // Only this session's own (hidden) comments carry the `ask` markers;
        // status comments and user prose can't be options comments.
        if !render::hidden_from_digest(&comment.body, Some(my_token)) {
            continue;
        }
        let parsed = render::parse_options_comment(&comment.body);
        match parsed.submit {
            Some(true) => {
                let (selected, unselected): (Vec<_>, Vec<_>) =
                    parsed.options.into_iter().partition(|o| o.checked);
                submissions.push(PendingSubmission {
                    repo: repo.clone(),
                    comment_id: comment.id,
                    rewritten_body: render::rewrite_submitted_body(
                        &comment.body,
                        &comment.updated_at,
                    ),
                    view: render::OptionSubmission {
                        thread_noun,
                        url: comment.html_url.clone(),
                        selected: selected.into_iter().map(|o| o.label).collect(),
                        unselected: unselected.into_iter().map(|o| o.label).collect(),
                    },
                });
            }
            // The latest outstanding question is the one still worth watching.
            Some(false) => {
                outstanding = match outstanding {
                    Some(prev) if prev.created_at >= comment.created_at => Some(prev),
                    _ => Some(comment),
                };
            }
            None => {}
        }
    }
    let watch = outstanding.map(|c| state::OptionsWatch { comment_id: c.id });
    (submissions, watch)
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
    // Non-allow-listed 👍 reactions that were ignored. Ignored comments are
    // collected separately by the digest layer; both render together in the
    // banner.
    ignored: Vec<render::IgnoredInput>,
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
    access: &access::AccessList,
    issue_repo: &github::RepoRef,
    code_repo: &github::RepoRef,
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
                // re-fires — including once consumed-but-ignored, so adding the
                // author to the allow-list later can't make an old comment fire.
                issue_state.consumed_directives.insert(comment.id);
                // Only allow-listed authors drive the workflow. The ignored
                // comment itself is reported by the digest layer, so just skip.
                if !access.accepts_comment(&comment.user.login, &comment.author_association) {
                    continue;
                }
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
                let repo = if source == "PR" {
                    code_repo
                } else {
                    issue_repo
                };
                if !access.accepts_reaction(repo, &reaction.user.login) {
                    // A 👍 has no other surfacing path, so note it here.
                    outcome.ignored.push(render::IgnoredInput {
                        by: reaction.user.login.clone(),
                        source,
                        kind: render::IgnoredKind::Reaction,
                    });
                    continue;
                }
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

/// Erase the plan commit from the branch's history, then force-push, when the
/// implementation has just been approved and `delete_plan_on_approval` is set.
/// Branch-mode only; every outcome is a printed note or a warning, never a hard
/// failure (the cleanup must not break the workflow).
fn maybe_delete_plan(issue: &models::Issue, number: u64, issue_state: &state::IssueState) {
    let Some(prep) = issue_state.prep.as_ref() else {
        return;
    };
    // No ghwf-managed commits exist in --no-branch mode.
    if prep.no_branch {
        return;
    }
    let (Some(worktree), Some(branch)) = (prep.worktree_path.as_ref(), prep.branch.as_ref()) else {
        return;
    };
    // Only the slug is needed here, which is independent of the branch prefix.
    let (_, slug) = state::branch_and_slug(None, number, &issue.title);
    let plan_rel = format!("plans/{number}-{slug}.md");
    match plan_cleanup::remove_plan_commit(worktree, branch, &plan_rel) {
        Ok(plan_cleanup::Removal::Removed) => {
            println!("Removed the plan commit (`{plan_rel}`) from `{branch}` and force-pushed.");
        }
        Ok(plan_cleanup::Removal::Skipped(reason)) => {
            eprintln!("warning: not removing the plan commit: {reason}.");
        }
        Err(err) => {
            eprintln!("warning: failed to remove the plan commit: {err:#}");
        }
    }
}

/// Resolve the issue argument to the PR backing its workflow, as `(owner, repo,
/// pr_number)`. Errors clearly when no PR exists yet (pre-plan, or `--no-branch`
/// mode never opens one). The argument may name the PR thread itself; this maps
/// it back to the workflow issue, like `hand_off` does.
fn resolve_pr(issue: &str) -> Result<(String, String, u64)> {
    let repo_ctx = github::config_repo()?;
    let (owner, repo, thread_number) = github::resolve_issue_ref(issue, repo_ctx.as_ref())?;
    let Some((number, issue_state)) = state::find_workflow_issue(&owner, &repo, thread_number)?
    else {
        bail!(
            "no workflow state recorded for issue #{thread_number}; run \
             `ghwf work-on` first."
        );
    };
    match issue_state.prep.as_ref().and_then(|p| p.pr_number) {
        // The PR lives in the code repo, which may differ from the issue's repo.
        Some(pr) => {
            let (code_owner, code_repo) = github::code_repo(&(owner, repo))?;
            Ok((code_owner, code_repo, pr))
        }
        None => bail!("no PR for issue #{number} yet — it is created in the prep-and-plan phase."),
    }
}

/// Read a command's body argument from stdin.
fn read_stdin() -> Result<String> {
    let mut body = String::new();
    std::io::stdin()
        .read_to_string(&mut body)
        .map_err(anyhow::Error::from)?;
    Ok(body)
}

/// Print the issue's PR title, state, and body so Claude can read it before
/// revising — the read path that pairs with `update-pr`.
fn show_pr(issue: &str) -> Result<()> {
    let (owner, repo, pr) = resolve_pr(issue)?;
    let pr = github::fetch_pr(&owner, &repo, pr)?;
    println!("{}", render::pr_overview(&pr));
    Ok(())
}

/// Update the issue's PR body (from stdin) and/or title. The body is written
/// verbatim — it is the PR description, not a Claude-attributed comment.
fn update_pr(issue: &str, title: Option<&str>) -> Result<()> {
    let stdin = read_stdin()?;
    let body = stdin.trim();
    let body = (!body.is_empty()).then_some(body);
    if body.is_none() && title.is_none() {
        bail!("nothing to update: provide a body on stdin and/or `--title`.");
    }

    let (owner, repo, pr) = resolve_pr(issue)?;
    github::update_pr(&owner, &repo, pr, title, body)?;

    let mut changed = Vec::new();
    if title.is_some() {
        changed.push("title");
    }
    if body.is_some() {
        changed.push("body");
    }
    println!("Updated PR #{pr} {}.", changed.join(" and "));
    Ok(())
}

/// Summarise the issue PR's CI checks, optionally dumping failing-job logs.
fn pr_checks(issue: &str, log_failed: bool) -> Result<()> {
    let (owner, repo, pr) = resolve_pr(issue)?;
    let summary = github::pr_checks(&owner, &repo, pr)?;
    println!("{summary}");
    if log_failed {
        let pr_data = github::fetch_pr(&owner, &repo, pr)?;
        let logs = github::failed_run_logs(&owner, &repo, &pr_data.head.sha)?;
        println!("\n{logs}");
    }
    Ok(())
}

/// Reply to an inline review comment on the issue's PR (body from stdin),
/// attributed like a conversation comment.
fn reply_review_comment(issue: &str, id: u64) -> Result<()> {
    let user_body = read_stdin()?;
    if user_body.trim().is_empty() {
        bail!("no reply body provided on stdin");
    }

    // Tag the reply with the authoring session when running under Claude Code.
    let token = match std::env::var(store::SESSION_ID_ENV) {
        Ok(session_id) if !session_id.is_empty() => Some(store::session_token(&session_id)?),
        _ => None,
    };

    let (owner, repo, pr) = resolve_pr(issue)?;
    let body = render::build_comment_body(&user_body, token.as_deref());
    let comment = github::reply_review_comment(&owner, &repo, pr, id, &body)?;
    println!("Posted reply: {}", comment.html_url);
    Ok(())
}

/// Upload `attach` into `(owner, repo)` and append the resulting markdown
/// trailer to `user_body`. With no attachments the body is returned unchanged.
fn append_attachments(
    owner: &str,
    repo: &str,
    issue: u64,
    user_body: &str,
    attach: &[PathBuf],
) -> Result<String> {
    let trimmed = user_body.trim();
    match attach::upload(owner, repo, issue, attach)? {
        Some(trailer) => Ok(format!("{trimmed}\n\n{trailer}")),
        None => Ok(trimmed.to_string()),
    }
}

fn create_issue_comment(issue: &str, attach: &[PathBuf]) -> Result<()> {
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
    // Upload any attachments into the issue's own repo (where the comment lives)
    // before posting, so a failed upload never leaves broken links.
    let (owner, repo, number) = github::resolve_issue_ref(issue, repo_ctx.as_ref())?;
    // Refuse to post to a thread outside the bound issue's workflow (#90): when a
    // bound issue exists, the named thread must map back to it.
    if let Some(bound) = infer_issue_arg()? {
        let workflow = state::find_workflow_issue(&owner, &repo, number)?;
        let target = workflow
            .as_ref()
            .map(|(workflow_number, _)| (owner.as_str(), repo.as_str(), *workflow_number));
        ensure_target_matches_bound(target, Some(bound.as_str()))?;
    }
    let user_body = append_attachments(&owner, &repo, number, &user_body, attach)?;
    let body = render::build_comment_body(&user_body, token.as_deref());
    echo_target(&owner, &repo, number, repo_ctx.as_ref());
    let comment = github::post_issue_comment(issue, &body, repo_ctx.as_ref())?;

    // Remember the post for feed-lag self-calibration in `wait`, best-effort.
    if let Err(err) = record_last_posted(issue, &comment, repo_ctx.as_ref()) {
        eprintln!("warning: failed to record the post for wait calibration: {err:#}");
    }

    println!("{}", render::comment_json(&comment)?);
    Ok(())
}

/// File a follow-up issue (body from stdin), blocked by the originating issue
/// by default.
///
/// The blocking guard is the label: it's included in the create payload, so the
/// issue carries it from the instant it exists and a worker pool can't grab it
/// in the gap before the dependency is set. The native `blocked_by` dependency
/// is set right after as the GitHub-UI truth; if that second call fails the
/// issue still stands (and stays guarded by the label).
fn create_issue(
    title: &str,
    issue_arg: Option<String>,
    labels: &[String],
    no_block: bool,
) -> Result<()> {
    if title.trim().is_empty() {
        bail!("an issue title is required (`--title`)");
    }
    // Issues may have empty bodies, so — unlike the comment commands — empty
    // stdin is fine.
    let body = read_stdin()?;
    let body = body.trim();

    let repo_ctx = github::config_repo()?;
    let (owner, repo) = github::repo_or_cwd()?;
    let blocked_label = config::find()?
        .map(|located| located.config.blocked_label)
        .unwrap_or_else(config::default_blocked_label);

    // Resolve the originating (blocker) issue unless --no-block. An explicit arg
    // resolves strictly; a merely inferred one that doesn't pan out is a soft
    // skip, so a stray invocation still files the issue rather than erroring.
    let blocker = if no_block {
        None
    } else if let Some(arg) = issue_arg {
        Some(github::fetch_issue(&arg, repo_ctx.as_ref())?)
    } else if let Some(arg) = infer_issue_arg()? {
        match github::fetch_issue(&arg, repo_ctx.as_ref()) {
            Ok(issue) => Some(issue),
            Err(err) => {
                eprintln!(
                    "warning: couldn't resolve the inferred originating issue, so \
                     filing an unblocked issue: {err:#}"
                );
                None
            }
        }
    } else {
        eprintln!(
            "warning: no originating issue given or inferred, so filing an unblocked \
             issue (pass an issue, or --no-block to silence this)."
        );
        None
    };

    // The guard label, when blocking — empty (a misconfigured `blocked_label`)
    // disables the guard but not the native dependency.
    let guard_label = blocker
        .as_ref()
        .map(|_| blocked_label.trim())
        .filter(|name| !name.is_empty());
    if blocker.is_some() && guard_label.is_none() {
        eprintln!(
            "warning: `blocked_label` is empty, so no guard label is applied; the \
             follow-up's native blocked_by dependency is still set, but a worker \
             could pick it up before then."
        );
    }
    let label_names = assemble_labels(guard_label, labels);
    // Ensure the guard label exists before it goes in the create payload —
    // GitHub silently drops unknown labels on issue creation, which would leave
    // the issue unguarded. (User --labels follow the usual "must already exist"
    // rule, as elsewhere.)
    if let Some(guard_label) = guard_label {
        github::ensure_label(&owner, &repo, guard_label)?;
    }

    let label_refs: Vec<&str> = label_names.iter().map(String::as_str).collect();
    let issue = github::create_issue(&owner, &repo, title, body, &label_refs)?;

    if let Some(blocker) = &blocker {
        match github::add_blocked_by(&owner, &repo, issue.number, blocker.id) {
            Ok(()) => {
                // The native dependency is now the GitHub-UI truth and is visible
                // on its own, so the guard label has served its one purpose — peel
                // it back off. Best-effort: a failure here just leaves a stale
                // label, which a human can clear.
                if let Some(guard_label) = guard_label {
                    if let Err(err) =
                        github::remove_issue_label(&owner, &repo, issue.number, guard_label)
                    {
                        eprintln!(
                            "warning: filed #{} and set its `blocked_by` dependency, but \
                             couldn't remove the temporary `{guard_label}` guard label: \
                             {err:#}\nremove it by hand; the dependency is what matters.",
                            issue.number
                        );
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "warning: filed #{} but couldn't set its `blocked_by` dependency on \
                     #{}: {err:#}\nthe `{blocked_label}` label is set, so a worker still \
                     won't pick it up; add the dependency by hand for the GitHub UI.",
                    issue.number, blocker.number
                );
            }
        }
    }

    println!("{}", render::issue_json(&issue)?);
    Ok(())
}

/// The labels for a new follow-up issue: the blocked label first (when
/// blocking), then any extras, trimmed, with empties and case-insensitive
/// duplicates dropped.
fn assemble_labels(blocked: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(blocked) = blocked {
        out.push(blocked.to_string());
    }
    for label in extra {
        let label = label.trim();
        if !label.is_empty() && !out.iter().any(|seen| seen.eq_ignore_ascii_case(label)) {
            out.push(label.to_string());
        }
    }
    out
}

/// Post Claude's hand-off comment (body from stdin) with the current phase's
/// next-step prompt appended by ghwf, flip the workflow to waiting-on-user,
/// and sync labels. The prompt makes the hand-off the thread's 👍 target
/// where an approval command applies.
///
/// In `question` mode the comment is a blocking question rather than an
/// end-of-phase hand-off: the body is posted as-is (no advance prompt) and no
/// 👍-advances-the-phase watch is armed, but attention still flips to the user
/// so the needs-you label is applied while Claude waits for an answer.
fn hand_off(issue: &str, question: bool, attach: &[PathBuf]) -> Result<()> {
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
             `ghwf work-on` first."
        );
    };
    // Refuse to hand off against a thread outside the bound issue's workflow (#90).
    ensure_target_matches_bound(Some((&owner, &repo, number)), infer_issue_arg()?.as_deref())?;

    if issue_state.pr_outcome.is_some() {
        bail!(
            "the workflow for issue #{number} has concluded (its PR was merged or \
             closed); there is nothing to hand off."
        );
    }
    let phase = issue_state.phase;
    let no_branch = issue_state.prep.as_ref().is_some_and(|p| p.no_branch);
    // A normal hand-off needs a phase advance prompt; the review phase has
    // none (the PR is already with the user), so it can't be handed off. A
    // question carries no prompt, so it's allowed in any phase.
    let prompt = render::hand_off_prompt(phase, no_branch);
    if !question && prompt.is_none() {
        bail!(
            "the workflow for issue #{number} is in the review phase — the PR is \
             already with the user; there is nothing to hand off."
        );
    }

    // Full comment on the phase's primary thread; when a PR exists, the other
    // thread gets a one-line stub linking to it, best-effort. The issue thread
    // is addressed by its full URL (so a foreign `issue_repos` issue resolves to
    // its own repo); the PR thread by its bare number (which resolves against
    // the configured code repo where the PR lives).
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let issue_subject = format!("https://github.com/{owner}/{repo}/issues/{number}");
    let primary_is_pr = pr_number.is_some() && render::status_primary_is_pr(phase);
    let primary_subject = if primary_is_pr {
        pr_number.expect("primary_is_pr requires a PR").to_string()
    } else {
        issue_subject.clone()
    };

    // Upload attachments into the repo hosting the primary thread (the code repo
    // when that thread is the PR, else the issue's repo) before posting, so a
    // failed upload never leaves broken links.
    let (att_owner, att_repo) = if primary_is_pr {
        github::code_repo(&(owner.clone(), repo.clone()))?
    } else {
        (owner.clone(), repo.clone())
    };
    let user_body = append_attachments(&att_owner, &att_repo, number, &user_body, attach)?;

    // Tag the comment with the authoring session when running under Claude Code.
    let token = match std::env::var(store::SESSION_ID_ENV) {
        Ok(session_id) if !session_id.is_empty() => Some(store::session_token(&session_id)?),
        _ => None,
    };
    // A question is posted as-is; an end-of-phase hand-off gets the advance
    // prompt appended so the comment is its own 👍 target.
    let full_body = match prompt {
        Some(prompt) if !question => {
            render::build_comment_body(&format!("{user_body}\n\n{prompt}"), token.as_deref())
        }
        _ => render::build_comment_body(&user_body, token.as_deref()),
    };

    echo_target(&owner, &repo, number, repo_ctx.as_ref());
    let comment = github::post_issue_comment(&primary_subject, &full_body, repo_ctx.as_ref())?;
    if let Some(pr) = pr_number {
        let (secondary_subject, secondary_noun, primary_noun) = if primary_is_pr {
            (issue_subject.clone(), "issue", "PR")
        } else {
            (pr.to_string(), "PR", "issue")
        };
        let noun = if question { "Question" } else { "Hand-off" };
        let stub = render::build_status_comment_body(&format!(
            "{noun} posted on the {primary_noun}: {}",
            comment.html_url
        ));
        if let Err(err) = github::post_issue_comment(&secondary_subject, &stub, repo_ctx.as_ref()) {
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
    // A question never advances the phase, so it arms no 👍 watch even if its
    // body happens to mention an approval command.
    if !question && state::parse_prompted_directive(&full_body).is_some() {
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
    let code = github::code_repo(&(owner.clone(), repo.clone()))?;
    labels::sync(
        &(owner.clone(), repo.clone()),
        &code,
        number,
        pr_number,
        &mut issue_state,
    );
    state::save(&owner, &repo, number, &issue_state)?;

    println!("{}", render::comment_json(&comment)?);
    Ok(())
}

/// Post an `ask` options comment (the question from stdin, each `--option` a
/// checkbox) and flip the workflow to waiting-on-user, like a blocking
/// question. ghwf owns the formatting — hidden per-option ids and the appended
/// submit checkbox — and arms an options watch so `wait` wakes only once the
/// submit box is ticked. The phase never advances, so no approval prompt or
/// 👍 watch is involved.
fn ask(issue: &str, options: &[String]) -> Result<()> {
    let mut question = String::new();
    std::io::stdin()
        .read_to_string(&mut question)
        .map_err(anyhow::Error::from)?;
    if question.trim().is_empty() {
        bail!("no question body provided on stdin");
    }
    if options.iter().all(|o| o.trim().is_empty()) {
        bail!("at least one --option is required");
    }
    let options: Vec<String> = options
        .iter()
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .collect();

    let repo_ctx = github::config_repo()?;
    let (owner, repo, thread_number) = github::resolve_issue_ref(issue, repo_ctx.as_ref())?;
    let Some((number, mut issue_state)) = state::find_workflow_issue(&owner, &repo, thread_number)?
    else {
        bail!(
            "no workflow state recorded for issue #{thread_number}; run \
             `ghwf work-on` first."
        );
    };
    // Refuse to ask against a thread outside the bound issue's workflow (#90).
    ensure_target_matches_bound(Some((&owner, &repo, number)), infer_issue_arg()?.as_deref())?;
    if issue_state.pr_outcome.is_some() {
        bail!(
            "the workflow for issue #{number} has concluded (its PR was merged or \
             closed); there is nothing to ask about."
        );
    }

    // Tag the comment with the authoring session when running under Claude Code.
    let token = match std::env::var(store::SESSION_ID_ENV) {
        Ok(session_id) if !session_id.is_empty() => Some(store::session_token(&session_id)?),
        _ => None,
    };
    let full_body = render::build_comment_body(
        &render::build_options_body(&question, &options),
        token.as_deref(),
    );

    // Full comment on the phase's primary thread; when a PR exists, the other
    // thread gets a one-line stub linking to it, best-effort. The issue thread
    // is addressed by its full URL (so a foreign `issue_repos` issue resolves to
    // its own repo); the PR thread by its bare number (which resolves against
    // the configured code repo where the PR lives).
    let pr_number = issue_state.prep.as_ref().and_then(|p| p.pr_number);
    let issue_subject = format!("https://github.com/{owner}/{repo}/issues/{number}");
    let primary_is_pr = pr_number.is_some() && render::status_primary_is_pr(issue_state.phase);
    let primary_subject = if primary_is_pr {
        pr_number.expect("primary_is_pr requires a PR").to_string()
    } else {
        issue_subject.clone()
    };
    echo_target(&owner, &repo, number, repo_ctx.as_ref());
    let comment = github::post_issue_comment(&primary_subject, &full_body, repo_ctx.as_ref())?;
    if let Some(pr) = pr_number {
        let (secondary_subject, secondary_noun, primary_noun) = if primary_is_pr {
            (issue_subject.clone(), "issue", "PR")
        } else {
            (pr.to_string(), "PR", "issue")
        };
        let stub = render::build_status_comment_body(&format!(
            "Question posted on the {primary_noun}: {}",
            comment.html_url
        ));
        if let Err(err) = github::post_issue_comment(&secondary_subject, &stub, repo_ctx.as_ref()) {
            eprintln!("warning: failed to post the question stub to the {secondary_noun}: {err:#}");
        }
    }

    // The ball is with the user now. Remember the post for feed-lag
    // self-calibration, and watch its submit box so `wait` wakes on submission.
    issue_state.attention = state::Attention::WaitingOnUser;
    issue_state.last_posted = Some(state::PostedRef {
        id: comment.id,
        created_at: comment.created_at.clone(),
    });
    if let Some(wait) = issue_state.wait.as_mut() {
        let thread = if primary_is_pr { "pr" } else { "issue" };
        wait.options_watches.insert(
            thread.to_string(),
            state::OptionsWatch {
                comment_id: comment.id,
            },
        );
        // The watch endpoint's URL changed with it; drop the stale ETag.
        wait.etags.remove(&format!("options_{thread}"));
    }
    let code = github::code_repo(&(owner.clone(), repo.clone()))?;
    labels::sync(
        &(owner.clone(), repo.clone()),
        &code,
        number,
        pr_number,
        &mut issue_state,
    );
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
        advance_on_pr_ready, advance_phase, assemble_labels, ensure_target_matches_bound,
        format_target_line, latest_prompt_watch, needs_worktree_guard, parse_issue_url,
        qualify_bare_number, AdvanceOutcome, Cli, PromptThumbs,
    };
    use crate::models::{Comment, PullRequest, Reaction, User};
    use crate::render::{NoteKind, Transition, Trigger};
    use crate::state::{Directive, IssueState, Phase, PrOutcome, PrepState};
    use clap::{CommandFactory, Parser};

    #[test]
    fn bare_number_anchors_to_the_bound_issue_repo() {
        // The session is bound to repo A; a bare number must resolve there, not
        // against whatever repo the cwd happens to be a checkout of.
        let bound = "https://github.com/owner-a/repo-a/issues/100";
        assert_eq!(
            qualify_bare_number("42", Some(bound)).as_deref(),
            Some("https://github.com/owner-a/repo-a/issues/42")
        );
    }

    #[test]
    fn bare_number_without_a_bound_issue_is_left_alone() {
        // No bound issue: fall back to today's behaviour (config repo, then cwd).
        assert_eq!(qualify_bare_number("42", None), None);
    }

    #[test]
    fn explicit_url_argument_is_never_rewritten() {
        // A URL is unambiguous and must override the bound issue's repo.
        let bound = "https://github.com/owner-a/repo-a/issues/100";
        assert_eq!(
            qualify_bare_number("https://github.com/owner-b/repo-b/issues/42", Some(bound)),
            None
        );
    }

    #[test]
    fn unparseable_bound_value_falls_back_to_passthrough() {
        // A bound value we can't read an owner/repo from must not crash the
        // command — leave the argument as the caller gave it.
        assert_eq!(qualify_bare_number("42", Some("not-a-url")), None);
    }

    #[test]
    fn parse_issue_url_reads_owner_repo_and_number() {
        // Both the issue and PR URL shapes yield all three parts…
        assert_eq!(
            parse_issue_url("https://github.com/owner-a/repo-a/issues/100"),
            Some(("owner-a".to_string(), "repo-a".to_string(), 100))
        );
        assert_eq!(
            parse_issue_url("https://github.com/owner-a/repo-a/pull/7"),
            Some(("owner-a".to_string(), "repo-a".to_string(), 7))
        );
        // …and a value we can't read all three from yields None rather than panicking.
        assert_eq!(parse_issue_url("not-a-url"), None);
    }

    #[test]
    fn target_matching_the_bound_issue_is_allowed() {
        // The resolved workflow issue names the same issue as the bound URL.
        let bound = "https://github.com/owner-a/repo-a/issues/100";
        assert!(ensure_target_matches_bound(Some(("owner-a", "repo-a", 100)), Some(bound)).is_ok());
    }

    #[test]
    fn target_in_a_different_repo_is_refused() {
        // The report's bug: a same-numbered object in another repo must not pass.
        let bound = "https://github.com/owner-a/repo-a/issues/100";
        assert!(
            ensure_target_matches_bound(Some(("owner-b", "repo-b", 100)), Some(bound)).is_err()
        );
    }

    #[test]
    fn target_with_a_different_number_is_refused() {
        // Same repo, wrong issue — a sibling issue is still a misroute.
        let bound = "https://github.com/owner-a/repo-a/issues/100";
        assert!(ensure_target_matches_bound(Some(("owner-a", "repo-a", 42)), Some(bound)).is_err());
    }

    #[test]
    fn target_with_no_recorded_workflow_is_refused() {
        // A bound issue exists but the named thread isn't a tracked workflow
        // (the closed dependabot PR in the report) — refuse.
        let bound = "https://github.com/owner-a/repo-a/issues/100";
        assert!(ensure_target_matches_bound(None, Some(bound)).is_err());
    }

    #[test]
    fn no_bound_issue_skips_validation() {
        // Nothing to compare against (e.g. a standalone, non-session command):
        // any target — even none — is allowed.
        assert!(ensure_target_matches_bound(Some(("owner-b", "repo-b", 7)), None).is_ok());
        assert!(ensure_target_matches_bound(None, None).is_ok());
    }

    #[test]
    fn unparseable_bound_skips_validation() {
        // An unreadable bound value stays lenient, matching qualify_bare_number.
        assert!(
            ensure_target_matches_bound(Some(("owner-b", "repo-b", 7)), Some("not-a-url")).is_ok()
        );
    }

    #[test]
    fn target_line_includes_title_and_state_when_known() {
        assert_eq!(
            format_target_line("owner-a", "repo-a", 100, Some(("Fix the thing", "open"))),
            "→ owner-a/repo-a#100 \"Fix the thing\" (OPEN)"
        );
    }

    #[test]
    fn target_line_degrades_to_identity_without_a_fetch() {
        assert_eq!(
            format_target_line("owner-a", "repo-a", 100, None),
            "→ owner-a/repo-a#100"
        );
    }

    #[test]
    fn cli_definition_is_valid() {
        // Catches arg-config mistakes like a conflict referencing a nonexistent
        // argument.
        Cli::command().debug_assert();
    }

    #[test]
    fn next_forever_parses_and_conflicts_with_timeout() {
        // `next --forever` still parses as a hidden transitional alias…
        assert!(Cli::try_parse_from(["ghwf", "next", "--forever"]).is_ok());
        // …but `--forever` is the opposite of a one-shot give-up, so pairing it
        // with `--timeout` is rejected.
        assert!(Cli::try_parse_from(["ghwf", "next", "--forever", "--timeout", "30"]).is_err());
    }

    #[test]
    fn forever_subcommand_parses_without_wait_args() {
        // `ghwf forever` parses on its own and with `--no-branch`…
        assert!(Cli::try_parse_from(["ghwf", "forever"]).is_ok());
        assert!(Cli::try_parse_from(["ghwf", "forever", "--no-branch"]).is_ok());
        // …but it takes no `--wait`/`--timeout` (forever-mode already waits).
        assert!(Cli::try_parse_from(["ghwf", "forever", "--wait"]).is_err());
        assert!(Cli::try_parse_from(["ghwf", "forever", "--timeout", "30"]).is_err());
    }

    #[test]
    fn assemble_labels_puts_blocked_first_and_dedupes() {
        // Blocked label leads; extras follow; a case-insensitive duplicate of
        // the blocked label and empty entries are dropped.
        assert_eq!(
            assemble_labels(
                Some("blocked"),
                &["foo".into(), "Blocked".into(), "  ".into()]
            ),
            vec!["blocked", "foo"]
        );
        // No blocker: just the (trimmed, de-duplicated) extras.
        assert_eq!(
            assemble_labels(None, &[" foo ".into(), "foo".into()]),
            vec!["foo"]
        );
        // No blocker, no extras: empty.
        assert!(assemble_labels(None, &[]).is_empty());
    }

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

    // An accept-all policy for the existing advance_phase tests: the comment
    // helper's author ("user") is the authenticated user, and the reaction
    // helper's author ("reactor") is allow-listed.
    fn access_all() -> crate::access::AccessList {
        crate::access::AccessList::new("user", &["reactor".to_string()])
    }

    fn repo_ref() -> crate::github::RepoRef {
        ("o".to_string(), "r".to_string())
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
        let outcome = advance_phase(
            &mut state,
            &comments,
            None,
            &[],
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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
        let outcome = advance_phase(
            &mut state,
            &[],
            Some(&pr),
            &[],
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
        assert_eq!(state.phase, Phase::Implement);
        assert_eq!(outcome.transitions.len(), 1);
        assert_eq!(directive_parts(&outcome.transitions[0]).1, "user");
    }

    #[test]
    fn duplicate_across_threads_is_stale() {
        let mut state = state_in(Phase::PrepAndPlan);
        let issue = [comment(1, "/approve-plan", "2026-01-01T00:00:00Z")];
        let pr = [comment(2, "/approve-plan", "2026-01-01T00:01:00Z")];
        let outcome = advance_phase(
            &mut state,
            &issue,
            Some(&pr),
            &[],
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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
        let outcome = advance_phase(
            &mut state,
            &comments,
            None,
            &[],
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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
            let outcome = advance_phase(
                &mut state,
                &comments,
                None,
                &[],
                &access_all(),
                &repo_ref(),
                &repo_ref(),
            );
            assert_eq!(state.phase, Phase::PrePlan);
            assert!(outcome.transitions.is_empty());
            assert!(matches!(outcome.notes[0].kind, NoteKind::Retired));
            assert!(state.consumed_directives.contains(&1));
        }
    }

    fn pull_request(draft: bool, state: &str, merged: bool) -> PullRequest {
        PullRequest {
            number: 18,
            title: "PR".to_string(),
            state: state.to_string(),
            merged,
            draft,
            body: None,
            html_url: "https://github.com/o/r/pull/18".to_string(),
            head: crate::models::Head {
                ref_name: "branch".to_string(),
                sha: "sha".to_string(),
            },
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
        let outcome = advance_phase(
            &mut state,
            &comments,
            None,
            &[],
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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

        let (new, ignored) = collect_new_comments(&comments, &seen, "mine", &access_all(), "issue");
        assert_eq!(new.len(), 2);
        assert_eq!(new[0].comment.id, 2);
        assert!(new[0].updated);
        assert_eq!(new[1].comment.id, 3);
        assert!(!new[1].updated);
        // The comment helper's author is the authenticated user, so nothing is
        // ignored on acceptance grounds here.
        assert!(ignored.is_empty());
    }

    #[test]
    fn collect_new_comments_filters_non_allow_listed_authors() {
        use super::collect_new_comments;
        use std::collections::BTreeMap;

        let mut stranger = comment(1, "drive-by", "2026-01-01T00:00:00Z");
        stranger.user.login = "stranger".to_string();
        stranger.author_association = "NONE".to_string();
        let mine = comment(2, "from me", "2026-01-01T00:01:00Z");
        let comments = [stranger, mine];
        let seen: BTreeMap<u64, String> = BTreeMap::new();

        // "user" is the authenticated user; "stranger" (NONE) is not allowed.
        let access = crate::access::AccessList::new("user", &[]);
        let (new, ignored) = collect_new_comments(&comments, &seen, "tok", &access, "issue");
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].comment.id, 2);
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].by, "stranger");
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
        let outcome = advance_phase(
            &mut state,
            &comments,
            None,
            &[],
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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
        let outcome = advance_phase(
            &mut state,
            &issue,
            Some(&pr),
            &[],
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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
        let outcome = advance_phase(
            &mut state,
            &[],
            None,
            &prompts,
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
        assert_eq!(state.phase, Phase::PrepAndPlan);
        assert_eq!(outcome.transitions.len(), 1);
        let (command, by, via_reaction) = directive_parts(&outcome.transitions[0]);
        assert!(via_reaction);
        assert_eq!(command, "/approve-pre-plan");
        assert_eq!(by, "reactor");
        assert!(state.consumed_reactions.contains(&100));
        // Re-running with the same reaction is a no-op.
        let outcome = advance_phase(
            &mut state,
            &[],
            None,
            &prompts,
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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
        let outcome = advance_phase(
            &mut state,
            &comments,
            None,
            &prompts,
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
        assert_eq!(state.phase, Phase::PrepAndPlan);
        assert_eq!(outcome.transitions.len(), 1);
        assert!(!directive_parts(&outcome.transitions[0]).2);
        assert_eq!(outcome.notes.len(), 1);
        assert!(matches!(outcome.notes[0].kind, NoteKind::Stale));
        assert!(outcome.notes[0].via_reaction);
        assert!(state.consumed_reactions.contains(&100));
    }

    #[test]
    fn non_allow_listed_directive_does_not_advance_but_is_consumed() {
        let mut state = state_in(Phase::PrePlan);
        let mut stranger = comment(1, "/approve-pre-plan", "2026-01-01T00:00:00Z");
        stranger.user.login = "stranger".to_string();
        stranger.author_association = "NONE".to_string();
        // Only "user" is accepted; "stranger" (NONE) is not.
        let access = crate::access::AccessList::new("user", &[]);
        let outcome = advance_phase(
            &mut state,
            &[stranger],
            None,
            &[],
            &access,
            &repo_ref(),
            &repo_ref(),
        );
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        // Consumed, so adding the author to the allow-list later can't make this
        // old comment fire retroactively.
        assert!(state.consumed_directives.contains(&1));
    }

    #[test]
    fn non_allow_listed_thumbs_up_does_not_advance_and_is_noted() {
        let mut state = state_in(Phase::PrePlan);
        let prompts = [thumbs(
            10,
            Directive::ApprovePrePlan,
            vec![reaction(100, "+1", "2026-01-01T00:00:00Z")],
        )];
        // The reaction author "reactor" is not accepted (no collaborators set).
        let access = crate::access::AccessList::new("user", &[]);
        let outcome = advance_phase(
            &mut state,
            &[],
            None,
            &prompts,
            &access,
            &repo_ref(),
            &repo_ref(),
        );
        assert_eq!(state.phase, Phase::PrePlan);
        assert!(outcome.transitions.is_empty());
        assert_eq!(outcome.ignored.len(), 1);
        assert_eq!(outcome.ignored[0].by, "reactor");
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
        let outcome = advance_phase(
            &mut state,
            &[],
            None,
            &prompts,
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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
        let outcome = advance_phase(
            &mut state,
            &[],
            None,
            &prompts,
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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
        let outcome = advance_phase(
            &mut state,
            &comments,
            None,
            &prompts,
            &access_all(),
            &repo_ref(),
            &repo_ref(),
        );
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

    #[test]
    fn scan_options_surfaces_submission_and_arms_no_watch() {
        use crate::render::{build_comment_body, build_options_body};
        let body = build_comment_body(
            &build_options_body("Pick", &["A".into(), "B".into()]),
            Some("tok"),
        )
        .replacen("- [ ] A", "- [x] A", 1)
        .replace("- [ ] **Submit", "- [x] **Submit");
        let comments = vec![comment(9, &body, "2026-06-06T12:00:00Z")];
        let (subs, watch) = super::scan_options(
            &comments,
            "tok",
            "issue thread",
            &("Org".to_string(), "repo".to_string()),
        );
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].comment_id, 9);
        assert_eq!(subs[0].view.selected, vec!["A".to_string()]);
        assert_eq!(subs[0].view.unselected, vec!["B".to_string()]);
        assert!(subs[0].rewritten_body.contains("_Answers submitted at"));
        // A submitted comment isn't re-watched.
        assert!(watch.is_none());
    }

    #[test]
    fn scan_options_watches_latest_outstanding() {
        use crate::render::{build_comment_body, build_options_body};
        let body = |q: &str| build_comment_body(&build_options_body(q, &["A".into()]), Some("tok"));
        let comments = vec![
            comment(9, &body("Old"), "2026-06-06T12:00:00Z"),
            comment(10, &body("New"), "2026-06-06T13:00:00Z"),
        ];
        let (subs, watch) = super::scan_options(
            &comments,
            "tok",
            "issue thread",
            &("Org".to_string(), "repo".to_string()),
        );
        assert!(subs.is_empty());
        assert_eq!(watch.unwrap().comment_id, 10);
    }

    #[test]
    fn scan_options_ignores_foreign_comments() {
        use crate::render::{build_comment_body, build_options_body};
        // Another session's options comment and a plain user comment: neither
        // is ours to act on.
        let other = build_comment_body(&build_options_body("Pick", &["A".into()]), Some("other"));
        let comments = vec![
            comment(9, &other, "2026-06-06T12:00:00Z"),
            comment(10, "just talking", "2026-06-06T12:30:00Z"),
        ];
        let (subs, watch) = super::scan_options(
            &comments,
            "tok",
            "issue thread",
            &("Org".to_string(), "repo".to_string()),
        );
        assert!(subs.is_empty());
        assert!(watch.is_none());
    }
}
