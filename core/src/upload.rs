use crate::api::{DriveClient, UploadStatus as ApiUploadStatus};
use crate::cache::MetadataCache;
use crate::error::CloudError;
use crate::types::{TransferInfo, TransferStatus};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use uuid::Uuid;
use md5::{Md5, Digest};

pub struct UploadManager {
    active_transfers: Arc<RwLock<HashMap<String, TransferInfo>>>,
}

impl UploadManager {
    pub fn new() -> Self {
        Self {
            active_transfers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get_transfers(&self) -> Vec<TransferInfo> {
        self.active_transfers.read().values().cloned().collect()
    }

    pub fn get_transfer(&self, id: &str) -> Option<TransferInfo> {
        self.active_transfers.read().get(id).cloned()
    }

    pub async fn enqueue(
        &self,
        local_path: &str,
        remote_parent_id: &str,
        remote_name: &str,
        account: &str,
        total_size: u64,
        api: Arc<DriveClient>,
        cache: Arc<MetadataCache>,
    ) -> Result<String, CloudError> {
        let transfer_id = Uuid::new_v4().to_string();
        
        let info = TransferInfo {
            id: transfer_id.clone(),
            file_name: remote_name.to_string(),
            account: account.to_string(),
            progress_bytes: 0,
            total_bytes: total_size,
            status: TransferStatus::Pending,
        };
        self.active_transfers.write().insert(transfer_id.clone(), info);

        let mime_type = mime_guess::from_path(local_path).first_or_octet_stream().to_string();
        let session_uri = api.initiate_resumable_upload(account, remote_name, remote_parent_id, &mime_type, total_size).await?;
        
        cache.save_upload_session(&transfer_id, local_path, &session_uri, total_size, account, remote_parent_id, remote_name)?;

        self.spawn_upload_task(transfer_id.clone(), local_path.to_string(), session_uri, total_size, 0, api, cache);

        Ok(transfer_id)
    }

    pub async fn resume_pending(&self, api: Arc<DriveClient>, cache: Arc<MetadataCache>) -> Result<Vec<String>, CloudError> {
        let pending = cache.get_pending_uploads()?;
        let mut resumed = Vec::new();

        for upload in pending {
            if let Ok(status) = api.check_upload_status(&upload.session_uri, upload.total_size).await {
                let confirmed = match status {
                    ApiUploadStatus::Incomplete(c) => c,
                    ApiUploadStatus::Complete(_) => upload.total_size,
                    ApiUploadStatus::SessionExpired => {
                        cache.remove_upload_session(&upload.id)?;
                        continue;
                    }
                };

                let info = TransferInfo {
                    id: upload.id.clone(),
                    file_name: upload.remote_name.clone(),
                    account: upload.account_email.clone(),
                    progress_bytes: confirmed,
                    total_bytes: upload.total_size,
                    status: TransferStatus::Pending,
                };
                self.active_transfers.write().insert(upload.id.clone(), info);
                
                self.spawn_upload_task(upload.id.clone(), upload.local_path.clone(), upload.session_uri.clone(), upload.total_size, confirmed, api.clone(), cache.clone());
                resumed.push(upload.id);
            } else {
                // Transient error (network, etc.) — leave session in SQLite for next resume attempt
                tracing::warn!("Failed to check upload status for '{}', will retry later", upload.id);
                continue;
            }
        }
        Ok(resumed)
    }

    pub fn cancel(&self, transfer_id: &str) -> Result<(), CloudError> {
        let mut transfers = self.active_transfers.write();
        if let Some(t) = transfers.get_mut(transfer_id) {
            t.status = TransferStatus::Cancelled;
        }
        Ok(())
    }

    fn spawn_upload_task(&self, id: String, local_path: String, session_uri: String, total_size: u64, mut offset: u64, api: Arc<DriveClient>, cache: Arc<MetadataCache>) {
        let active_transfers = self.active_transfers.clone();

        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("Cannot spawn upload task: {}", e);
                return;
            }
        };
        
        handle.spawn(async move {
            if let Some(t) = active_transfers.write().get_mut(&id) {
                t.status = TransferStatus::InProgress;
            }

            // Handle 0-byte files: the file was already created by initiate_resumable_upload
            if total_size == 0 {
                // Send a final empty PUT to complete the upload
                match api.upload_chunk(&session_uri, vec![], 0, 0).await {
                    Ok(ApiUploadStatus::Complete(drive_file)) => {
                        let account = if let Some(t) = active_transfers.read().get(&id) {
                            t.account.clone()
                        } else { return; };
                        let _ = cache.upsert_file(&drive_file, &account);
                        let _ = cache.remove_upload_session(&id);
                        if let Some(t) = active_transfers.write().get_mut(&id) {
                            t.status = TransferStatus::Completed;
                            t.progress_bytes = 0;
                        }
                    },
                    _ => {
                        let _ = cache.remove_upload_session(&id);
                        if let Some(t) = active_transfers.write().get_mut(&id) {
                            t.status = TransferStatus::Completed;
                            t.progress_bytes = 0;
                        }
                    }
                }
                return;
            }

            let mut file = match File::open(&local_path).await {
                Ok(f) => f,
                Err(e) => {
                    if let Some(t) = active_transfers.write().get_mut(&id) {
                        t.status = TransferStatus::Failed(e.to_string());
                    }
                    return;
                }
            };

            if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
                if let Some(t) = active_transfers.write().get_mut(&id) {
                    t.status = TransferStatus::Failed(e.to_string());
                }
                return;
            }

            let chunk_size = 5 * 1024 * 1024; // 5MB
            let mut buffer = vec![0; chunk_size];

            loop {
                // Check if cancelled
                if let Some(t) = active_transfers.read().get(&id) {
                    if matches!(t.status, TransferStatus::Cancelled) {
                        let _ = cache.remove_upload_session(&id);
                        return;
                    }
                } else {
                    return;
                }

                let bytes_read = match file.read(&mut buffer).await {
                    Ok(n) => n,
                    Err(e) => {
                        if let Some(t) = active_transfers.write().get_mut(&id) {
                            t.status = TransferStatus::Failed(e.to_string());
                        }
                        return;
                    }
                };

                if bytes_read == 0 {
                    break;
                }

                let chunk_data = buffer[..bytes_read].to_vec();
                
                match api.upload_chunk(&session_uri, chunk_data, offset, total_size).await {
                    Ok(ApiUploadStatus::Complete(drive_file)) => {
                        // Checksum verification
                        if let Some(ref remote_md5) = drive_file.md5_checksum {
                            if let Ok(mut hash_file) = std::fs::File::open(&local_path) {
                                let mut hasher = Md5::new();
                                if std::io::copy(&mut hash_file, &mut hasher).is_ok() {
                                    let local_md5 = format!("{:x}", hasher.finalize());
                                    if local_md5 != *remote_md5 {
                                        if let Some(t) = active_transfers.write().get_mut(&id) {
                                            t.status = TransferStatus::Failed("MD5 checksum mismatch".to_string());
                                        }
                                        return;
                                    }
                                }
                            }
                        }
                        
                        // Update cache with the new file metadata so it appears in the tree
                        let account = if let Some(t) = active_transfers.read().get(&id) {
                            t.account.clone()
                        } else { "".to_string() };
                        if !account.is_empty() {
                            let _ = cache.upsert_file(&drive_file, &account);
                        }

                        let _ = cache.remove_upload_session(&id);
                        if let Some(t) = active_transfers.write().get_mut(&id) {
                            t.status = TransferStatus::Completed;
                            t.progress_bytes = total_size;
                        }
                        return;
                    },
                    Ok(ApiUploadStatus::Incomplete(confirmed)) => {
                        offset = confirmed;
                        if let Some(t) = active_transfers.write().get_mut(&id) {
                            t.progress_bytes = offset;
                        }
                        let _ = cache.update_upload_progress(&id, offset);
                        // Re-seek file to match confirmed offset from server
                        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
                            if let Some(t) = active_transfers.write().get_mut(&id) {
                                t.status = TransferStatus::Failed(format!("Seek error: {}", e));
                            }
                            return;
                        }
                    },
                    Ok(ApiUploadStatus::SessionExpired) => {
                        if let Some(t) = active_transfers.write().get_mut(&id) {
                            t.status = TransferStatus::Failed("Session expired".into());
                        }
                        return;
                    },
                    Err(e) => {
                        if let Some(t) = active_transfers.write().get_mut(&id) {
                            t.status = TransferStatus::Failed(e.to_string());
                        }
                        return;
                    }
                }
            }
        });
    }
}
