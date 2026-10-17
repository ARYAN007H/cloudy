use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum AuthError {
    #[error("Google rejected the saved sign-in (invalid grant)")]
    InvalidGrant,
    #[error("The sign-in for this account has expired or was revoked")]
    RefreshTokenExpired,
    #[error("No Google API credentials configured")]
    NoClientCredentials,
    #[error("Secure storage error: {0}")]
    KeyringError(String),
    #[error("Could not open the browser: {0}")]
    BrowserOpenFailed(String),
    #[error("OAuth error: {0}")]
    OAuth(String),
    #[error("Timed out: {0}")]
    Timeout(String),
    #[error("Sign-in was cancelled")]
    Cancelled,
    #[error("Signed in as {actual} but {expected} was expected")]
    AccountMismatch { expected: String, actual: String },
}

#[derive(Error, Debug, Clone)]
pub enum CloudError {
    #[error("{0}")]
    Auth(#[from] AuthError),
    #[error("{0}")]
    Api(#[from] ApiError),
    #[error("Cache error: {0}")]
    Cache(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Google is rate limiting requests; try again shortly")]
    RateLimit,
    #[error("Storage quota exceeded")]
    Quota,
    #[error("Not enough free space in any single account (need {needed} bytes, best account has {best_free} free)")]
    InsufficientSpace { needed: u64, best_free: u64 },
    #[error("Integrity error: {0}")]
    Integrity(String),
    #[error("Config error: {0}")]
    Config(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Cancelled")]
    Cancelled,
}

impl CloudError {
    /// Stable machine-readable code the UI can switch on.
    pub fn code(&self) -> &'static str {
        match self {
            CloudError::Auth(AuthError::NoClientCredentials) => "no_credentials",
            CloudError::Auth(AuthError::Cancelled) | CloudError::Cancelled => "cancelled",
            CloudError::Auth(AuthError::AccountMismatch { .. }) => "account_mismatch",
            CloudError::Auth(AuthError::Timeout(_)) => "timeout",
            CloudError::Auth(_) => "needs_reauth",
            CloudError::Api(e) if e.status == 401 => "needs_reauth",
            CloudError::Api(e) if e.status == 404 => "not_found",
            CloudError::Api(e) if e.is_storage_full() => "no_space",
            CloudError::Api(e) if e.status == 429 || e.is_rate_limit() => "rate_limited",
            CloudError::Api(_) => "api_error",
            CloudError::Network(_) => "offline",
            CloudError::RateLimit => "rate_limited",
            CloudError::Quota | CloudError::InsufficientSpace { .. } => "no_space",
            CloudError::NotFound(_) => "not_found",
            CloudError::Config(_) => "config",
            CloudError::Io(_) => "io",
            CloudError::Cache(_) => "cache",
            CloudError::Integrity(_) => "integrity",
        }
    }

    /// True when retrying later (without user action) could succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            CloudError::Network(_) | CloudError::RateLimit => true,
            CloudError::Api(e) => e.status >= 500 || e.status == 429 || e.is_rate_limit(),
            _ => false,
        }
    }

    /// True when only the user re-signing in can fix it.
    pub fn needs_reauth(&self) -> bool {
        matches!(self.code(), "needs_reauth")
    }
}

impl From<std::io::Error> for CloudError {
    fn from(e: std::io::Error) -> Self {
        CloudError::Io(e.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    /// Google's machine reason, e.g. `rateLimitExceeded`, `storageQuotaExceeded`.
    pub reason: Option<String>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), reason: None }
    }

    pub fn is_rate_limit(&self) -> bool {
        matches!(
            self.reason.as_deref(),
            Some("rateLimitExceeded") | Some("userRateLimitExceeded") | Some("sharingRateLimitExceeded")
        )
    }

    pub fn is_storage_full(&self) -> bool {
        matches!(self.reason.as_deref(), Some("storageQuotaExceeded"))
    }
}

/// Only hide a message if it plausibly contains an actual secret value.
fn looks_secret(msg: &str) -> bool {
    msg.contains("ya29.")
        || msg.contains("1//")
        || msg.contains("GOCSPX-")
        || msg.to_ascii_lowercase().contains("bearer ")
        || msg.to_ascii_lowercase().contains("client_secret=")
        || msg.to_ascii_lowercase().contains("refresh_token=")
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if looks_secret(&self.message) {
            write!(f, "Google API error {}: [REDACTED]", self.status)
        } else {
            write!(f, "Google API error {}: {}", self.status, self.message)
        }
    }
}

impl std::error::Error for ApiError {}
