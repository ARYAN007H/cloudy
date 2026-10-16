use crate::error::CloudError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Locations used by the app. Each can be overridden with an environment
/// variable (handy for tests and portable installs).
pub mod paths {
    use super::*;

    fn dir(env: &str, base: Option<PathBuf>) -> PathBuf {
        if let Ok(v) = std::env::var(env) {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
        let mut p = base.unwrap_or_else(|| PathBuf::from("."));
        p.push("clouddrive");
        p
    }

    pub fn config_dir() -> PathBuf {
        dir("CLOUDDRIVE_CONFIG_DIR", dirs::config_dir())
    }
    pub fn data_dir() -> PathBuf {
        dir("CLOUDDRIVE_DATA_DIR", dirs::data_dir())
    }
    pub fn cache_dir() -> PathBuf {
        dir("CLOUDDRIVE_CACHE_DIR", dirs::cache_dir())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum UploadPolicy {
    MostFreeSpace,
    RoundRobin,
    PinnedAccount(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)] // a config written by an older version must never reset itself
pub struct AppConfig {
    pub mount_point: PathBuf,
    pub cache_size_bytes: u64,
    pub sync_interval_secs: u64,
    pub upload_policy: UploadPolicy,
    pub log_level: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut mount_point = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        mount_point.push("CloudDrive");
        Self {
            mount_point,
            cache_size_bytes: 2 * 1024 * 1024 * 1024,
            sync_interval_secs: 60,
            upload_policy: UploadPolicy::MostFreeSpace,
            log_level: "info".to_string(),
        }
    }
}

impl AppConfig {
    pub const MIN_SYNC_SECS: u64 = 15;
    pub const MAX_SYNC_SECS: u64 = 3600;
    pub const MIN_CACHE_BYTES: u64 = 256 * 1024 * 1024;

    pub fn config_path() -> PathBuf {
        paths::config_dir().join("config.toml")
    }

    /// Clamp values into a safe range.
    pub fn sanitized(mut self) -> Self {
        self.sync_interval_secs = self
            .sync_interval_secs
            .clamp(Self::MIN_SYNC_SECS, Self::MAX_SYNC_SECS);
        self.cache_size_bytes = self.cache_size_bytes.max(Self::MIN_CACHE_BYTES);
        self
    }

    pub fn load_or_default() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str::<AppConfig>(&content) {
                Ok(cfg) => cfg.sanitized(),
                Err(e) => {
                    tracing::warn!("Could not parse {}: {e}; using defaults", path.display());
                    // Keep the broken file around instead of destroying the user's data.
                    let _ = std::fs::rename(&path, path.with_extension("toml.bad"));
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), CloudError> {
        let path = Self::config_path();
        let content = toml::to_string(self).map_err(|e| CloudError::Config(e.to_string()))?;
        write_atomic(&path, content.as_bytes(), false).map_err(|e| CloudError::Config(e.to_string()))
    }
}

/// Write via temp file + rename so a crash can never leave a half-written file.
/// `private` restricts permissions to the owner (0600) for secrets.
pub fn write_atomic(path: &Path, data: &[u8], private: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let _ = private;
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
