use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::Connection;

use crate::chromium::snapshot_file;
use crate::config::{TOKEN_KIND_COOKIE, TOKEN_KIND_LOCAL_STORAGE, TokenSourceConfig};
use crate::settings::BrowserPreset;

pub const MASKED_PREFIX: usize = 8;
pub const MASKED_SUFFIX: usize = 4;

#[derive(Debug, Clone)]
pub struct Token {
    pub key: String,
    pub value: String,
}

impl Token {
    pub fn masked(&self) -> String {
        mask(&self.value)
    }
}

/// Firefox localStorage values for chat.deepseek.com are JSON wrappers like
/// `{"value": "..."}` (nested one more level in some keys) - unwrap them.
pub(crate) fn unwrap_local_storage_value(value: String) -> String {
    let mut current = value;
    for _ in 0..2 {
        if !current.starts_with('{') {
            break;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&current) else {
            break;
        };
        match parsed.get("value") {
            Some(serde_json::Value::String(inner)) => current = inner.clone(),
            Some(serde_json::Value::Object(_)) => {
                let Some(serde_json::Value::String(inner)) = parsed["value"].get("value") else {
                    break;
                };
                current = inner.clone();
            }
            _ => break,
        }
    }
    current
}

pub fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= MASKED_PREFIX + MASKED_SUFFIX {
        return "*".repeat(chars.len().max(3));
    }
    let head: String = chars[..MASKED_PREFIX].iter().collect();
    let tail: String = chars[chars.len() - MASKED_SUFFIX..].iter().collect();
    format!("{head}\u{2026}{tail} ({} chars)", chars.len())
}

pub fn read_token(
    profile_dir: &Path,
    preset: BrowserPreset,
    source: &TokenSourceConfig,
) -> Result<Token> {
    match source.kind.as_str() {
        TOKEN_KIND_LOCAL_STORAGE => {
            let key = source
                .keys
                .first()
                .ok_or_else(|| anyhow!("token.keys is empty for localStorage source"))?;
            match preset {
                BrowserPreset::Firefox => read_local_storage_token(profile_dir, &source.db_path, key),
                BrowserPreset::Chrome | BrowserPreset::Yandex => {
                    if source.origin_host.trim().is_empty() {
                        bail!("token.origin_host is empty, cannot look up Chromium localStorage");
                    }
                    crate::chromium::read_local_storage(profile_dir, &source.origin_host, key)
                }
            }
        }
        TOKEN_KIND_COOKIE => {
            let name = source
                .keys
                .first()
                .ok_or_else(|| anyhow!("token.keys is empty for cookie source"))?;
            match preset {
                BrowserPreset::Firefox => {
                    read_firefox_cookie(profile_dir, &source.host_pattern, name)
                }
                BrowserPreset::Chrome | BrowserPreset::Yandex => {
                    crate::chromium::read_cookie(profile_dir, &source.host_pattern, name)
                }
            }
        }
        other => bail!("unknown token source kind: '{other}'"),
    }
}

/// Reads a plaintext cookie from a Firefox profile (`cookies.sqlite`).
fn read_firefox_cookie(profile_dir: &Path, host_pattern: &str, name: &str) -> Result<Token> {
    let db_path = profile_dir.join("cookies.sqlite");
    if !db_path.is_file() {
        bail!("cookies.sqlite not found: {}", db_path.display());
    }
    let snapshot = snapshot_file(&db_path, "cookies.sqlite")?;
    let conn = Connection::open_with_flags(&snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open snapshot {}", snapshot.display()))?;

    let mut query = conn
        .prepare("SELECT host, name, value FROM moz_cookies WHERE name = ?1")
        .context("failed to query moz_cookies")?;
    let mut rows = query.query([name]).context("cookie query failed")?;
    while let Some(row) = rows.next().context("cookie row failed")? {
        let host: String = row.get(0).unwrap_or_default();
        if !crate::chromium::host_matches(&host, host_pattern) {
            continue;
        }
        let value: String = row.get(2).unwrap_or_default();
        if value.is_empty() {
            continue;
        }
        let _ = fs::remove_file(&snapshot);
        return Ok(Token {
            key: name.to_owned(),
            value,
        });
    }
    bail!("cookie '{name}' for '{host_pattern}' not found")
}

fn read_local_storage_token(profile_dir: &Path, db_rel: &str, key: &str) -> Result<Token> {
    if db_rel.is_empty() {
        bail!("token.db_path is empty for localStorage source");
    }
    let db_path = profile_dir.join(db_rel);
    if !db_path.is_file() {
        bail!(
            "localStorage database not found: {}",
            db_path.display()
        );
    }

    let temp_dir = snapshot_db(&db_path).context("failed to snapshot localStorage database")?;
    let snapshot = temp_dir.join("data.sqlite");

    let result = (|| -> Result<Token> {
        let conn =
            Connection::open_with_flags(&snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("failed to open snapshot {}", snapshot.display()))?;

        let (raw, utf16_length, conversion_type, compression_type): (Vec<u8>, i64, i64, i64) = conn
            .query_row(
                "SELECT value, utf16_length, conversion_type, compression_type \
                 FROM data WHERE key = ?1",
                [key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    anyhow!("key '{key}' not found in localStorage database")
                }
                other => anyhow!("failed to query key '{key}': {other}"),
            })?;

        if compression_type != 0 {
            bail!(
                "value of '{key}' is LZ4-compressed (compression_type={compression_type}), \
                 which is not supported yet"
            );
        }

        let value = match conversion_type {
            1 => raw.iter().map(|&b| b as char).collect::<String>(),
            0 => {
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                String::from_utf16(&units)
                    .map_err(|e| anyhow!("invalid UTF-16 value for '{key}': {e}"))?
            }
            other => bail!("unknown conversion_type={other} for key '{key}'"),
        };

        let _ = utf16_length;
        let value = unwrap_local_storage_value(value.trim().to_owned());
        if value.trim().is_empty() {
            bail!("value of '{key}' is empty");
        }
        Ok(Token {
            key: key.to_owned(),
            value: value.trim().to_owned(),
        })
    })();

    let _ = fs::remove_dir_all(&temp_dir);
    result
}

/// Copies the SQLite database (plus -wal/-shm sidecars) into a temporary
/// directory so we can read it even while Firefox holds a lock on the original.
fn snapshot_db(db_path: &Path) -> Result<PathBuf> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!(
        "firecode_auth_{}_{}",
        std::process::id(),
        seq
    ));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;

    fs::copy(db_path, temp_dir.join("data.sqlite"))
        .with_context(|| format!("failed to copy {}", db_path.display()))?;
    for sidecar in ["-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{sidecar}", db_path.display()));
        if src.is_file() {
            let _ = fs::copy(&src, temp_dir.join(format!("data.sqlite{sidecar}")));
        }
    }
    Ok(temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_hides_middle_of_long_tokens() {
        let masked = mask("abcdefghij1234567890");
        assert!(masked.starts_with("abcdefgh\u{2026}"));
        assert!(masked.ends_with("7890 (20 chars)"));
        assert!(!masked.contains("ij1234"));
    }

    #[test]
    fn mask_short_values_fully() {
        assert_eq!(mask("abc"), "***");
    }

    #[test]
    fn latin1_decoding_matches_ascii() {
        let bytes: Vec<u8> = "eyJhbGciOiJIUzQ".as_bytes().to_vec();
        let decoded: String = bytes.iter().map(|&b| b as char).collect();
        assert_eq!(decoded, "eyJhbGciOiJIUzQ");
    }

    #[test]
    fn unwrap_extracts_token_from_json_wrapper() {
        assert_eq!(
            unwrap_local_storage_value(r#"{"value":"B2abc123"}"#.to_owned()),
            "B2abc123"
        );
        assert_eq!(unwrap_local_storage_value("rawtoken".to_owned()), "rawtoken");
        assert_eq!(
            unwrap_local_storage_value(
                r#"{"value":{"orig":"inner"}}"#.to_owned()
            ),
            r#"{"value":{"orig":"inner"}}"#
        );
    }

    /// Verification against the real Firefox profile on this machine.
    /// Run explicitly: cargo test --release -- --ignored
    #[test]
    #[ignore = "requires the real Firefox profile on the dev machine"]
    fn real_profile_token_is_readable() {
        let (config, path) = crate::config::load_or_create().expect("config load failed");
        println!("config: {}", path.display());
        let preset = crate::settings::BrowserPreset::Firefox;
        let profile = crate::config::resolve_profile_dir(&config, preset);
        let site = config.site(&config.default_site).expect("default site");
        let token = read_token(&profile, preset, &site.token).expect("token read failed");
        println!("site '{}', key '{}': {}", config.default_site, token.key, token.masked());
        assert!(token.value.len() > 20);
        assert!(
            !token.value.starts_with('{'),
            "token must be unwrapped from the localStorage JSON wrapper"
        );
    }
}
