//! Storage for secrets (refresh tokens, OAuth client credentials) and the
//! non-secret index of linked accounts.
//!
//! * If a working OS keyring (Secret Service / KWallet / Keychain) is found, it
//!   is used and legacy plaintext files are migrated into it.
//! * Otherwise secrets live in `0600` files in the config directory. Writes are
//!   atomic and a failure is surfaced instead of being ignored.
//! * `accounts.json` (emails only) is always kept, so the account list does not
//!   depend on the keyring being unlocked at start-up.

use crate::config::{paths, write_atomic};
use crate::error::{AuthError, CloudError};
use keyring::Entry;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

const SVC_TOKEN: &str = "clouddrive-refresh-token";
const SVC_CLIENT_ID: &str = "clouddrive-client-id";
const SVC_CLIENT_SECRET: &str = "clouddrive-client-secret";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Backend {
    Keyring,
    File,
}

pub struct SecretStore {
    backend: Backend,
    dir: PathBuf,
    lock: Mutex<()>,
}

pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Run the keyring round-trip on a helper thread: on some desktops an
/// unresponsive Secret Service can block for a long time.
fn keyring_works() -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ok = (|| -> Result<bool, keyring::Error> {
            let e = Entry::new("clouddrive-probe", "probe")?;
            e.set_password("ok")?;
            let v = e.get_password()?;
            let _ = e.delete_credential();
            Ok(v == "ok")
        })()
        .unwrap_or(false);
        let _ = tx.send(ok);
    });
    rx.recv_timeout(Duration::from_secs(6)).unwrap_or(false)
}

impl SecretStore {
    /// Open the store, auto-detecting the best backend.
    pub fn open() -> Self {
        let forced_file = std::env::var("CLOUDDRIVE_NO_KEYRING").is_ok();
        let backend = if !forced_file && keyring_works() { Backend::Keyring } else { Backend::File };
        let s = Self { backend, dir: paths::config_dir(), lock: Mutex::new(()) };
        if backend == Backend::Keyring {
            s.migrate_files_into_keyring();
        }
        tracing::info!("Secret storage backend: {:?}", s.backend);
        s
    }

    /// File backend in an explicit directory (tests).
    pub fn file_backed(dir: PathBuf) -> Self {
        Self { backend: Backend::File, dir, lock: Mutex::new(()) }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    fn tokens_path(&self) -> PathBuf {
        self.dir.join("tokens.json")
    }
    fn creds_path(&self) -> PathBuf {
        self.dir.join("credentials.json")
    }
    fn index_path(&self) -> PathBuf {
        self.dir.join("accounts.json")
    }

    fn read_map(path: &PathBuf) -> HashMap<String, String> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_map(path: &PathBuf, map: &HashMap<String, String>) -> Result<(), CloudError> {
        let s = serde_json::to_string_pretty(map).map_err(|e| CloudError::Io(e.to_string()))?;
        write_atomic(path, s.as_bytes(), true).map_err(|e| CloudError::Io(e.to_string()))
    }

    // ---------------------------------------------------------------- accounts

    pub fn list_accounts(&self) -> Vec<String> {
        let _g = self.lock.lock();
        let mut set: BTreeSet<String> = BTreeSet::new();
        if let Ok(s) = std::fs::read_to_string(self.index_path()) {
            if let Ok(v) = serde_json::from_str::<Vec<String>>(&s) {
                set.extend(v.into_iter().map(|e| normalize_email(&e)));
            }
        }
        // Accounts from an older version only existed in tokens.json.
        set.extend(Self::read_map(&self.tokens_path()).keys().map(|e| normalize_email(e)));
        set.into_iter().collect()
    }

    fn save_index(&self, set: &BTreeSet<String>) -> Result<(), CloudError> {
        let v: Vec<&String> = set.iter().collect();
        let s = serde_json::to_string_pretty(&v).map_err(|e| CloudError::Io(e.to_string()))?;
        write_atomic(&self.index_path(), s.as_bytes(), false).map_err(|e| CloudError::Io(e.to_string()))
    }

    // ----------------------------------------------------------- refresh tokens

    pub fn get_refresh_token(&self, email: &str) -> Option<String> {
        let email = normalize_email(email);
        if self.backend == Backend::Keyring {
            if let Some(t) = Entry::new(SVC_TOKEN, &email).ok().and_then(|e| e.get_password().ok()) {
                return Some(t);
            }
        }
        let _g = self.lock.lock();
        let map = Self::read_map(&self.tokens_path());
        map.get(&email)
            .or_else(|| map.iter().find(|(k, _)| normalize_email(k) == email).map(|(_, v)| v))
            .cloned()
    }

    pub fn set_refresh_token(&self, email: &str, token: &str) -> Result<(), CloudError> {
        let email = normalize_email(email);
        let mut stored_in_keyring = false;
        if self.backend == Backend::Keyring {
            if let Ok(e) = Entry::new(SVC_TOKEN, &email) {
                if e.set_password(token).is_ok() && e.get_password().ok().as_deref() == Some(token) {
                    stored_in_keyring = true;
                }
            }
        }
        let _g = self.lock.lock();
        let mut map = Self::read_map(&self.tokens_path());
        if stored_in_keyring {
            // Do not leave a stale plaintext copy around.
            if map.remove(&email).is_some() {
                if map.is_empty() {
                    let _ = std::fs::remove_file(self.tokens_path());
                } else {
                    Self::write_map(&self.tokens_path(), &map)?;
                }
            }
        } else {
            map.insert(email.clone(), token.to_string());
            Self::write_map(&self.tokens_path(), &map)?;
        }
        let mut idx: BTreeSet<String> = std::fs::read_to_string(self.index_path())
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|e| normalize_email(&e))
            .collect();
        idx.insert(email);
        self.save_index(&idx)
    }

    pub fn delete_account(&self, email: &str) {
        let email = normalize_email(email);
        if let Ok(e) = Entry::new(SVC_TOKEN, &email) {
            let _ = e.delete_credential();
        }
        let _g = self.lock.lock();
        let mut map = Self::read_map(&self.tokens_path());
        let before = map.len();
        map.retain(|k, _| normalize_email(k) != email);
        if map.len() != before {
            let _ = if map.is_empty() {
                std::fs::remove_file(self.tokens_path()).map_err(|e| CloudError::Io(e.to_string()))
            } else {
                Self::write_map(&self.tokens_path(), &map)
            };
        }
        let mut idx: BTreeSet<String> = std::fs::read_to_string(self.index_path())
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|e| normalize_email(&e))
            .collect();
        idx.remove(&email);
        let _ = self.save_index(&idx);
    }

    // ------------------------------------------------------ client credentials

    pub fn get_client_credentials(&self) -> Option<(String, String)> {
        if self.backend == Backend::Keyring {
            let id = Entry::new(SVC_CLIENT_ID, "global").ok().and_then(|e| e.get_password().ok());
            let secret = Entry::new(SVC_CLIENT_SECRET, "global").ok().and_then(|e| e.get_password().ok());
            if let (Some(id), Some(secret)) = (id, secret) {
                if !id.trim().is_empty() && !secret.trim().is_empty() {
                    return Some((id.trim().to_string(), secret.trim().to_string()));
                }
            }
        }
        let _g = self.lock.lock();
        let map = Self::read_map(&self.creds_path());
        let id = map.get("client_id")?.trim().to_string();
        let secret = map.get("client_secret")?.trim().to_string();
        (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
    }

    pub fn set_client_credentials(&self, id: &str, secret: &str) -> Result<(), CloudError> {
        let (id, secret) = (id.trim(), secret.trim());
        let mut in_keyring = false;
        if self.backend == Backend::Keyring {
            let a = Entry::new(SVC_CLIENT_ID, "global").map(|e| e.set_password(id).is_ok()).unwrap_or(false);
            let b = Entry::new(SVC_CLIENT_SECRET, "global").map(|e| e.set_password(secret).is_ok()).unwrap_or(false);
            in_keyring = a && b;
        }
        let _g = self.lock.lock();
        if in_keyring {
            let _ = std::fs::remove_file(self.creds_path());
            return Ok(());
        }
        let mut map = HashMap::new();
        map.insert("client_id".to_string(), id.to_string());
        map.insert("client_secret".to_string(), secret.to_string());
        Self::write_map(&self.creds_path(), &map)
    }

    // --------------------------------------------------------------- migration

    fn migrate_files_into_keyring(&self) {
        let _g = self.lock.lock();
        let tokens = Self::read_map(&self.tokens_path());
        if !tokens.is_empty() {
            let mut all_ok = true;
            for (email, tok) in &tokens {
                let ok = Entry::new(SVC_TOKEN, &normalize_email(email))
                    .map(|e| e.set_password(tok).is_ok() && e.get_password().ok().as_deref() == Some(tok.as_str()))
                    .unwrap_or(false);
                all_ok &= ok;
            }
            if all_ok {
                let _ = std::fs::remove_file(self.tokens_path());
                tracing::info!("Moved {} refresh token(s) from tokens.json into the system keyring", tokens.len());
            }
        }
        let creds = Self::read_map(&self.creds_path());
        if let (Some(id), Some(secret)) = (creds.get("client_id"), creds.get("client_secret")) {
            let a = Entry::new(SVC_CLIENT_ID, "global").map(|e| e.set_password(id).is_ok()).unwrap_or(false);
            let b = Entry::new(SVC_CLIENT_SECRET, "global").map(|e| e.set_password(secret).is_ok()).unwrap_or(false);
            if a && b {
                let _ = std::fs::remove_file(self.creds_path());
            }
        }
    }
}

/// Validate and normalise user-supplied OAuth client credentials.
/// Also accepts the JSON file downloaded from Google Cloud Console.
pub fn parse_client_credentials(id: &str, secret: &str) -> Result<(String, String), CloudError> {
    let (mut id, mut secret) = (id.trim().to_string(), secret.trim().to_string());
    // The user pasted the whole downloaded JSON into the ID box.
    if id.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&id) {
            let inner = v.get("installed").or_else(|| v.get("web")).unwrap_or(&v);
            id = inner["client_id"].as_str().unwrap_or_default().trim().to_string();
            secret = inner["client_secret"].as_str().unwrap_or_default().trim().to_string();
        }
    }
    if id.is_empty() || secret.is_empty() {
        return Err(CloudError::Config("Client ID and Client Secret are both required".into()));
    }
    if !id.ends_with(".apps.googleusercontent.com") {
        return Err(CloudError::Config(
            "That doesn't look like a Google Client ID (it should end in .apps.googleusercontent.com)".into(),
        ));
    }
    Ok((id, secret))
}

pub fn no_credentials() -> CloudError {
    CloudError::Auth(AuthError::NoClientCredentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_roundtrip_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let s = SecretStore::file_backed(dir.path().to_path_buf());
        assert!(s.list_accounts().is_empty());
        s.set_refresh_token("A@Gmail.com", "rt-1").unwrap();
        s.set_refresh_token("b@gmail.com", "rt-2").unwrap();
        assert_eq!(s.list_accounts(), vec!["a@gmail.com", "b@gmail.com"]);
        assert_eq!(s.get_refresh_token("a@gmail.com").as_deref(), Some("rt-1"));
        s.delete_account("a@gmail.com");
        assert_eq!(s.list_accounts(), vec!["b@gmail.com"]);
        assert!(s.get_refresh_token("a@gmail.com").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(dir.path().join("tokens.json")).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o600, "token file must be private");
        }
    }

    #[test]
    fn legacy_tokens_json_still_lists_accounts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tokens.json"), r#"{"Old@Gmail.com":"rt"}"#).unwrap();
        let s = SecretStore::file_backed(dir.path().to_path_buf());
        assert_eq!(s.list_accounts(), vec!["old@gmail.com"]);
        assert_eq!(s.get_refresh_token("old@gmail.com").as_deref(), Some("rt"));
    }

    #[test]
    fn credentials_parse_google_json_and_reject_junk() {
        let json = r#"{"installed":{"client_id":"123-abc.apps.googleusercontent.com","client_secret":" GOCSPX-x "}}"#;
        let (id, sec) = parse_client_credentials(json, "").unwrap();
        assert_eq!(id, "123-abc.apps.googleusercontent.com");
        assert_eq!(sec, "GOCSPX-x");
        assert!(parse_client_credentials("nope", "x").is_err());
        assert!(parse_client_credentials("", "").is_err());
    }
}
