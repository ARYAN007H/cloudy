use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    pub size: Option<u64>,
    #[serde(default)]
    pub modified_time: String,
    pub md5_checksum: Option<String>,
    #[serde(default)]
    pub trashed: bool,
    #[serde(default)]
    pub is_folder: bool,
    #[serde(rename = "thumbnailLink", default)]
    pub thumbnail_link: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageQuota {
    pub limit: u64,
    pub usage: u64,
    pub usage_in_drive: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AccountInfo {
    pub email: String,
    pub display_name: String,
    pub is_connected: bool,
    pub needs_reauth: bool,
    pub quota: Option<StorageQuota>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Change {
    pub file_id: String,
    pub removed: bool,
    pub file: Option<DriveFile>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum SyncStatus {
    Idle,
    Syncing,
    Error(String),
    NeedsReauth,
    Offline,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TransferInfo {
    pub id: String,
    pub file_name: String,
    pub account: String,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub status: TransferStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum TransferStatus {
    Pending,
    InProgress,
    Paused,
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VirtualNode {
    pub id: String,
    pub drive_file_id: Option<String>,
    pub account: Option<String>,
    pub name: String,
    pub display_name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mime_type: String,
    pub modified_time: DateTime<Utc>,
    pub is_read_only: bool,
    pub children_ids: Vec<String>,
    pub is_workspace_file: bool,
    pub thumbnail_link: Option<String>,
}

pub fn workspace_export_mime_and_ext(mime: &str) -> Option<(&'static str, &'static str)> {
    match mime {
        "application/vnd.google-apps.document" => Some(("application/vnd.openxmlformats-officedocument.wordprocessingml.document", ".docx")),
        "application/vnd.google-apps.spreadsheet" => Some(("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", ".xlsx")),
        "application/vnd.google-apps.presentation" => Some(("application/vnd.openxmlformats-officedocument.presentationml.presentation", ".pptx")),
        "application/vnd.google-apps.drawing" => Some(("image/png", ".png")),
        _ => None,
    }
}
