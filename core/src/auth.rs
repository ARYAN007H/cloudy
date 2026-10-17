//! OAuth 2.0 (installed-app / loopback flow with PKCE) for Google accounts.

use crate::endpoints::Endpoints;
use crate::error::{AuthError, CloudError};
use crate::store::{normalize_email, parse_client_credentials, SecretStore};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use url::Url;

const SCOPE: &str = "https://www.googleapis.com/auth/drive";
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh this long before the real expiry.
const EXPIRY_MARGIN: Duration = Duration::from_secs(120);

struct AccessTokenEntry {
    token: String,
    expires_at: Instant,
}

/// A sign-in that has been prepared (listener bound, URL built) but not
/// completed. Show `url` to the user / open it in the browser, then pass the
/// value to [`OAuthManager::complete_auth`].
pub struct PendingAuth {
    pub url: String,
    listener: TcpListener,
    verifier: String,
    state: String,
    redirect_uri: String,
    client_id: String,
    client_secret: String,
    expected_email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthOutcome {
    pub email: String,
    pub display_name: Option<String>,
    /// The account was already linked; only its credentials were refreshed.
    pub already_linked: bool,
}

pub struct OAuthManager {
    client: reqwest::Client,
    endpoints: Endpoints,
    store: Arc<SecretStore>,
    access: RwLock<HashMap<String, AccessTokenEntry>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    reauth: RwLock<HashSet<String>>,
    creds: RwLock<Option<(String, String)>>,
}

impl OAuthManager {
    pub fn new(endpoints: Endpoints, store: Arc<SecretStore>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("Cloudy/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self {
            client,
            endpoints,
            store,
            access: RwLock::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            reauth: RwLock::new(HashSet::new()),
            creds: RwLock::new(None),
        }
    }

    pub fn store(&self) -> &Arc<SecretStore> {
        &self.store
    }

    // ------------------------------------------------------------ credentials

    pub fn client_credentials(&self) -> Result<(String, String), CloudError> {
        if let Some(c) = self.creds.read().clone() {
            return Ok(c);
        }
        let c = self.store.get_client_credentials().ok_or(CloudError::Auth(AuthError::NoClientCredentials))?;
        *self.creds.write() = Some(c.clone());
        Ok(c)
    }

    pub fn has_client_credentials(&self) -> bool {
        self.client_credentials().is_ok()
    }

    pub fn set_client_credentials(&self, id: &str, secret: &str) -> Result<(), CloudError> {
        let (id, secret) = parse_client_credentials(id, secret)?;
        self.store.set_client_credentials(&id, &secret)?;
        *self.creds.write() = Some((id, secret));
        Ok(())
    }

    // ------------------------------------------------------------- sign-in

    /// Bind the loopback listener and build the Google consent URL.
    /// `strict`: the user must sign in as exactly `email_hint` (re-connecting an
    /// existing account); otherwise the hint only pre-selects an account.
    pub async fn begin_auth(&self, email_hint: Option<&str>, strict: bool) -> Result<PendingAuth, CloudError> {
        let (client_id, client_secret) = self.client_credentials()?;

        let (verifier, challenge, state) = {
            use rand::RngCore;
            let mut b = [0u8; 48];
            rand::rngs::OsRng.fill_bytes(&mut b);
            let verifier = URL_SAFE_NO_PAD.encode(b);
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            let mut s = [0u8; 24];
            rand::rngs::OsRng.fill_bytes(&mut s);
            (verifier, challenge, URL_SAFE_NO_PAD.encode(s))
        };

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}");

        let mut url = Url::parse(&self.endpoints.auth_url)
            .map_err(|e| CloudError::Config(format!("bad auth url: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("response_type", "code")
                .append_pair("scope", SCOPE)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("access_type", "offline")
                // `select_account` is essential: without it Google silently
                // re-uses the browser's signed-in account, making it
                // impossible to add a *second* account.
                .append_pair("prompt", "select_account consent")
                .append_pair("state", &state);
            if let Some(h) = email_hint.map(str::trim).filter(|h| !h.is_empty()) {
                q.append_pair("login_hint", h);
            }
        }

        Ok(PendingAuth {
            url: url.to_string(),
            listener,
            verifier,
            state,
            redirect_uri,
            client_id,
            client_secret,
            expected_email: if strict { email_hint.map(normalize_email).filter(|h| !h.is_empty()) } else { None },
        })
    }

    /// Wait for the browser redirect, exchange the code, identify the account
    /// and persist its refresh token.
    pub async fn complete_auth(
        &self,
        pending: PendingAuth,
        cancel: CancellationToken,
    ) -> Result<AuthOutcome, CloudError> {
        let PendingAuth { listener, verifier, state, redirect_uri, client_id, client_secret, expected_email, .. } =
            pending;

        let code = wait_for_code(listener, state, cancel, AUTH_TIMEOUT).await?;

        let params = [
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri.as_str()),
        ];
        let res = self
            .client
            .post(&self.endpoints.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| CloudError::Network(e.to_string()))?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(map_token_error(status.as_u16(), &body, false));
        }

        let access_token = body["access_token"]
            .as_str()
            .ok_or_else(|| AuthError::OAuth("Google returned no access token".into()))?
            .to_string();
        let expires_in = body["expires_in"].as_u64().unwrap_or(3600);

        let (email, display_name) = self.fetch_identity(&access_token).await?;

        if let Some(expected) = expected_email {
            if expected != email {
                return Err(AuthError::AccountMismatch { expected, actual: email }.into());
            }
        }

        let refresh_token = match body["refresh_token"].as_str() {
            Some(t) => t.to_string(),
            None => self.store.get_refresh_token(&email).ok_or_else(|| {
                AuthError::OAuth(
                    "Google did not return a refresh token. Remove Cloudy at \
                     https://myaccount.google.com/permissions and sign in again."
                        .into(),
                )
            })?,
        };

        let already_linked = self.store.list_accounts().contains(&email);
        self.store.set_refresh_token(&email, &refresh_token)?;
        self.reauth.write().remove(&email);
        self.access.write().insert(
            email.clone(),
            AccessTokenEntry { token: access_token, expires_at: Instant::now() + Duration::from_secs(expires_in) },
        );
        Ok(AuthOutcome { email, display_name, already_linked })
    }

    async fn fetch_identity(&self, access_token: &str) -> Result<(String, Option<String>), CloudError> {
        let url = format!("{}/about?fields=user(emailAddress,displayName)", self.endpoints.drive_base);
        let res = self
            .client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| CloudError::Network(e.to_string()))?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            let msg = body["error"]["message"].as_str().unwrap_or("could not read account identity");
            let hint = if body["error"]["status"] == "PERMISSION_DENIED" || msg.contains("has not been used") {
                " (Is the Google Drive API enabled in your Google Cloud project?)"
            } else {
                ""
            };
            return Err(AuthError::OAuth(format!("{msg}{hint}")).into());
        }
        let email = body["user"]["emailAddress"]
            .as_str()
            .map(normalize_email)
            .filter(|e| !e.is_empty())
            .ok_or_else(|| AuthError::OAuth("Google did not report the account's e-mail address".into()))?;
        let name = body["user"]["displayName"].as_str().map(|s| s.to_string());
        Ok((email, name))
    }

    // --------------------------------------------------------- access tokens

    fn cached(&self, email: &str) -> Option<String> {
        let g = self.access.read();
        g.get(email).filter(|e| e.expires_at > Instant::now() + EXPIRY_MARGIN).map(|e| e.token.clone())
    }

    fn lock_for(&self, email: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().entry(email.to_string()).or_default().clone()
    }

    /// Valid access token for `email`, refreshing it (once, even under heavy
    /// concurrency) when needed.
    pub async fn get_access_token(&self, email: &str) -> Result<String, CloudError> {
        let email = normalize_email(email);
        if let Some(t) = self.cached(&email) {
            return Ok(t);
        }
        if self.reauth.read().contains(&email) {
            return Err(AuthError::RefreshTokenExpired.into());
        }
        let lock = self.lock_for(&email);
        let _g = lock.lock().await;
        if let Some(t) = self.cached(&email) {
            return Ok(t); // somebody else refreshed while we waited
        }
        self.refresh_locked(&email).await
    }

    /// Drop the cached access token (e.g. after a 401) so the next call refreshes.
    pub fn invalidate(&self, email: &str) {
        self.access.write().remove(&normalize_email(email));
    }

    /// Force a refresh now.
    pub async fn refresh_access_token(&self, email: &str) -> Result<String, CloudError> {
        let email = normalize_email(email);
        let lock = self.lock_for(&email);
        let _g = lock.lock().await;
        self.refresh_locked(&email).await
    }

    async fn refresh_locked(&self, email: &str) -> Result<String, CloudError> {
        let (client_id, client_secret) = self.client_credentials()?;
        let Some(refresh_token) = self.store.get_refresh_token(email) else {
            self.reauth.write().insert(email.to_string());
            return Err(AuthError::RefreshTokenExpired.into());
        };
        let params = [
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];
        let res = self
            .client
            .post(&self.endpoints.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| CloudError::Network(e.to_string()))?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            let err = map_token_error(status.as_u16(), &body, true);
            if matches!(err, CloudError::Auth(AuthError::RefreshTokenExpired)) {
                self.reauth.write().insert(email.to_string());
            }
            return Err(err);
        }
        let access_token = body["access_token"]
            .as_str()
            .ok_or_else(|| AuthError::OAuth("Google returned no access token".into()))?
            .to_string();
        let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
        if let Some(new_rt) = body["refresh_token"].as_str() {
            let _ = self.store.set_refresh_token(email, new_rt);
        }
        self.access.write().insert(
            email.to_string(),
            AccessTokenEntry {
                token: access_token.clone(),
                expires_at: Instant::now() + Duration::from_secs(expires_in),
            },
        );
        Ok(access_token)
    }

    // ------------------------------------------------------------- accounts

    pub fn list_accounts(&self) -> Vec<String> {
        self.store.list_accounts()
    }

    pub fn needs_reauth(&self, email: &str) -> bool {
        self.reauth.read().contains(&normalize_email(email))
    }

    pub fn mark_needs_reauth(&self, email: &str) {
        self.reauth.write().insert(normalize_email(email));
    }

    pub fn remove_account(&self, email: &str) {
        let email = normalize_email(email);
        self.access.write().remove(&email);
        self.reauth.write().remove(&email);
        self.refresh_locks.lock().remove(&email);
        self.store.delete_account(&email);
    }

    /// Test helper: pretend `email` already has a fresh access token.
    #[doc(hidden)]
    pub fn seed_access_token(&self, email: &str, token: &str) {
        self.access.write().insert(
            normalize_email(email),
            AccessTokenEntry { token: token.into(), expires_at: Instant::now() + Duration::from_secs(3600) },
        );
    }
}

fn map_token_error(status: u16, body: &serde_json::Value, refreshing: bool) -> CloudError {
    let code = body["error"].as_str().unwrap_or_default();
    let desc = body["error_description"].as_str().unwrap_or_default();
    if status >= 500 || status == 429 {
        return CloudError::Network(format!("Google's sign-in service is unavailable (HTTP {status})"));
    }
    match code {
        "invalid_grant" if refreshing => AuthError::RefreshTokenExpired.into(),
        "invalid_grant" => AuthError::OAuth(
            "Google rejected the sign-in code (invalid_grant). Please try adding the account again.".into(),
        )
        .into(),
        "invalid_client" | "unauthorized_client" => AuthError::OAuth(
            "Google rejected your Client ID / Secret. Check them in Settings and make sure the OAuth client \
             type is \"Desktop app\"."
                .into(),
        )
        .into(),
        "redirect_uri_mismatch" => AuthError::OAuth(
            "redirect_uri_mismatch: create the OAuth client as type \"Desktop app\" (not \"Web application\").".into(),
        )
        .into(),
        "" => AuthError::OAuth(format!("Unexpected response from Google (HTTP {status})")).into(),
        other => AuthError::OAuth(if desc.is_empty() { other.to_string() } else { format!("{other}: {desc}") }).into(),
    }
}

// ----------------------------------------------------------------------------
// Loopback redirect server
// ----------------------------------------------------------------------------

fn friendly_oauth_error(code: &str) -> String {
    match code {
        "access_denied" => "Access was denied. If your Google Cloud app is in \"Testing\" mode, add this Google \
            account under OAuth consent screen → Test users, then try again."
            .to_string(),
        other => format!("Google returned the error \"{other}\"."),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(ok: bool, title: &str, msg: &str) -> String {
    let accent = if ok { "#8ab4f8" } else { "#f28b82" };
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Cloudy</title><style>\
         body{{margin:0;height:100vh;display:flex;align-items:center;justify-content:center;background:#131314;color:#e3e3e3;\
         font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif}}\
         .card{{max-width:420px;background:#1e1f20;border:1px solid #3b3d3f;border-radius:24px;padding:36px 44px;text-align:center}}\
         h1{{margin:0 0 10px;font-size:1.4rem;color:{accent}}}p{{margin:0;color:#c4c7c5;line-height:1.55;font-size:.95rem}}\
         </style></head><body><div class=\"card\"><h1>{}</h1><p>{}</p></div></body></html>",
        html_escape(title),
        html_escape(msg)
    )
}

async fn respond(sock: &mut TcpStream, status: &str, html: &str) {
    let res = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = sock.write_all(res.as_bytes()).await;
    let _ = sock.shutdown().await;
}

/// Serve the loopback redirect until a valid callback arrives.
///
/// Every connection is handled in its own task with a read timeout: browsers
/// open speculative idle connections, and a sequential `read` on one of those
/// would otherwise block the real callback.
pub(crate) async fn wait_for_code(
    listener: TcpListener,
    state: String,
    cancel: CancellationToken,
    timeout: Duration,
) -> Result<String, CloudError> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String, CloudError>>(4);
    let accept = tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { break };
            let (tx, state) = (tx.clone(), state.clone());
            tokio::spawn(async move { handle_callback(sock, &state, tx).await });
        }
    });

    let result = tokio::select! {
        _ = cancel.cancelled() => Err(CloudError::Auth(AuthError::Cancelled)),
        _ = tokio::time::sleep(timeout) => Err(CloudError::Auth(AuthError::Timeout(
            format!("No response from the browser after {} seconds", timeout.as_secs())))),
        r = rx.recv() => r.unwrap_or(Err(CloudError::Auth(AuthError::Cancelled))),
    };
    accept.abort();
    result
}

async fn handle_callback(
    mut sock: TcpStream,
    expected_state: &str,
    tx: tokio::sync::mpsc::Sender<Result<String, CloudError>>,
) {
    let mut buf = Vec::with_capacity(2048);
    let read = tokio::time::timeout(Duration::from_secs(5), async {
        let mut chunk = [0u8; 2048];
        loop {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
                        break;
                    }
                }
            }
        }
    })
    .await;
    if read.is_err() || buf.is_empty() {
        return; // idle pre-connect – just drop it
    }

    let req = String::from_utf8_lossy(&buf);
    let target = req.lines().next().unwrap_or_default().split_whitespace().nth(1).unwrap_or("/").to_string();
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return respond(&mut sock, "400 Bad Request", &page(false, "Bad request", "Malformed request.")).await;
    };
    if url.path() != "/" {
        return respond(&mut sock, "404 Not Found", "").await;
    }
    let q: HashMap<_, _> = url.query_pairs().into_owned().collect();

    if q.get("state").map(String::as_str) != Some(expected_state) {
        // Stale tab, other program or forged request – never accept it.
        return respond(
            &mut sock,
            "400 Bad Request",
            &page(false, "Sign-in mismatch", "This link does not belong to the current sign-in. Please start again from Cloudy."),
        )
        .await;
    }
    if let Some(err) = q.get("error") {
        let msg = friendly_oauth_error(err);
        respond(&mut sock, "400 Bad Request", &page(false, "Sign-in failed", &msg)).await;
        let _ = tx.send(Err(AuthError::OAuth(msg).into())).await;
    } else if let Some(code) = q.get("code") {
        respond(
            &mut sock,
            "200 OK",
            &page(true, "You're connected", "Account linked. You can close this tab and return to Cloudy."),
        )
        .await;
        let _ = tx.send(Ok(code.clone())).await;
    } else {
        respond(&mut sock, "400 Bad Request", &page(false, "Missing code", "Google did not send a sign-in code.")).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn http_get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out).await;
        out
    }

    async fn listener() -> (TcpListener, u16) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        (l, p)
    }

    #[tokio::test]
    async fn idle_preconnect_does_not_block_real_callback() {
        let (l, port) = listener().await;
        let task = tokio::spawn(wait_for_code(l, "S".into(), CancellationToken::new(), Duration::from_secs(20)));
        let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap(); // browser pre-connect
        let res = http_get(port, "/?code=abc&state=S").await;
        assert!(res.starts_with("HTTP/1.1 200"), "{res}");
        assert_eq!(task.await.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn wrong_state_is_rejected_but_flow_continues() {
        let (l, port) = listener().await;
        let task = tokio::spawn(wait_for_code(l, "GOOD".into(), CancellationToken::new(), Duration::from_secs(20)));
        let bad = http_get(port, "/?code=evil&state=BAD").await;
        assert!(bad.starts_with("HTTP/1.1 400"));
        let nostate = http_get(port, "/?code=evil").await;
        assert!(nostate.starts_with("HTTP/1.1 400"));
        let fav = http_get(port, "/favicon.ico").await;
        assert!(fav.starts_with("HTTP/1.1 404") || fav.starts_with("HTTP/1.1 400"));
        http_get(port, "/?code=real&state=GOOD").await;
        assert_eq!(task.await.unwrap().unwrap(), "real");
    }

    #[tokio::test]
    async fn access_denied_gives_actionable_message() {
        let (l, port) = listener().await;
        let task = tokio::spawn(wait_for_code(l, "S".into(), CancellationToken::new(), Duration::from_secs(20)));
        http_get(port, "/?error=access_denied&state=S").await;
        let err = task.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("Test users"), "{err}");
    }

    #[tokio::test]
    async fn cancel_and_timeout() {
        let (l, _) = listener().await;
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let task = tokio::spawn(wait_for_code(l, "S".into(), cancel, Duration::from_secs(20)));
        c2.cancel();
        assert!(matches!(task.await.unwrap(), Err(CloudError::Auth(AuthError::Cancelled))));

        let (l, _) = listener().await;
        let r = wait_for_code(l, "S".into(), CancellationToken::new(), Duration::from_millis(50)).await;
        assert!(matches!(r, Err(CloudError::Auth(AuthError::Timeout(_)))));
    }

    #[tokio::test]
    async fn auth_url_forces_account_picker_and_is_bound_to_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SecretStore::file_backed(dir.path().to_path_buf()));
        let m = OAuthManager::new(Endpoints::default(), store);
        m.set_client_credentials("1-x.apps.googleusercontent.com", "secret").unwrap();
        let p = m.begin_auth(Some("Me@Gmail.com"), false).await.unwrap();
        let u = Url::parse(&p.url).unwrap();
        let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["prompt"], "select_account consent");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], p.state);
        assert_eq!(q["login_hint"], "Me@Gmail.com");
        assert!(q["redirect_uri"].starts_with("http://127.0.0.1:"));
    }
}
