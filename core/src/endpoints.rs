/// All remote URLs in one place so tests can point the engine at a local
/// mock server.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub auth_url: String,
    pub token_url: String,
    /// e.g. `https://www.googleapis.com/drive/v3`
    pub drive_base: String,
    /// e.g. `https://www.googleapis.com/upload/drive/v3`
    pub upload_base: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            drive_base: "https://www.googleapis.com/drive/v3".into(),
            upload_base: "https://www.googleapis.com/upload/drive/v3".into(),
        }
    }
}

impl Endpoints {
    /// Everything on one local mock server (used by tests).
    pub fn local(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            auth_url: format!("{base}/oauth/auth"),
            token_url: format!("{base}/oauth/token"),
            drive_base: format!("{base}/drive/v3"),
            upload_base: format!("{base}/upload/drive/v3"),
        }
    }
}
