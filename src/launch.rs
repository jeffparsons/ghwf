use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::state::{self, IssueState};
use crate::{github, prep, store};

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
    let (owner, repo, number) = github::resolve_issue_ref(issue_arg, repo_ctx.as_ref())?;
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

    // --no-branch work has no worktree to anchor a session to: launch a fresh
    // Claude where we are.
    if use_no_branch {
        println!(
            "Issue #{number} is being worked with --no-branch (no dedicated worktree), \
             so Claude will start in the current directory."
        );
        print_fresh_reminder(number);
        return exec_claude(None, None);
    }

    // Find or create the worktree. The launcher creates it immediately — even
    // before planning — so the session it starts is anchored there and stays
    // resumable across every phase.
    let worktree = match issue_state.prep.as_ref().and_then(|p| p.worktree_path.clone()) {
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
            path
        }
        None => {
            println!(
                "Issue #{number} has no worktree yet; creating it now so the Claude session \
                 is anchored there and can be resumed for later phases."
            );
            let issue_data = github::fetch_issue(issue_arg, repo_ctx.as_ref())?;
            let (path, branch) = prep::ensure_worktree(&issue_data, &owner, &repo, &mut issue_state)?;
            state::save(&owner, &repo, number, &issue_state)?;
            println!("Created worktree `{}` on branch `{branch}`.", path.display());
            path
        }
    };

    // Resume the worktree's recorded session if its transcript is still around.
    let resume = resumable_session(&claude_dir()?, &issue_state, &worktree);
    match &resume {
        Some(id) => println!(
            "Resuming this worktree's previous Claude session: launching \
             `claude --resume {id}` in `{}`.",
            worktree.display()
        ),
        None => {
            println!(
                "Starting a fresh Claude session in `{}`.",
                worktree.display()
            );
            print_fresh_reminder(number);
        }
    }
    exec_claude(Some(&worktree), resume.as_deref())
}

/// Remind the user how to kick off the workflow in a fresh session. The launcher
/// can't do it for them: passing Claude a prompt would be programmatic use,
/// billed as API traffic.
fn print_fresh_reminder(number: u64) {
    println!("Once Claude is up, run `/work-on {number}` to pick up the workflow.");
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

/// Claude Code's per-user directory: `$CLAUDE_CONFIG_DIR` when set, else
/// `~/.claude`.
fn claude_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine a home directory"))?;
    Ok(base.home_dir().join(".claude"))
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
/// given, resuming `resume` when given.
///
/// Never pass `-p`/`--print` (or any prompt): that is programmatic use, billed
/// as API traffic rather than the user's subscription. Because we exec rather
/// than spawn, quitting Claude returns the user to the shell that ran ghwf.
fn exec_claude(dir: Option<&Path>, resume: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("claude");
    if let Some(id) = resume {
        cmd.args(["--resume", id]);
    }
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
