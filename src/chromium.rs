use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::{
    Aes256Gcm,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::Connection;
use rusty_leveldb::{DB, LdbIterator, Options};

use crate::auth::Token;

/// Copies a file with its SQLite sidecars into a fresh temp dir.
pub(crate) fn snapshot_file(src: &Path, dest_name: &str) -> Result<PathBuf> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!(
        "firecode_chromium_{}_{}",
        std::process::id(),
        seq
    ));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;
    fs::copy(src, temp_dir.join(dest_name))
        .with_context(|| format!("failed to copy {}", src.display()))?;
    for sidecar in ["-wal", "-shm", "-journal"] {
        let sibling = PathBuf::from(format!("{}{sidecar}", src.display()));
        if sibling.is_file() {
            let _ = fs::copy(&sibling, temp_dir.join(format!("{dest_name}{sidecar}")));
        }
    }
    Ok(temp_dir.join(dest_name))
}

/// Decrypts DPAPI-protected bytes (e.g. Chrome's AES master key).
#[cfg(windows)]
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData};
    use windows_sys::core::BOOL;

    if data.is_empty() {
        bail!("dpapi: empty input");
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: input points at `data` which outlives the call; output is
    // allocated by CryptUnprotectData and freed with LocalFree below.
    let ok: BOOL = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    if ok == 0 {
        bail!("CryptUnprotectData failed");
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData as HLOCAL);
    }
    Ok(bytes)
}

#[cfg(not(windows))]
pub fn dpapi_unprotect(_data: &[u8]) -> Result<Vec<u8>> {
    bail!("dpapi is only available on Windows")
}

/// Extracts Chrome's AES-256 cookie key from `Local State`
/// (`os_crypt.encrypted_key`, base64, "DPAPI"-prefixed then DPAPI-wrapped).
pub fn read_master_key(profile_dir: &Path) -> Result<[u8; 32]> {
    let local_state = profile_dir
        .parent()
        .context("profile dir has no parent")?
        .join("Local State");
    let raw = fs::read_to_string(&local_state)
        .with_context(|| format!("Local State not found: {}", local_state.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&raw).context("failed to parse Local State")?;
    let encoded = json["os_crypt"]["encrypted_key"]
        .as_str()
        .context("Local State has no os_crypt.encrypted_key")?;
    let mut wrapped =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
            .context("failed to base64-decode the encrypted key")?;
    if wrapped.starts_with(b"DPAPI") {
        wrapped.drain(..5);
    }
    let key = dpapi_unprotect(&wrapped).context("failed to DPAPI-unprotect the key")?;
    if key.len() != 32 {
        bail!("unexpected master key length: {}", key.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&key);
    Ok(out)
}

/// Decrypts one Chrome cookie value: "v10"/"v11" prefix, 12-byte nonce,
/// AES-256-GCM ciphertext+tag.
pub fn decrypt_cookie_value(master_key: &[u8; 32], encrypted: &[u8]) -> Result<String> {
    if encrypted.len() < 3 + 12 + 16 {
        bail!("encrypted cookie value too short");
    }
    if &encrypted[..3] != b"v10" && &encrypted[..3] != b"v11" {
        bail!("unknown cookie encryption prefix");
    }
    let cipher = Aes256Gcm::new_from_slice(master_key).map_err(|e| anyhow!("bad key: {e}"))?;
    let nonce = aes_gcm::Nonce::try_from(&encrypted[3..15]).map_err(|_| anyhow!("bad nonce"))?;
    let plain = cipher
        .decrypt(&nonce, &encrypted[15..])
        .map_err(|_| anyhow!("AES-GCM decryption failed"))?;
    String::from_utf8(plain).context("cookie value is not UTF-8")
}

pub(crate) fn host_matches(db_host: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let host = db_host.trim_start_matches('.');
    host == pattern || host.ends_with(&format!(".{pattern}"))
}

/// Reads a cookie from a Chromium profile (`Network/Cookies`, fallback
/// `Cookies`). Plaintext `value` wins; otherwise the DPAPI/AES path runs.
pub fn read_cookie(
    profile_dir: &Path,
    host_pattern: &str,
    name: &str,
) -> Result<Token> {
    let db_path = ["Network/Cookies", "Cookies"]
        .iter()
        .map(|rel| profile_dir.join(rel))
        .find(|p| p.is_file())
        .with_context(|| {
            format!(
                "no Cookies database in {}",
                profile_dir.display()
            )
        })?;
    let snapshot = snapshot_file(&db_path, "Cookies")?;
    let conn = Connection::open_with_flags(&snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open snapshot {}", snapshot.display()))?;

    let mut query = conn
        .prepare("SELECT host_key, name, value, encrypted_value FROM cookies WHERE name = ?1")
        .context("failed to query cookies")?;
    let mut master_key: Option<[u8; 32]> = None;
    let mut rows = query.query([name]).context("cookie query failed")?;
    while let Some(row) = rows.next().context("cookie row failed")? {
        let host: String = row.get(0).unwrap_or_default();
        if !host_matches(&host, host_pattern) {
            continue;
        }
        let value: String = row.get(2).unwrap_or_default();
        if !value.is_empty() {
            let _ = fs::remove_file(&snapshot);
            return Ok(Token {
                key: name.to_owned(),
                value,
            });
        }
        let encrypted: Vec<u8> = row.get(3).unwrap_or_default();
        if encrypted.is_empty() {
            continue;
        }
        if master_key.is_none() {
            master_key = Some(read_master_key(profile_dir)?);
        }
        let value = decrypt_cookie_value(&master_key.expect("key checked above"), &encrypted)?;
        let _ = fs::remove_file(&snapshot);
        return Ok(Token {
            key: name.to_owned(),
            value,
        });
    }
    bail!("cookie '{name}' for '{host_pattern}' not found")
}

/// Copies a Chromium LevelDB directory (minus the lock) into a temp dir.
fn snapshot_leveldb(src: &Path) -> Result<PathBuf> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!(
        "firecode_leveldb_{}_{}",
        std::process::id(),
        seq
    ));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;
    for entry in fs::read_dir(src)
        .with_context(|| format!("leveldb dir not found: {}", src.display()))?
    {
        let entry = entry.context("leveldb entry failed")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "LOCK" {
            continue;
        }
        if entry.file_type().context("file type failed")?.is_file() {
            fs::copy(entry.path(), temp_dir.join(name.as_ref()))
                .with_context(|| format!("failed to copy {}", entry.path().display()))?;
            // newer Chrome names sorted tables *.sst while the MANIFEST still
            // references *.ldb - provide both names for LevelDB readers.
            if name.as_ref().ends_with(".sst") {
                let ldb_name = format!("{}.ldb", name.as_ref().trim_end_matches(".sst"));
                fs::copy(entry.path(), temp_dir.join(&ldb_name))
                    .with_context(|| format!("failed to copy {}", entry.path().display()))?;
            }
        }
    }
    Ok(temp_dir)
}

/// Scores decoded text: ASCII wins, control characters lose. This tells
/// a correct UTF-16LE decode apart from mojibake - any even-length byte
/// string "decodes" as UTF-16, e.g. ASCII bytes become CJK characters.
fn text_score(text: &str) -> i64 {
    let mut score = 0i64;
    for c in text.chars() {
        if c.is_ascii_graphic() || c == ' ' {
            score += 2;
        } else if c == '\t' || c == '\n' || c == '\r' {
            score += 1;
        } else if c.is_control() {
            score -= 10;
        } else {
            score += 1;
        }
    }
    score
}

/// Decodes a Chromium localStorage value. The on-disk shape varies (a type
/// prefix byte plus UTF-16LE or single-byte text), so every combination is
/// tried and the best-looking result wins.
fn decode_ls_value(raw: &[u8]) -> Option<String> {
    let utf16le = |bytes: &[u8]| -> Option<String> {
        if bytes.len() < 2 || bytes.len() % 2 != 0 {
            return None;
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).ok().map(|s| s.trim().to_owned())
    };
    let single_byte = |bytes: &[u8]| -> Option<String> {
        let text: String = bytes.iter().map(|&b| b as char).collect();
        let text = text.trim().to_owned();
        if text.is_empty() || text.chars().all(|c| c.is_control()) {
            None
        } else {
            Some(text)
        }
    };

    let mut candidates = Vec::new();
    if raw.len() > 1 {
        candidates.push(utf16le(&raw[1..]));
        candidates.push(single_byte(&raw[1..]));
    }
    candidates.push(utf16le(raw));
    candidates.push(single_byte(raw));

    candidates
        .into_iter()
        .flatten()
        .filter(|text| !text.is_empty())
        .max_by_key(|text| text_score(text))
}

/// Reads a localStorage key from a Chromium profile's LevelDB storage.
/// Chromium key framing differs across versions, so entries are matched by
/// substring: the decoded key must contain both the origin host and the
/// storage key name.
pub fn read_local_storage(
    profile_dir: &Path,
    origin_host: &str,
    key_name: &str,
) -> Result<Token> {
    let ldb_dir = profile_dir.join("Local Storage").join("leveldb");
    if !ldb_dir.is_dir() {
        bail!("leveldb dir not found: {}", ldb_dir.display());
    }
    let snapshot = snapshot_leveldb(&ldb_dir)?;
    let mut options = Options::default();
    options.create_if_missing = false;
    let mut db =
        DB::open(&snapshot, options).with_context(|| "failed to open leveldb snapshot")?;

    let mut iter = db.new_iter().context("leveldb iterator failed")?;
    iter.seek_to_first();
    while iter.valid() {
        if let Some((key, value)) = iter.current() {
            let decoded_key = String::from_utf8_lossy(&key);
            if decoded_key.contains(origin_host) && decoded_key.contains(key_name) {
                if let Some(text) = decode_ls_value(&value) {
                    let _ = fs::remove_dir_all(&snapshot);
                    return Ok(Token {
                        key: key_name.to_owned(),
                        value: crate::auth::unwrap_local_storage_value(text),
                    });
                }
            }
        }
        iter.advance();
    }
    let _ = fs::remove_dir_all(&snapshot);
    bail!("localStorage key '{key_name}' for '{origin_host}' not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_matching() {
        assert!(host_matches(".deepseek.com", "deepseek.com"));
        assert!(host_matches("chat.deepseek.com", "deepseek.com"));
        assert!(host_matches("chat.deepseek.com", "chat.deepseek.com"));
        assert!(!host_matches("notdeepseek.com", "deepseek.com"));
        assert!(!host_matches("chat.deepseek.com", ""));
    }

    #[test]
    fn ls_value_decoding_strategies() {
        // type prefix + UTF-16LE
        let mut prefixed = vec![0x01u8];
        for unit in "hello".encode_utf16() {
            prefixed.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_ls_value(&prefixed).as_deref(), Some("hello"));
        // bare UTF-16LE
        let mut bare = Vec::new();
        for unit in "B2token".encode_utf16() {
            bare.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_ls_value(&bare).as_deref(), Some("B2token"));
        // single-byte text beats CJK mojibake from a UTF-16 misread
        assert_eq!(decode_ls_value(b"\x01abc").as_deref(), Some("abc"));
        assert_eq!(decode_ls_value(b"raw").as_deref(), Some("raw"));
        // UTF-16LE cyrillic beats its latin-1 mojibake (control chars lose)
        let mut cyrillic = vec![0x01u8];
        for unit in "Привет".encode_utf16() {
            cyrillic.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_ls_value(&cyrillic).as_deref(), Some("Привет"));
        // garbage
        assert_eq!(decode_ls_value(&[]), None);
        assert_eq!(decode_ls_value(&[0x01]), None);
    }

    #[test]
    fn cookie_decrypt_round_trip_with_random_key() {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};

        let key = [0x42u8; 32];
        let nonce_bytes = [0x07u8; 12];
        let cipher = Aes256Gcm::new_from_slice(&key).expect("key");
        let ciphertext = cipher
            .encrypt(&Nonce::try_from(&nonce_bytes[..]).expect("nonce"), b"secret-value".as_ref())
            .expect("encrypt");
        let mut stored = b"v10".to_vec();
        stored.extend_from_slice(&nonce_bytes);
        stored.extend_from_slice(&ciphertext);
        assert_eq!(
            decrypt_cookie_value(&key, &stored).expect("decrypt"),
            "secret-value"
        );
        assert!(decrypt_cookie_value(&key, b"short").is_err());
        assert!(decrypt_cookie_value(&key, b"v99rest-of-bytes-padded....").is_err());
    }

    #[test]
    fn leveldb_write_then_read_back() {
        let dir = std::env::temp_dir().join(format!(
            "firecode_leveldb_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let mut options = Options::default();
        options.create_if_missing = true;
        let mut db = DB::open(&dir, options).expect("open test db");
        // Chrome-style framing: prefix, origin, separators, storage key
        let key = format!("_\x00https://chat.deepseek.com\x00\x01userToken");
        let mut value = vec![0x01u8];
        for unit in "{\"value\":\"B2abc\"}".encode_utf16() {
            value.extend_from_slice(&unit.to_le_bytes());
        }
        db.put(key.as_bytes(), &value).expect("put");
        drop(db);

        // move it aside so read_local_storage treats it as a profile copy
        let profile = std::env::temp_dir().join(format!(
            "firecode_leveldb_profile_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&profile);
        let ldb = profile.join("Local Storage").join("leveldb");
        fs::create_dir_all(&ldb).expect("mkdir");
        for entry in fs::read_dir(&dir).expect("read dir") {
            let entry = entry.expect("entry");
            fs::copy(entry.path(), ldb.join(entry.file_name())).expect("copy");
        }
        let token =
            read_local_storage(&profile, "chat.deepseek.com", "userToken").expect("read back");
        assert_eq!(token.value, "B2abc");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&profile);
    }

    /// DPAPI round trip on this Windows machine.
    #[test]
    #[cfg(windows)]
    fn dpapi_round_trip() {
        use windows_sys::Win32::Foundation::HLOCAL;
        use windows_sys::Win32::Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CryptProtectData,
        };

        let plain = b"firecode-dpapi-test";
        let input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
                &mut output,
            )
        };
        assert_ne!(ok, 0, "CryptProtectData failed");
        let encrypted =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(output.pbData as HLOCAL);
        }
        assert_eq!(dpapi_unprotect(&encrypted).expect("unprotect"), plain);
    }

    /// Extracts the real AES master key from this machine's Chrome Local State.
    /// Run explicitly: cargo test --release -- --ignored
    #[test]
    #[ignore = "requires Chrome with an os_crypt key in Local State on this machine"]
    fn real_chrome_master_key_is_readable() {
        let profile = BrowserPresetPath::chrome_default_for_test();
        let local_state = profile
            .parent()
            .expect("parent")
            .join("Local State");
        if !local_state.is_file() {
            println!("skipped: no Local State file");
            return;
        }
        let raw = std::fs::read_to_string(&local_state).expect("read");
        if !raw.contains("encrypted_key") {
            println!("skipped: Local State has no os_crypt.encrypted_key on this machine");
            return;
        }
        let key = read_master_key(&profile).expect("master key");
        assert_eq!(key.len(), 32);
        assert!(key.iter().any(|&b| b != 0));
    }

    /// Opens a copy of the real Chrome LevelDB on this machine.
    /// Run explicitly: cargo test --release -- --ignored
    #[test]
    #[ignore = "requires Chrome with a leveldb dir on this machine"]
    fn real_chrome_leveldb_opens() {
        let profile = BrowserPresetPath::chrome_default_for_test();
        let snapshot = snapshot_leveldb(&profile.join("Local Storage").join("leveldb"))
            .expect("snapshot");
        let mut options = Options::default();
        options.create_if_missing = false;
        let mut db = DB::open(&snapshot, options).expect("open");
        let mut iter = db.new_iter().expect("iter");
        iter.seek_to_first();
        let mut count = 0u32;
        while iter.valid() {
            iter.next();
            count += 1;
            if count > 100_000 {
                break;
            }
        }
        println!("leveldb entries: {count}");
        let _ = fs::remove_dir_all(&snapshot);
    }

    /// Helper resolving the real Chrome profile dir on this machine.
    struct BrowserPresetPath;
    impl BrowserPresetPath {
        fn chrome_default_for_test() -> PathBuf {
            crate::settings::BrowserPreset::Chrome
                .default_profile_dir()
                .expect("chrome dir")
        }
    }
}
