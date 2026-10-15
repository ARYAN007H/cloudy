use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum AuthError {
    #[error("Invalid grant")]
    InvalidGrant,
    #[error("Refresh token expired")]
    RefreshTokenExpired,
    #[error("No client credentials found")]
    NoClientCredentials,
    #[error("Keyring error: {0}")]
    KeyringError(String),
    #[error("Browser open failed: {0}")]
    BrowserOpenFailed(String),
    #[error("OAuth error: {0}")]
    OAuth(String),
    #[error("Timeout: {0}")]
    Timeout(String),
}

#[derive(Error, Debug, Clone)]
pub enum CloudError {
    #[error("Auth error: {0}")]
    Auth(#[from] AuthError),
    #[error("API error: {0}")]
    Api(#[from] ApiError),
    #[error("Cache error: {0}")]
    Cache(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Rate limit exceeded")]
    RateLimit,
    #[error("Quota exceeded")]
    Quota,
    #[error("Integrity error: {0}")]
    Integrity(String),
    #[error("Config error: {0}")]
    Config(String),
    #[error("Not found: {0}")]
    NotFound(String),
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = self.message.to_lowercase();
        if msg.contains("token") || msg.contains("secret") || msg.contains("authorization") {
            write!(f, "API Error {}: [REDACTED]", self.status)
        } else {
            write!(f, "API Error {}: {}", self.status, self.message)
        }
    }
}

impl std::error::Error for ApiError {}
