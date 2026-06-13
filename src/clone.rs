use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::{git, github};

/// Set up ghwf's preferred layout by cloning: a container directory holding
/// the bare repo (as `<name>.git`), a generated `ghwf.toml`, and an empty
/// worktrees directory.
pub fn run(repo: &str, directory: Option<&Path>, reference: Option<&Path>) -> Result<()> {
    let (url, name) = resolve(repo, &github::git_protocol())?;
    let container = match directory {
        Some(dir) => dir.to_path_buf(),
        None => PathBuf::from(&name),
    };
    let default_worktree = clone_into(&container, &name, &url, reference)?;
    report(&container, &name, default_worktree.as_deref());
    Ok(())
}

/// Resolve the repo argument to `(clone URL, repo name)`. A URL-shaped
/// argument is used verbatim; anything else is treated as `owner/repo`
/// shorthand and expanded with the given protocol.
fn resolve(repo: &str, protocol: &str) -> Result<(String, String)> {
    if repo.contains("://") || repo.starts_with("git@") {
        let (_, name) = github::parse_remote_url(repo)?;
        return Ok((repo.to_string(), name));
    }
    match repo.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            let name = name.strip_suffix(".git").unwrap_or(name);
            Ok((shorthand_url(protocol, owner, name), name.to_string()))
        }
        _ => bail!("`{repo}` is not an `owner/repo` shorthand or a GitHub URL"),
    }
}

/// The clone URL for an `owner/repo` shorthand: SSH when that's the user's
/// preferred protocol, HTTPS otherwise.
fn shorthand_url(protocol: &str, owner: &str, repo: &str) -> String {
    match protocol {
        "ssh" => format!("git@github.com:{owner}/{repo}.git"),
        _ => format!("https://github.com/{owner}/{repo}.git"),
    }
}

/// Create the container directory and populate it, removing the container on
/// failure when this run created it — a failed run leaves nothing
/// half-built behind. Returns the default branch when its worktree was created.
fn clone_into(
    container: &Path,
    name: &str,
    url: &str,
    reference: Option<&Path>,
) -> Result<Option<String>> {
    let created = create_container(container)?;
    let result = populate(container, name, url, reference);
    if result.is_err() && created {
        // Best-effort; the error the user sees is the populate failure.
        let _ = std::fs::remove_dir_all(container);
    }
    result
}

/// Create the container directory, accepting an existing *empty* directory
/// (matching `git clone`'s rule). Returns whether this run created it.
fn create_container(container: &Path) -> Result<bool> {
    match std::fs::create_dir(container) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let is_empty_dir = container.is_dir()
                && container
                    .read_dir()
                    .with_context(|| format!("failed to read `{}`", container.display()))?
                    .next()
                    .is_none();
            if !is_empty_dir {
                bail!(
                    "`{}` already exists and is not an empty directory",
                    container.display()
                );
            }
            Ok(false)
        }
        Err(err) => Err(err)
            .with_context(|| format!("failed to create directory `{}`", container.display())),
    }
}

/// Everything after URL resolution: clone the bare repo, make its remote
/// behave like a normal clone's, and generate the config and worktrees
/// directory. Returns the default branch when its worktree was created.
///
/// Shared with `convert`, which builds the same layout from an existing clone.
pub(crate) fn populate(
    container: &Path,
    name: &str,
    url: &str,
    reference: Option<&Path>,
) -> Result<Option<String>> {
    let bare = container.join(format!("{name}.git"));
    git::clone_bare(url, &bare, reference)?;
    git::setup_conventional_remote(&bare)?;
    std::fs::write(container.join("ghwf.toml"), config_text(name))
        .context("failed to write ghwf.toml")?;
    std::fs::create_dir(container.join("worktrees"))
        .context("failed to create the worktrees directory")?;
    Ok(create_default_worktree(container, &bare))
}

/// Best-effort: check out the default branch into `worktrees/<default>`, so a
/// fresh clone has a ready place to view and update the default branch (and so
/// `prep::update_default_worktree` has a checkout to keep current on later
/// fetches). Returns the default branch on success, or `None` after warning —
/// a failure here is not a clone failure, since the bare repo and config are
/// already in place.
fn create_default_worktree(container: &Path, bare: &Path) -> Option<String> {
    match try_create_default_worktree(container, bare) {
        Ok(default) => Some(default),
        Err(err) => {
            eprintln!("warning: failed to create the default-branch worktree: {err:#}");
            None
        }
    }
}

/// The fallible mechanics of [`create_default_worktree`].
fn try_create_default_worktree(container: &Path, bare: &Path) -> Result<String> {
    let default = git::default_remote_branch(bare)?;
    let worktree = container.join("worktrees").join(&default);
    git::add_worktree_for_branch(bare, &worktree, &default)?;
    Ok(default)
}

/// The generated `ghwf.toml`: essentials only. `ghwf config init` offers the
/// extras.
fn config_text(name: &str) -> String {
    format!("main_repo = \"{name}.git\"\nworktrees_dir = \"worktrees\"\n")
}

/// Describe the created layout and what to do next. `default_worktree` is the
/// default branch when its worktree was created under `worktrees/`.
fn report(container: &Path, name: &str, default_worktree: Option<&str>) {
    println!(
        "Created ghwf's preferred layout in `{}`:",
        container.display()
    );
    println!("- `{name}.git` — the bare repo");
    println!("- `ghwf.toml` — the essentials (`main_repo`, `worktrees_dir`)");
    println!("- `worktrees/` — per-issue worktrees are created here");
    if let Some(default) = default_worktree {
        println!("  - `{default}/` — a checkout of the default branch, ready to use");
    }
    println!();
    println!("Next steps:");
    println!("- `cd {}`", container.display());
    println!(
        "- `ghwf config init` to set up the optional extras \
         (priority labels, PR instructions, workflow status labels)"
    );
    println!("- `ghwf work-on <issue>` (or `ghwf next`) to start working");
}

#[cfg(test)]
mod tests {
    use super::{clone_into, config_text, create_container, populate, resolve, shorthand_url};
    use crate::git::tests::{git_stdout, init_repo, run_git, scratch};
    use std::path::{Path, PathBuf};

    #[test]
    fn shorthand_urls_follow_protocol() {
        assert_eq!(
            shorthand_url("https", "owner", "repo"),
            "https://github.com/owner/repo.git"
        );
        assert_eq!(
            shorthand_url("ssh", "owner", "repo"),
            "git@github.com:owner/repo.git"
        );
        // Anything unrecognized falls back to HTTPS.
        assert_eq!(
            shorthand_url("", "owner", "repo"),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn resolve_passes_urls_through_and_derives_the_name() {
        for (arg, name) in [
            ("https://github.com/owner/repo", "repo"),
            ("https://github.com/owner/repo.git", "repo"),
            ("git@github.com:owner/repo.git", "repo"),
        ] {
            let (url, derived) = resolve(arg, "https").unwrap();
            assert_eq!(url, arg);
            assert_eq!(derived, name);
        }
    }

    #[test]
    fn resolve_expands_shorthand() {
        let (url, name) = resolve("owner/repo", "ssh").unwrap();
        assert_eq!(url, "git@github.com:owner/repo.git");
        assert_eq!(name, "repo");
        // A `.git` suffix on the shorthand is tolerated, not doubled.
        let (url, name) = resolve("owner/repo.git", "https").unwrap();
        assert_eq!(url, "https://github.com/owner/repo.git");
        assert_eq!(name, "repo");
    }

    #[test]
    fn resolve_rejects_garbage() {
        for arg in ["no-slash", "/repo", "owner/", "owner/extra/repo", ""] {
            assert!(resolve(arg, "https").is_err(), "`{arg}` should not resolve");
        }
        // A non-GitHub URL errors rather than cloning somewhere surprising.
        assert!(resolve("https://gitlab.com/owner/repo", "https").is_err());
    }

    #[test]
    fn generated_config_parses_as_a_ghwf_config() {
        let config: crate::config::Config = toml::from_str(&config_text("repo")).unwrap();
        assert_eq!(config.main_repo, Some(PathBuf::from("repo.git")));
        assert_eq!(config.worktrees_dir, PathBuf::from("worktrees"));
    }

    /// A fixture origin repo with a second branch besides `main`.
    fn fixture_origin(root: &Path) -> PathBuf {
        let origin = root.join("origin");
        std::fs::create_dir(&origin).unwrap();
        init_repo(&origin);
        run_git(&origin, &["branch", "extra"]);
        origin
    }

    #[test]
    fn populate_builds_a_working_layout() {
        let root = scratch("clone-layout");
        let origin = fixture_origin(&root);

        let container = root.join("project");
        std::fs::create_dir(&container).unwrap();
        let default = populate(&container, "project", origin.to_str().unwrap(), None).unwrap();
        assert_eq!(default.as_deref(), Some("main"));

        let bare = container.join("project.git");
        assert_eq!(
            git_stdout(&bare, &["rev-parse", "--is-bare-repository"]),
            "true"
        );
        // Remote-tracking refs exist exactly as after a working-copy clone.
        for rev in [
            "refs/remotes/origin/main",
            "refs/remotes/origin/extra",
            "refs/remotes/origin/HEAD",
        ] {
            assert!(crate::git::rev_parse_ok(&bare, rev).is_some(), "{rev}");
        }
        // `--single-branch` kept local branches down to just the default.
        assert_eq!(crate::git::list_local_branches(&bare).unwrap(), ["main"]);
        assert_eq!(
            git_stdout(&bare, &["config", "remote.origin.fetch"]),
            "+refs/heads/*:refs/remotes/origin/*"
        );
        // The default branch is checked out into `worktrees/main`: a normal
        // (non-bare) checkout that git associates with the `main` branch.
        let default_worktree = container.join("worktrees").join("main");
        assert!(default_worktree.join("file.txt").is_file());
        assert_eq!(
            git_stdout(&default_worktree, &["rev-parse", "--is-bare-repository"]),
            "false"
        );
        assert_eq!(
            crate::git::branch_worktree(&bare, "main")
                .unwrap()
                .unwrap()
                .canonicalize()
                .unwrap(),
            default_worktree.canonicalize().unwrap()
        );
        // The operation prep-and-plan actually performs works.
        let worktree = container.join("worktrees").join("issue_1");
        crate::git::add_worktree(&bare, &worktree, "issue_1", "origin/main").unwrap();
        assert!(worktree.join("file.txt").is_file());
        // The generated config is in place and loadable.
        let text = std::fs::read_to_string(container.join("ghwf.toml")).unwrap();
        assert!(toml::from_str::<crate::config::Config>(&text).is_ok());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reference_clone_succeeds_and_dissociates() {
        let root = scratch("clone-reference");
        let origin = fixture_origin(&root);
        run_git(&root, &["clone", "origin", "reference"]);

        let container = root.join("project");
        std::fs::create_dir(&container).unwrap();
        populate(
            &container,
            "project",
            origin.to_str().unwrap(),
            Some(&root.join("reference")),
        )
        .unwrap();

        let bare = container.join("project.git");
        assert!(crate::git::rev_parse_ok(&bare, "refs/remotes/origin/main").is_some());
        // Dissociated: no lingering dependence on the reference repo.
        assert!(!bare.join("objects/info/alternates").exists());

        // A missing reference repo is a pre-clone error.
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        let missing = root.join("nonexistent");
        assert!(populate(&other, "other", origin.to_str().unwrap(), Some(&missing)).is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn container_rules_and_failure_cleanup() {
        let root = scratch("clone-container");
        let origin = fixture_origin(&root);
        let url = origin.to_str().unwrap();

        // An existing empty directory is fine.
        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        clone_into(&empty, "empty", url, None).unwrap();
        assert!(empty.join("empty.git").is_dir());

        // A non-empty directory errors without touching its contents.
        let occupied = root.join("occupied");
        std::fs::create_dir(&occupied).unwrap();
        std::fs::write(occupied.join("keep.txt"), "x\n").unwrap();
        assert!(create_container(&occupied).is_err());
        assert!(occupied.join("keep.txt").is_file());

        // A file in the way errors too.
        let file = root.join("file");
        std::fs::write(&file, "x\n").unwrap();
        assert!(create_container(&file).is_err());

        // A failed clone removes the container this run created…
        let fresh = root.join("fresh");
        assert!(clone_into(&fresh, "fresh", "this-url-does-not-exist", None).is_err());
        assert!(!fresh.exists());

        // …but leaves a pre-existing (empty) one in place.
        let kept = root.join("kept");
        std::fs::create_dir(&kept).unwrap();
        assert!(clone_into(&kept, "kept", "this-url-does-not-exist", None).is_err());
        assert!(kept.is_dir());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
