use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

/// Environment variable Claude Code injects into spawned tools, holding the
/// current session UUID.
pub const SESSION_ID_ENV: &str = "CLAUDE_CODE_SESSION_ID";

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
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(token);
    fs::write(&path, session_id)
        .with_context(|| format!("failed to record session mapping at {}", path.display()))
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
