//! Uploading local files as GitHub comment attachments.
//!
//! GitHub has no token-authenticated API for the inline attachment CDN the web
//! UI uses, so we commit each file into the repo (on a dedicated, orphan-history
//! branch so it never touches the working branch or a PR diff) via the Git Data
//! API and reference it from the comment body. On a public repo, images embed
//! inline via a `?raw=true` blob link and videos via an HTML `<video>` tag
//! pointing at the raw bytes. Audio and everything else — and every attachment
//! on a private repo, whose blob links are auth-gated and so won't render
//! inline — is a plain link.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::github;

/// The branch attachments are committed to. Its history is orphaned (the first
/// commit has no parents) so it shares nothing with the code branches.
const BRANCH: &str = "ghwf-attachments";

/// Soft per-file cap. Base64-in-JSON is heavy and the blobs API has limits;
/// comment attachments are screenshots/logs, not large binaries.
const MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Image extensions GitHub renders inline from a `?raw=true` blob link.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "apng"];

/// Video extensions GitHub plays inline from an HTML `<video>` tag. GitHub's
/// markdown sanitizer keeps `<video controls src="…">` (verified against its
/// `/markdown` API) but strips `<audio>`, so audio has no inline form and falls
/// through to a link.
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "webm"];

/// How an attachment can be presented in a comment, decided by file extension.
enum MediaKind {
    /// Embeds inline via `![](…?raw=true)` on a public repo.
    Image,
    /// Embeds inline via an HTML `<video>` tag on a public repo.
    Video,
    /// No inline form GitHub supports; always a plain link.
    Other,
}

/// A file read and ready to upload.
struct Prepared {
    /// The original basename, used as the link's display text.
    name: String,
    /// The collision-safe path under the attachments branch.
    repo_path: String,
    content_base64: String,
}

/// Upload `paths` into `(owner, repo)` and return the markdown trailer to append
/// to a comment body, or `None` when there are no paths. Uploading happens
/// before the caller posts, so a failure here never leaves a comment with broken
/// links.
pub fn upload(owner: &str, repo: &str, issue: u64, paths: &[PathBuf]) -> Result<Option<String>> {
    if paths.is_empty() {
        return Ok(None);
    }

    let prepared = prepare(issue, paths)?;

    // Blobs are content-addressed and branch-independent, so they're created
    // once and reused even if the ref update has to retry.
    let mut entries = Vec::with_capacity(prepared.len());
    for p in &prepared {
        let sha = github::create_blob(owner, repo, &p.content_base64)?;
        entries.push(github::TreeEntry {
            path: p.repo_path.clone(),
            sha,
        });
    }
    commit_to_branch(owner, repo, &entries)?;

    let private = github::repo_is_private(owner, repo)?;
    let lines: Vec<String> = prepared
        .iter()
        .map(|p| {
            let blob_url = format!(
                "https://github.com/{owner}/{repo}/blob/{BRANCH}/{}",
                p.repo_path
            );
            let raw_url = format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{BRANCH}/{}",
                p.repo_path
            );
            attachment_markdown(&p.name, &blob_url, &raw_url, media_kind(&p.name), private)
        })
        .collect();
    Ok(Some(build_trailer(&lines)))
}

/// Read and validate each path, dropping later duplicates that resolve to the
/// same repo path (identical content and name).
fn prepare(issue: u64, paths: &[PathBuf]) -> Result<Vec<Prepared>> {
    let mut prepared = Vec::with_capacity(paths.len());
    let mut seen = HashSet::new();
    for path in paths {
        let meta = fs::metadata(path)
            .with_context(|| format!("cannot read attachment `{}`", path.display()))?;
        if !meta.is_file() {
            bail!("attachment `{}` is not a regular file", path.display());
        }
        if meta.len() > MAX_BYTES {
            bail!(
                "attachment `{}` is {} bytes, over the {MAX_BYTES}-byte limit",
                path.display(),
                meta.len()
            );
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                anyhow::anyhow!("attachment `{}` has no usable file name", path.display())
            })?
            .to_string();
        let bytes = fs::read(path)
            .with_context(|| format!("cannot read attachment `{}`", path.display()))?;
        let repo_path = repo_path(issue, &bytes, &name);
        if !seen.insert(repo_path.clone()) {
            continue;
        }
        let content_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        prepared.push(Prepared {
            name,
            repo_path,
            content_base64,
        });
    }
    Ok(prepared)
}

/// Commit `entries` onto the attachments branch in one commit, retrying once if
/// a concurrent writer moves the tip between read and update.
fn commit_to_branch(owner: &str, repo: &str, entries: &[github::TreeEntry]) -> Result<()> {
    let message = "Add comment attachment(s)";
    for attempt in 0..2 {
        let tip = github::get_branch_tip(owner, repo, BRANCH)?;
        let (base_tree, parents) = match &tip {
            Some((commit, tree)) => (Some(tree.as_str()), vec![commit.clone()]),
            None => (None, Vec::new()),
        };
        let tree = github::create_tree(owner, repo, base_tree, entries)?;
        let commit = github::create_commit(owner, repo, message, &tree, &parents)?;
        let updated = match tip {
            Some(_) => github::update_ref(owner, repo, BRANCH, &commit)?,
            // Branch absent: create it. If a concurrent writer created it first,
            // creation fails — loop and retry as a fast-forward update.
            None => match github::create_ref(owner, repo, BRANCH, &commit) {
                Ok(()) => true,
                Err(_) if attempt == 0 => false,
                Err(e) => return Err(e),
            },
        };
        if updated {
            return Ok(());
        }
    }
    bail!("could not update the `{BRANCH}` branch after a concurrent change; retry the attachment");
}

/// The collision-safe path for a file: keyed by issue and a short content hash,
/// so identical content lands on the same path (idempotent) and different files
/// sharing a name don't collide.
fn repo_path(issue: u64, content: &[u8], name: &str) -> String {
    let digest = Sha256::digest(content);
    let hash8: String = hex(&digest).chars().take(8).collect();
    format!("attachments/{issue}/{hash8}-{}", sanitize_filename(name))
}

/// Reduce a basename to a path-safe slug, keeping `[A-Za-z0-9._-]` and replacing
/// anything else with `_`.
fn sanitize_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "file".to_string()
    } else {
        s
    }
}

/// Classify a name by extension into the way it can be presented.
fn media_kind(name: &str) -> MediaKind {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext {
        Some(e) if IMAGE_EXTS.contains(&e.as_str()) => MediaKind::Image,
        Some(e) if VIDEO_EXTS.contains(&e.as_str()) => MediaKind::Video,
        _ => MediaKind::Other,
    }
}

/// One attachment's markdown. On a public repo an image embeds via the
/// `?raw=true` blob link and a video via an HTML `<video>` tag pointing at the
/// raw bytes (so the player can fetch and seek directly); everything else, and
/// every kind on a private repo (where those URLs are auth-gated), is a link.
fn attachment_markdown(
    name: &str,
    blob_url: &str,
    raw_url: &str,
    kind: MediaKind,
    private: bool,
) -> String {
    match kind {
        MediaKind::Image if !private => format!("![{name}]({blob_url}?raw=true)"),
        MediaKind::Video if !private => format!("<video controls src=\"{raw_url}\"></video>"),
        _ => format!("[{name}]({blob_url})"),
    }
}

/// Assemble the trailer appended to a comment body.
fn build_trailer(lines: &[String]) -> String {
    let bullets = lines
        .iter()
        .map(|l| format!("- {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("---\n\n**Attachments:**\n\n{bullets}")
}

/// Lower-case hex encoding of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_filename("my screenshot.png"), "my_screenshot.png");
        assert_eq!(sanitize_filename("a/b:c.txt"), "a_b_c.txt");
        assert_eq!(
            sanitize_filename("plain-name_1.0.log"),
            "plain-name_1.0.log"
        );
        assert_eq!(sanitize_filename(""), "file");
    }

    #[test]
    fn repo_path_is_deterministic_and_content_addressed() {
        let a = repo_path(80, b"hello", "shot.png");
        let b = repo_path(80, b"hello", "shot.png");
        assert_eq!(a, b);
        assert!(a.starts_with("attachments/80/"));
        assert!(a.ends_with("-shot.png"));
        // Different content changes the hash prefix.
        let c = repo_path(80, b"world", "shot.png");
        assert_ne!(a, c);
        // Different issue changes the directory.
        let d = repo_path(7, b"hello", "shot.png");
        assert!(d.starts_with("attachments/7/"));
    }

    #[test]
    fn media_kind_matches_known_extensions() {
        for n in ["a.png", "b.JPG", "c.jpeg", "d.gif", "e.WEBP", "f.svg"] {
            assert!(
                matches!(media_kind(n), MediaKind::Image),
                "{n} should be an image"
            );
        }
        for n in ["clip.mp4", "screen.MOV", "loop.webm"] {
            assert!(
                matches!(media_kind(n), MediaKind::Video),
                "{n} should be a video"
            );
        }
        // Audio has no inline form GitHub supports, so it classifies as Other.
        for n in ["voice.mp3", "tone.wav", "server.log", "patch.diff", "noext"] {
            assert!(
                matches!(media_kind(n), MediaKind::Other),
                "{n} should be other"
            );
        }
    }

    #[test]
    fn markdown_embeds_public_images_and_videos_only() {
        let blob = "https://github.com/o/r/blob/ghwf-attachments/attachments/80/ab12cd34-shot.png";
        let raw =
            "https://raw.githubusercontent.com/o/r/ghwf-attachments/attachments/80/ab12cd34-shot.png";
        // Image in a public repo: inline image.
        assert_eq!(
            attachment_markdown("shot.png", blob, raw, MediaKind::Image, false),
            format!("![shot.png]({blob}?raw=true)")
        );
        // Image in a private repo: link only (no `!`, no `?raw=true`).
        assert_eq!(
            attachment_markdown("shot.png", blob, raw, MediaKind::Image, true),
            format!("[shot.png]({blob})")
        );
        // Video in a public repo: a `<video>` tag pointing at the raw bytes,
        // never the blob page that broke playback.
        assert_eq!(
            attachment_markdown("clip.mp4", blob, raw, MediaKind::Video, false),
            format!("<video controls src=\"{raw}\"></video>")
        );
        // Video in a private repo: link only.
        assert_eq!(
            attachment_markdown("clip.mp4", blob, raw, MediaKind::Video, true),
            format!("[clip.mp4]({blob})")
        );
        // Audio (Other): link even on a public repo — GitHub strips `<audio>`.
        assert_eq!(
            attachment_markdown("voice.mp3", blob, raw, MediaKind::Other, false),
            format!("[voice.mp3]({blob})")
        );
    }

    #[test]
    fn trailer_lists_each_attachment() {
        let lines = vec![
            "![a.png](u1?raw=true)".to_string(),
            "[b.log](u2)".to_string(),
        ];
        let trailer = build_trailer(&lines);
        assert_eq!(
            trailer,
            "---\n\n**Attachments:**\n\n- ![a.png](u1?raw=true)\n- [b.log](u2)"
        );
    }
}
