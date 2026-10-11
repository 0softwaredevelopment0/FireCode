use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// One message as stored in the local chat cache. Roles include the
/// local-only ones ("command", "feedback") alongside "user"/"assistant",
/// so a restored chat looks exactly like the live session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedMessage {
    pub role: String,
    #[serde(default)]
    pub thinking: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedChat {
    pub messages: Vec<CachedMessage>,
    #[serde(default)]
    pub last_message_id: Option<i64>,
}

/// Cache lives next to config.toml / settings.toml, one JSON file per
/// DeepSeek session id.
fn cache_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "0softwaredevelopment0", "FireCode")
        .context("no project config directory")?;
    let dir = dirs.config_dir().join("chat-cache");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn cache_path(session_id: &str) -> Result<PathBuf> {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        bail!("empty session id");
    }
    Ok(cache_dir()?.join(format!("{safe}.json")))
}

/// Best-effort save: failures are reported on stderr but never fatal - a
/// broken cache must not take the chat down.
pub fn save(session_id: &str, chat: &CachedChat) {
    match cache_path(session_id) {
        Ok(path) => {
            let json = match serde_json::to_string_pretty(chat) {
                Ok(json) => json,
                Err(e) => {
                    eprintln!("chat cache serialize: {e}");
                    return;
                }
            };
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("chat cache write {}: {e}", path.display());
            }
        }
        Err(e) => eprintln!("chat cache path: {e:#}"),
    }
}

/// Returns None when no cache exists yet (the caller falls back to the
/// server history) or when the file is unreadable/corrupt.
pub fn load(session_id: &str) -> Option<CachedChat> {
    let path = cache_path(session_id).ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}
