use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::state::{self, IssueState};
use crate::{config, git, github, next, prep, store};

/// Run `work-on` as a launcher: no Claude session is present, so prepare the
/// issue's worktree and replace this process with an interactive Claude session
/// anchored in it, resuming the worktree's previous session when possible.
///
/// This is deliberately thin — phase advancement, banners, and the activity
/// digest all happen on the `ghwf work-on` run *inside* the launched session.
pub fn run(issue_arg: &str, no_branch: bool) -> Result<()> {
    println!(
        "No Claude session detected ({} is unset), so ghwf is acting as a launcher:\n\
         it will make sure the issue's worktree exists, then start Claude in it.",
        store::SESSION_ID_ENV
    );

    let repo_ctx = github::config_repo()?;
    let (owner, repo, requested) = github::resolve_issue_ref(issue_arg, repo_ctx.as_ref())?;
    // The code repo (where the worktree, branch, and PR live) — the configured
    // repo, or the issue's own repo when there's no config. It may differ from
    // the issue's repo for a foreign `issue_repos` issue.
    let (code_owner, code_repo) = github::code_repo(&(owner.clone(), repo.clone()))?;
    // A tracking issue (one with sub-issues) is never worked directly: redirect
    // to a workable sub-issue so the session is anchored to the real work.
    // Sub-issues live in the issue's own repo.
    let number = next::resolve_workable(&owner, &repo, requested)?;
    if number != requested {
        println!("Issue #{requested} is a tracking issue; working sub-issue #{number} instead.");
    }
    let mut issue_state = state::load(&owner, &repo, number)?;

    // The recorded mode wins over the flag, as in prep-and-plan.
    let use_no_branch = match issue_state.prep.as_ref().map(|p| p.no_branch) {
        Some(recorded) => {
            if no_branch && !recorded {
                eprintln!(
                    "warning: this issue is already being worked in branch mode; \
                     ignoring --no-branch."
                );
            }
            recorded
        }
        None => no_branch,
    };

    // The canonical issue URL, set as $GHWF_ISSUE on the launched Claude so
    // ghwf commands inside the session don't need an issue argument.
    let issue_url = format!("https://github.com/{owner}/{repo}/issues/{number}");

    // The configured permission mode for the launched session, resolved
    // best-effort: several launch paths must keep working without a config.
    let permission_mode = match config::find() {
        Ok(Some(located)) => located.config.permission_mode,
        Ok(None) => None,
        Err(err) => {
            eprintln!(
                "warning: launching without a permission mode — \
                 failed to load the config: {err:#}"
            );
            None
        }
    };

    // --no-branch work has no worktree to anchor a session to: launch a fresh
    // Claude where we are.
    if use_no_branch {
        println!(
            "Issue #{number} is being worked with --no-branch (no dedicated worktree), \
             so Claude will start in the current directory{}.",
            mode_note(permission_mode.as_deref())
        );
        // Record the mode now so the in-session run honours it even though the
        // user won't repeat the flag (recorded mode wins over the flag there).
        if issue_state.prep.is_none() {
            issue_state.prep = Some(state::PrepState {
                no_branch: true,
                ..Default::default()
            });
            state::save(&owner, &repo, number, &issue_state)?;
        }
        return exec_claude(None, None, permission_mode.as_deref(), &issue_url);
    }

    // Find or create the worktree. The launcher creates it immediately — even
    // before planning — so the session it starts is anchored there and stays
    // resumable across every phase.
    let worktree = match issue_state
        .prep
        .as_ref()
        .and_then(|p| p.worktree_path.clone())
    {
        Some(path) => {
            if !path.is_dir() {
                bail!(
                    "the worktree recorded for issue #{number}, `{}`, no longer exists on disk; \
                     restore it (or clear the issue's ghwf state) and retry.",
                    path.display()
                );
            }
            println!(
                "Issue #{number} already has its worktree at `{}`.",
                path.display()
            );
            // The worktree-creation path fetches as a side effect; this path
            // wouldn't otherwise touch the network, so take the opportunity
            // to keep the local default-branch checkout fresh. The default
            // branch is the code repo's, where the worktree lives.
            refresh_main_repo(&code_owner, &code_repo);
            path
        }
        None => {
            println!(
                "Issue #{number} has no worktree yet; creating it now so the Claude session \
                 is anchored there and can be resumed for later phases."
            );
            // Fetch the issue we resolved to work on. After a tracking-issue
            // redirect that is a sub-issue, not the `issue_arg` the user named,
            // so fetch by the canonical URL of the resolved number.
            let issue_data = github::fetch_issue(&issue_url, repo_ctx.as_ref())?;
            // The worktree and branch live in the code repo.
            let (path, branch) =
                prep::ensure_worktree(&issue_data, &code_owner, &code_repo, &mut issue_state)?;
            state::save(&owner, &repo, number, &issue_state)?;
            println!(
                "Created worktree `{}` on branch `{branch}`.",
                path.display()
            );
            path
        }
    };

    // Resume the worktree's recorded session if its transcript is still around.
    let resume = resumable_session(&store::claude_dir()?, &issue_state, &worktree);
    match &resume {
        Some(id) => println!(
            "Resuming this worktree's previous Claude session: launching \
             `claude --resume {id}` in `{}`{}.",
            worktree.display(),
            mode_note(permission_mode.as_deref())
        ),
        None => {
            println!(
                "Starting a fresh Claude session in `{}`{}.",
                worktree.display(),
                mode_note(permission_mode.as_deref())
            );
        }
    }
    exec_claude(
        Some(&worktree),
        resume.as_deref(),
        permission_mode.as_deref(),
        &issue_url,
    )
}

/// Note appended to launch messages when a permission mode is configured, so
/// the user can see why Claude came up in that mode. Empty when none is.
fn mode_note(permission_mode: Option<&str>) -> String {
    match permission_mode {
        Some(mode) => format!(" with `--permission-mode {mode}`"),
        None => String::new(),
    }
}

/// Best-effort fetch plus default-branch worktree update, so every launch is a
/// chance to keep the local default-branch checkout fresh. The launcher works
/// without a config (and offline) in the existing-worktree path, so every
/// failure here degrades to a warning rather than blocking the launch.
fn refresh_main_repo(owner: &str, repo: &str) {
    let located = match config::find() {
        Ok(Some(located)) => located,
        Ok(None) => {
            // No config means no main repo to refresh; stay quiet.
            return;
        }
        Err(err) => {
            eprintln!("warning: skipping the fetch — failed to load the config: {err:#}");
            return;
        }
    };
    let main_repo = located.main_repo_path();
    println!("Fetching origin in `{}`…", main_repo.display());
    if let Err(err) = git::fetch(&main_repo) {
        eprintln!("warning: fetch failed: {err:#}");
        return;
    }
    let default = match github::default_branch(owner, repo) {
        Ok(default) => default,
        Err(err) => {
            eprintln!(
                "warning: skipping the default-branch update — \
                 failed to resolve the default branch: {err:#}"
            );
            return;
        }
    };
    prep::update_default_worktree(&main_repo, &default);
}

/// The session recorded for this worktree, if its transcript still exists under
/// `claude_dir` (otherwise there is nothing `claude --resume` could load).
fn resumable_session(
    claude_dir: &Path,
    issue_state: &IssueState,
    worktree: &Path,
) -> Option<String> {
    let id = issue_state
        .prep
        .as_ref()
        .and_then(|p| p.worktree_session_id.clone())?;
    let transcript = transcript_path(claude_dir, worktree, &id);
    if transcript.is_file() {
        Some(id)
    } else {
        println!(
            "This worktree's recorded session ({id}) has no transcript at `{}`; \
             it can't be resumed.",
            transcript.display()
        );
        None
    }
}

/// Path to the transcript Claude Code keeps for `session_id` launched in `dir`:
/// `<claude_dir>/projects/<munged dir>/<session_id>.jsonl`.
fn transcript_path(claude_dir: &Path, dir: &Path, session_id: &str) -> PathBuf {
    // Claude Code keys project directories by the session's canonical cwd.
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    claude_dir
        .join("projects")
        .join(munge(&canonical))
        .join(format!("{session_id}.jsonl"))
}

/// Claude Code names a project directory after the session's working directory
/// with every non-alphanumeric character (`/`, `.`, `_`, …) replaced by `-`.
fn munge(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Replace this process with an interactive `claude`, launched in `dir` when
/// given, resuming `resume` when given, in `permission_mode` when given (the
/// value is passed through verbatim; claude rejects invalid modes itself).
/// `issue_url` is exported as $GHWF_ISSUE so ghwf commands inside the session
/// can default to it.
///
/// The session starts itself by passing `/work-on` as a positional initial
/// prompt. That keeps it interactive (the user stays in the prompt loop) and
/// subscription-billed — only `-p`/`--print` is the headless/programmatic mode
/// that bills separately, and we never use it. The auto-start makes the whole
/// flow drivable from a phone: no one has to type `/work-on` to get going.
/// Because we exec rather than spawn, quitting Claude returns the user to the
/// shell that ran ghwf.
fn exec_claude(
    dir: Option<&Path>,
    resume: Option<&str>,
    permission_mode: Option<&str>,
    issue_url: &str,
) -> Result<()> {
    let mut cmd = Command::new("claude");
    cmd.env(store::ISSUE_ENV, issue_url);
    if let Some(id) = resume {
        cmd.args(["--resume", id]);
    }
    if let Some(mode) = permission_mode {
        cmd.args(["--permission-mode", mode]);
    }
    // The initial prompt goes last, after any flags, so it is the positional
    // argument claude treats as the first user message.
    cmd.arg("/work-on");
    if let Some(dir) = dir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("failed to change directory to `{}`", dir.display()))?;
    }
    // Everything printed so far must land before the terminal is handed over.
    std::io::stdout().flush().ok();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // exec only returns on failure.
        let err = cmd.exec();
        Err(err).context("failed to launch `claude` — is it installed and on PATH?")
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .context("failed to launch `claude` — is it installed and on PATH?")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::{munge, resumable_session, transcript_path};
    use crate::state::{IssueState, PrepState};
    use std::path::{Path, PathBuf};

    #[test]
    fn munge_replaces_non_alphanumerics() {
        // Mirrors real entries under ~/.claude/projects/: `/`, `.`, and `_` all
        // become `-`.
        assert_eq!(
            munge(Path::new("/Users/jeff/Projects/ghwf/repo.git")),
            "-Users-jeff-Projects-ghwf-repo-git"
        );
        assert_eq!(
            munge(Path::new("/tmp/worktrees/issue_2_foo")),
            "-tmp-worktrees-issue-2-foo"
        );
    }

    #[test]
    fn transcript_path_layout() {
        // A nonexistent dir can't canonicalize, so the path is munged as-is.
        let path = transcript_path(
            Path::new("/home/u/.claude"),
            Path::new("/nonexistent/wt_1"),
            "abc-123",
        );
        assert_eq!(
            path,
            Path::new("/home/u/.claude/projects/-nonexistent-wt-1/abc-123.jsonl")
        );
    }

    /// Issue state recording `session` for a worktree.
    fn state_with_session(worktree: &Path, session: &str) -> IssueState {
        IssueState {
            prep: Some(PrepState {
                worktree_path: Some(worktree.to_path_buf()),
                worktree_session_id: Some(session.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A unique scratch directory; tests build a fake Claude dir inside it.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ghwf-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resume_when_transcript_exists() {
        let root = scratch("resume");
        let worktree = root.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let claude_dir = root.join("claude");
        let transcript = transcript_path(&claude_dir, &worktree, "sess-1");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}").unwrap();

        let state = state_with_session(&worktree, "sess-1");
        assert_eq!(
            resumable_session(&claude_dir, &state, &worktree),
            Some("sess-1".to_string())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn no_resume_when_transcript_missing() {
        let root = scratch("no-resume");
        let worktree = root.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let claude_dir = root.join("claude");

        let state = state_with_session(&worktree, "sess-1");
        assert_eq!(resumable_session(&claude_dir, &state, &worktree), None);
        // No recorded session at all.
        assert_eq!(
            resumable_session(&claude_dir, &IssueState::default(), &worktree),
            None
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
