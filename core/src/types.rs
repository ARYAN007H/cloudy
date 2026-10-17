use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
pub const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";

/// Google's JSON APIs encode 64-bit integers as *strings* (`"size": "12345"`).
/// Accept strings, integers and floats so a format change can never silently
/// drop a file again.
pub fn de_opt_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        Num(u64),
        Float(f64),
        Str(String),
    }
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        None => None,
        Some(NumOrStr::Num(n)) => Some(n),
        Some(NumOrStr::Float(f)) => Some(f.max(0.0) as u64),
        Some(NumOrStr::Str(s)) => s.trim().parse::<u64>().ok(),
    })
}

/// Wire format of a Drive `File` resource (camelCase, string sizes).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDriveFile {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    size: Option<u64>,
    #[serde(default)]
    modified_time: String,
    #[serde(default)]
    md5_checksum: Option<String>,
    #[serde(default)]
    trashed: bool,
    #[serde(default)]
    thumbnail_link: Option<String>,
    #[serde(default = "yes")]
    owned_by_me: bool,
}

fn yes() -> bool {
    true
}

impl From<RawDriveFile> for DriveFile {
    fn from(r: RawDriveFile) -> Self {
        let is_folder = r.mime_type == FOLDER_MIME;
        DriveFile {
            id: r.id,
            name: r.name,
            mime_type: r.mime_type,
            parents: r.parents,
            size: r.size,
            modified_time: r.modified_time,
            md5_checksum: r.md5_checksum,
            trashed: r.trashed,
            is_folder,
            thumbnail_link: r.thumbnail_link,
            owned_by_me: r.owned_by_me,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(from = "RawDriveFile")]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub parents: Vec<String>,
    pub size: Option<u64>,
    pub modified_time: String,
    pub md5_checksum: Option<String>,
    pub trashed: bool,
    pub is_folder: bool,
    pub thumbnail_link: Option<String>,
    /// Files merely shared with the user don't live in their storage.
    pub owned_by_me: bool,
}

/// The `fields` selector used for every Drive file listing / lookup so that
/// all code paths receive the same set of properties.
pub const FILE_FIELDS: &str =
    "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed,thumbnailLink,ownedByMe";

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StorageQuota {
    /// Total allowance in bytes. `0` means "unlimited / not reported".
    pub limit: u64,
    /// Used across Drive + Gmail + Photos.
    pub usage: u64,
    pub usage_in_drive: u64,
}

impl StorageQuota {
    pub fn free(&self) -> u64 {
        if self.limit == 0 {
            u64::MAX
        } else {
            self.limit.saturating_sub(self.usage)
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AccountInfo {
    pub email: String,
    pub display_name: String,
    pub is_connected: bool,
    pub needs_reauth: bool,
    pub quota: Option<StorageQuota>,
}

/// Everything the UI needs to render one account card.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AccountSummary {
    pub email: String,
    pub display_name: Option<String>,
    pub quota: Option<StorageQuota>,
    pub sync: SyncStatus,
    /// Number of files currently indexed for this account.
    pub files_indexed: u64,
    /// Human readable problem (quota lookup failed, token revoked, ...).
    pub error: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct RawChange {
    #[serde(default)]
    file_id: String,
    #[serde(default)]
    removed: bool,
    #[serde(default)]
    file: Option<DriveFile>,
}

impl From<RawChange> for Change {
    fn from(r: RawChange) -> Self {
        // Drive sometimes only sets `fileId` on `file`; be defensive.
        let file_id = if r.file_id.is_empty() {
            r.file.as_ref().map(|f| f.id.clone()).unwrap_or_default()
        } else {
            r.file_id
        };
        Change { file_id, removed: r.removed, file: r.file }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(from = "RawChange")]
pub struct Change {
    pub file_id: String,
    pub removed: bool,
    pub file: Option<DriveFile>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum SyncStatus {
    Idle,
    Syncing,
    Error(String),
    NeedsReauth,
    Offline,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferDirection {
    #[default]
    Upload,
    Download,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TransferInfo {
    pub id: String,
    pub file_name: String,
    pub account: String,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub status: TransferStatus,
    #[serde(default)]
    pub direction: TransferDirection,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum TransferStatus {
    Pending,
    InProgress,
    Paused,
    Completed,
    Failed(String),
    Cancelled,
}

impl TransferStatus {
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            TransferStatus::Completed | TransferStatus::Failed(_) | TransferStatus::Cancelled
        )
    }
}

/// One concrete (account, Drive id) behind a virtual node.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeSource {
    pub account: String,
    pub drive_file_id: String,
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
    /// Parent in the virtual tree (None for the synthetic root).
    #[serde(default)]
    pub parent_id: Option<String>,
    /// For pooled folders that merge same-named folders from several
    /// accounts: every concrete folder behind this node.
    #[serde(default)]
    pub sources: Vec<NodeSource>,
}

impl VirtualNode {
    /// All concrete Drive objects this node stands for.
    pub fn all_sources(&self) -> Vec<NodeSource> {
        if !self.sources.is_empty() {
            return self.sources.clone();
        }
        match (&self.account, &self.drive_file_id) {
            (Some(a), Some(d)) => vec![NodeSource { account: a.clone(), drive_file_id: d.clone() }],
            _ => vec![],
        }
    }
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AddAccountResult {
    pub email: String,
    pub display_name: Option<String>,
    /// The account was already linked; only its sign-in was refreshed.
    pub already_linked: bool,
}

/// A file sitting in some account's Drive trash (still using quota!).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TrashItem {
    pub account: String,
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
    pub modified_time: String,
    pub is_folder: bool,
}

/// Events pushed to UIs so they never have to poll.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    /// Sync state of one account changed (or progressed).
    Sync { account: String, status: SyncStatus, files_indexed: u64 },
    /// The virtual tree was rebuilt – views should refresh.
    TreeChanged,
    /// Accounts were added / removed / re-authorised.
    AccountsChanged,
    /// Upload/download progress or state change.
    Transfer(TransferInfo),
}
