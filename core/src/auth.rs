use crate::error::{AuthError, CloudError};
use keyring::Entry;
use parking_lot::RwLock;
use reqwest::Client;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use sha2::{Sha256, Digest};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

pub struct AccessTokenEntry {
    pub token: String,
    pub expires_at: Instant,
}

pub struct OAuthManager {
    client: Client,
    accounts: RwLock<HashMap<String, AccessTokenEntry>>,
}

impl OAuthManager {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            accounts: RwLock::new(HashMap::new()),
        }
    }

    pub fn credentials_file_path() -> PathBuf {
        let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("clouddrive");
        let _ = std::fs::create_dir_all(&path);
        path.push("credentials.json");
        path
    }

    pub fn tokens_file_path() -> PathBuf {
        let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("clouddrive");
        let _ = std::fs::create_dir_all(&path);
        path.push("tokens.json");
        path
    }

    pub fn get_client_credentials() -> Result<(String, String), CloudError> {
        // 1. Try keyring first
        let keyring_creds = (|| -> Option<(String, String)> {
            let id_entry = Entry::new("clouddrive-client-id", "global").ok()?;
            let secret_entry = Entry::new("clouddrive-client-secret", "global").ok()?;
            let id = id_entry.get_password().ok()?;
            let secret = secret_entry.get_password().ok()?;
            if !id.trim().is_empty() && !secret.trim().is_empty() {
                Some((id, secret))
            } else {
                None
            }
        })();

        if let Some((id, secret)) = keyring_creds {
            return Ok((id, secret));
        }

        // 2. Fallback to credentials.json in config dir
        let path = Self::credentials_file_path();
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    let id = val["client_id"].as_str().unwrap_or_default().trim();
                    let secret = val["client_secret"].as_str().unwrap_or_default().trim();
                    if !id.is_empty() && !secret.is_empty() {
                        return Ok((id.to_string(), secret.to_string()));
                    }
                }
            }
        }

        Err(CloudError::Auth(AuthError::NoClientCredentials))
    }

    pub fn store_client_credentials(id: &str, secret: &str) -> Result<(), CloudError> {
        // Try saving to system keyring
        if let Ok(id_entry) = Entry::new("clouddrive-client-id", "global") {
            let _ = id_entry.set_password(id);
        }
        if let Ok(secret_entry) = Entry::new("clouddrive-client-secret", "global") {
            let _ = secret_entry.set_password(secret);
        }

        // Always save to config directory as reliable backup
        let path = Self::credentials_file_path();
        let val = serde_json::json!({
            "client_id": id,
            "client_secret": secret
        });
        if let Ok(content) = serde_json::to_string_pretty(&val) {
            let _ = std::fs::write(&path, content);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }

        Ok(())
    }

    pub fn has_client_credentials() -> bool {
        Self::get_client_credentials().is_ok()
    }

    pub async fn start_auth_flow(&self, email_hint: Option<&str>) -> Result<String, CloudError> {
        let (client_id, client_secret) = Self::get_client_credentials().map_err(|_| CloudError::Auth(AuthError::NoClientCredentials))?;

        // Generate PKCE verifier and challenge synchronously (OsRng is Send-safe)
        let (verifier, challenge) = {
            use rand::RngCore;
            let mut bytes = [0u8; 48];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            let verifier = URL_SAFE_NO_PAD.encode(bytes);

            let mut hasher = Sha256::new();
            hasher.update(verifier.as_bytes());
            let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());
            (verifier, challenge)
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| CloudError::Io(e.to_string()))?;
        let port = listener.local_addr().map_err(|e| CloudError::Io(e.to_string()))?.port();
        let redirect_uri = format!("http://127.0.0.1:{}", port);

        let mut auth_url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
        auth_url.query_pairs_mut()
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", "https://www.googleapis.com/auth/drive https://www.googleapis.com/auth/userinfo.email")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
        
        if let Some(hint) = email_hint {
            auth_url.query_pairs_mut().append_pair("login_hint", hint);
        }

        open::that(auth_url.as_str()).map_err(|e| CloudError::Auth(AuthError::BrowserOpenFailed(e.to_string())))?;

        let code = tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                let (mut socket, _) = listener.accept().await.map_err(|e| CloudError::Io(e.to_string()))?;
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.map_err(|e| CloudError::Io(e.to_string()))?;
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let first_line = req_str.lines().next().unwrap_or_default();
                let path = first_line.split_whitespace().nth(1).unwrap_or("/");

                if path.starts_with("/favicon") {
                    let res = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(res.as_bytes()).await;
                    let _ = socket.flush().await;
                    continue;
                }

                let parsed_url = Url::parse(&format!("http://localhost{}", path)).ok();
                let code_opt = parsed_url.as_ref().and_then(|u| {
                    u.query_pairs().find(|(k, _)| k == "code").map(|(_, v)| v.into_owned())
                });

                if let Some(code) = code_opt {
                    let html = r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>CloudDrive Auth Success</title>
<style>body{background:#131314;color:#E3E3E3;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;}
.card{background:#1E1F20;border:1px solid #3B3D3F;border-radius:24px;padding:36px 48px;text-align:center;box-shadow:0 8px 32px rgba(0,0,0,0.5);}
h2{color:#A8C7FA;margin:0 0 10px 0;font-size:1.5rem;}
p{color:#C4C7C5;margin:0;font-size:0.95rem;}</style></head>
<body><div class="card"><h2>Authentication Successful!</h2><p>You can close this browser tab and return to CloudDrive.</p></div></body></html>"#;
                    let res = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", html.len(), html);
                    let _ = socket.write_all(res.as_bytes()).await;
                    let _ = socket.flush().await;
                    return Ok(code);
                } else if let Some(err_msg) = parsed_url.as_ref().and_then(|u| {
                    u.query_pairs().find(|(k, _)| k == "error").map(|(_, v)| v.into_owned())
                }) {
                    let html = format!(r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>CloudDrive Auth Error</title>
<style>body{{background:#131314;color:#E3E3E3;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;}}
.card{{background:#1E1F20;border:1px solid #601410;border-radius:24px;padding:36px 48px;text-align:center;box-shadow:0 8px 32px rgba(0,0,0,0.5);}}
h2{{color:#F2B8B5;margin:0 0 10px 0;font-size:1.5rem;}}
p{{color:#C4C7C5;margin:0;font-size:0.95rem;}}</style></head>
<body><div class="card"><h2>Authentication Failed</h2><p>Google returned error: {}</p></div></body></html>"#, err_msg);
                    let res = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", html.len(), html);
                    let _ = socket.write_all(res.as_bytes()).await;
                    let _ = socket.flush().await;
                    return Err(CloudError::Auth(AuthError::OAuth(err_msg)));
                } else {
                    let res = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(res.as_bytes()).await;
                    let _ = socket.flush().await;
                }
            }
        })
        .await
        .map_err(|_| CloudError::Auth(AuthError::Timeout("Timed out waiting for Google sign-in (180s)".into())))??;

        let params = [
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri.as_str()),
        ];

        let res = self.client.post("https://oauth2.googleapis.com/token")
            .form(&params)
            .send()
            .await
            .map_err(|e| CloudError::Network(e.to_string()))?;

        if !res.status().is_success() {
            return Err(CloudError::Auth(AuthError::InvalidGrant));
        }

        let json: serde_json::Value = res.json().await.map_err(|e| CloudError::Network(e.to_string()))?;
        
        let access_token = json["access_token"].as_str().ok_or(CloudError::Auth(AuthError::InvalidGrant))?.to_string();
        let refresh_token = json["refresh_token"].as_str().ok_or(CloudError::Auth(AuthError::InvalidGrant))?.to_string();
        let expires_in = json["expires_in"].as_u64().unwrap_or(3600);

        let user_info_res = self.client.get("https://www.googleapis.com/oauth2/v2/userinfo")
            .bearer_auth(&access_token)
            .send()
            .await
            .map_err(|e| CloudError::Network(e.to_string()))?;
        
        let user_info: serde_json::Value = user_info_res.json().await.map_err(|e| CloudError::Network(e.to_string()))?;
        let email = user_info["email"].as_str().ok_or(CloudError::Auth(AuthError::InvalidGrant))?.to_string();

        // 1. Try Keyring
        if let Ok(entry) = Entry::new("clouddrive-refresh-token", &email) {
            let _ = entry.set_password(&refresh_token);
        }

        // 2. Also persist to tokens.json file
        let tokens_path = Self::tokens_file_path();
        let mut tokens: HashMap<String, String> = std::fs::read_to_string(&tokens_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        tokens.insert(email.clone(), refresh_token.clone());
        if let Ok(json_str) = serde_json::to_string_pretty(&tokens) {
            let _ = std::fs::write(&tokens_path, json_str);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tokens_path, std::fs::Permissions::from_mode(0o600));
            }
        }

        self.accounts.write().insert(email.clone(), AccessTokenEntry {
            token: access_token,
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        });

        Ok(email)
    }

    pub async fn get_access_token(&self, email: &str) -> Result<String, CloudError> {
        let needs_refresh = {
            let accounts = self.accounts.read();
            if let Some(entry) = accounts.get(email) {
                if entry.expires_at > Instant::now() + Duration::from_secs(300) {
                    return Ok(entry.token.clone());
                }
                true
            } else {
                true
            }
        };

        if needs_refresh {
            return self.refresh_access_token(email).await;
        }
        
        Err(CloudError::Auth(AuthError::InvalidGrant))
    }

    pub async fn refresh_access_token(&self, email: &str) -> Result<String, CloudError> {
        let (client_id, client_secret) = Self::get_client_credentials().map_err(|_| CloudError::Auth(AuthError::NoClientCredentials))?;
        
        let token_from_keyring = Entry::new("clouddrive-refresh-token", email)
            .ok()
            .and_then(|entry| entry.get_password().ok());
        
        let refresh_token = if let Some(t) = token_from_keyring {
            t
        } else {
            let tokens_path = Self::tokens_file_path();
            let tokens: HashMap<String, String> = std::fs::read_to_string(&tokens_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            tokens.get(email).cloned().ok_or(CloudError::Auth(AuthError::InvalidGrant))?
        };

        let params = [
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];

        let res = self.client.post("https://oauth2.googleapis.com/token")
            .form(&params)
            .send()
            .await
            .map_err(|e| CloudError::Network(e.to_string()))?;

        if res.status() == reqwest::StatusCode::BAD_REQUEST {
            return Err(CloudError::Auth(AuthError::RefreshTokenExpired));
        }
        if !res.status().is_success() {
            return Err(CloudError::Auth(AuthError::InvalidGrant));
        }

        let json: serde_json::Value = res.json().await.map_err(|e| CloudError::Network(e.to_string()))?;
        let access_token = json["access_token"].as_str().ok_or(CloudError::Auth(AuthError::InvalidGrant))?.to_string();
        let expires_in = json["expires_in"].as_u64().unwrap_or(3600);

        self.accounts.write().insert(email.to_string(), AccessTokenEntry {
            token: access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        });

        Ok(access_token)
    }

    pub fn remove_account(&self, email: &str) -> Result<(), CloudError> {
        self.accounts.write().remove(email);
        if let Ok(entry) = Entry::new("clouddrive-refresh-token", email) {
            let _ = entry.delete_credential();
        }
        let tokens_path = Self::tokens_file_path();
        let mut tokens: HashMap<String, String> = std::fs::read_to_string(&tokens_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        tokens.remove(email);
        if let Ok(json_str) = serde_json::to_string_pretty(&tokens) {
            let _ = std::fs::write(&tokens_path, json_str);
        }
        Ok(())
    }
}
