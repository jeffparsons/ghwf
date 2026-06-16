use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

/// Environment variable Claude Code injects into spawned tools, holding the
/// current session UUID.
pub const SESSION_ID_ENV: &str = "CLAUDE_CODE_SESSION_ID";

/// Environment variable the outside-Claude launcher sets on the `claude` it
/// execs, holding the canonical URL of the issue being worked. Commands run
/// inside that session fall back to it when no issue argument is given.
pub const ISSUE_ENV: &str = "GHWF_ISSUE";

/// Per-user data directory for ghwf, e.g. `~/Library/Application Support/ghwf`
/// on macOS, `~/.local/share/ghwf` on Linux. Created if absent.
pub(crate) fn data_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "ghwf")
        .ok_or_else(|| anyhow!("could not determine a home directory for ghwf's data"))?;
    let dir = dirs.data_local_dir().to_path_buf();
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create data directory {}", dir.display()))?;
    Ok(dir)
}

/// Claude Code's per-user directory: `$CLAUDE_CONFIG_DIR` when set, else
/// `~/.claude`.
pub(crate) fn claude_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine a home directory"))?;
    Ok(base.home_dir().join(".claude"))
}

/// Load the persistent salt, generating and storing it on first use.
///
/// The salt makes the session token unguessable from the session id alone.
fn load_or_create_salt() -> Result<String> {
    let path = data_dir()?.join("salt");
    if let Ok(salt) = fs::read_to_string(&path) {
        return Ok(salt);
    }

    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow!("failed to gather randomness for the salt: {e}"))?;
    let salt = hex(&bytes);
    fs::write(&path, &salt)
        .with_context(|| format!("failed to write salt to {}", path.display()))?;
    Ok(salt)
}

/// Compute the opaque session token (a salted hash of the session id) and record
/// the reverse mapping so the token can later be resolved back to the session.
pub fn session_token(session_id: &str) -> Result<String> {
    let salt = load_or_create_salt()?;

    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(session_id.as_bytes());
    // 16 hex chars (64 bits) is ample to avoid collisions for one person's work.
    let token: String = hex(&hasher.finalize()).chars().take(16).collect();

    record_session(&token, session_id)?;
    Ok(token)
}

/// Record `token -> session_id` so a token read back off a comment can be mapped
/// to the session that authored it. The reverse direction is just a recompute.
fn record_session(token: &str, session_id: &str) -> Result<()> {
    let dir = data_dir()?.join("sessions");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(token);
    fs::write(&path, session_id)
        .with_context(|| format!("failed to record session mapping at {}", path.display()))
}

/// Atomically replace `path`'s contents by writing a sibling temp file and
/// renaming it into place, so a concurrent reader — or a crash mid-write —
/// never sees a half-written file. The temp name carries our pid so writers in
/// different processes don't collide on it. `path`'s parent directory must
/// already exist.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to install {}", path.display()))
}

/// A short content hash used to detect changes to an issue body or comment.
///
/// 16 hex chars (64 bits) is plenty to detect edits; this is change-detection,
/// not a security boundary.
pub fn content_hash(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex(&hasher.finalize()).chars().take(16).collect()
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
    use super::atomic_write;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ghwf-store-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_replaces_contents() {
        let dir = scratch("atomic-write");
        let path = dir.join("v.json");
        atomic_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn atomic_write_leaves_no_temp_sibling() {
        let dir = scratch("atomic-write-tmp");
        let path = dir.join("w.json");
        atomic_write(&path, b"payload").unwrap();
        // The temp file is renamed into place, so only the target remains.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
    }
}
