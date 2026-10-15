use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use crate::error::CloudError;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum UploadPolicy {
    MostFreeSpace,
    RoundRobin,
    PinnedAccount(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
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
    pub fn config_path() -> PathBuf {
        let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("clouddrive");
        path.push("config.toml");
        path
    }

    pub fn load_or_default() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(config) = toml::from_str(&content) {
                    return config;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) -> Result<(), CloudError> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CloudError::Config(e.to_string()))?;
        }
        let content = toml::to_string(self).map_err(|e| CloudError::Config(e.to_string()))?;
        std::fs::write(path, content).map_err(|e| CloudError::Config(e.to_string()))?;
        Ok(())
    }
}
