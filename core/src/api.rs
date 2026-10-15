use crate::auth::OAuthManager;
use crate::error::{ApiError, AuthError, CloudError};
use crate::types::{Change, DriveFile, StorageQuota};
use bytes::Bytes;
use reqwest::{Client, RequestBuilder, StatusCode};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

pub struct DriveClient {
    client: Client,
    auth: Arc<OAuthManager>,
}

pub enum UploadStatus {
    Complete(DriveFile),
    Incomplete(u64),
    SessionExpired,
}

impl DriveClient {
    pub fn new(auth: Arc<OAuthManager>) -> Self {
        Self {
            client: Client::new(),
            auth,
        }
    }

    pub async fn execute_with_retry<F, Fut, T>(&self, account: &str, mut operation: F) -> Result<T, CloudError>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<T, CloudError>>,
    {
        let mut retries = 0;
        let mut token = self.auth.get_access_token(account).await?;

        loop {
            match operation(token.clone()).await {
                Ok(val) => return Ok(val),
                Err(e) => match &e {
                    CloudError::Api(ApiError { status, .. }) if *status == 401 => {
                        if retries == 0 {
                            token = self.auth.refresh_access_token(account).await?;
                            retries += 1;
                            continue;
                        } else {
                            return Err(CloudError::Auth(AuthError::RefreshTokenExpired));
                        }
                    }
                    CloudError::Api(ApiError { status, message }) if *status == 429 || (*status == 403 && (message.contains("rateLimit") || message.contains("userRateLimit") || message.contains("quotaExceeded"))) => {
                        if retries < 5 {
                            let backoff = 2u64.pow(retries) + rand::random::<u64>() % 2;
                            let delay = backoff.min(32);
                            warn!("Rate limited. Retrying in {}s", delay);
                            sleep(Duration::from_secs(delay)).await;
                            retries += 1;
                            continue;
                        } else {
                            return Err(CloudError::RateLimit);
                        }
                    }
                    CloudError::Api(ApiError { status, .. }) if *status >= 500 => {
                        if retries < 5 {
                            let backoff = 2u64.pow(retries) + rand::random::<u64>() % 2;
                            let delay = backoff.min(32);
                            warn!("Server error. Retrying in {}s", delay);
                            sleep(Duration::from_secs(delay)).await;
                            retries += 1;
                            continue;
                        } else {
                            return Err(e);
                        }
                    }
                    _ => return Err(e),
                },
            }
        }
    }

    pub async fn list_all_files(&self, account: &str) -> Result<Vec<DriveFile>, CloudError> {
        let mut all_files = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let token_clone = page_token.clone();
            let res = self.execute_with_retry(account, |access_token| {
                let mut req = self.client.get("https://www.googleapis.com/drive/v3/files")
                    .bearer_auth(access_token)
                    .query(&[
                        ("q", "trashed=false"),
                        ("fields", "nextPageToken, files(id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed,thumbnailLink)"),
                    ]);
                if let Some(pt) = &token_clone {
                    req = req.query(&[("pageToken", pt)]);
                }
                async move { self.handle_json_response::<serde_json::Value>(req).await }
            }).await?;

            if let Some(files) = res.get("files").and_then(|f| f.as_array()) {
                for file_val in files {
                    if let Ok(file) = serde_json::from_value(file_val.clone()) {
                        let mut df: DriveFile = file;
                        df.is_folder = df.mime_type == "application/vnd.google-apps.folder";
                        all_files.push(df);
                    }
                }
            }

            if let Some(next_page) = res.get("nextPageToken").and_then(|s| s.as_str()) {
                page_token = Some(next_page.to_string());
            } else {
                break;
            }
        }

        Ok(all_files)
    }

    pub async fn get_file_metadata(&self, account: &str, file_id: &str) -> Result<DriveFile, CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let req = self.client.get(format!("https://www.googleapis.com/drive/v3/files/{}", file_id))
                .bearer_auth(access_token)
                .query(&[("fields", "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed,thumbnailLink")]);
            let mut file: DriveFile = self.handle_json_response(req).await?;
            file.is_folder = file.mime_type == "application/vnd.google-apps.folder";
            Ok(file)
        }).await
    }

    pub async fn read_content_range(&self, account: &str, file_id: &str, offset: u64, length: u64) -> Result<Bytes, CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let req = self.client.get(format!("https://www.googleapis.com/drive/v3/files/{}?alt=media", file_id))
                .bearer_auth(access_token)
                .header("Range", format!("bytes={}-{}", offset, offset + length - 1));
            
            let res = req.send().await.map_err(|e| CloudError::Network(e.to_string()))?;
            let status = res.status();
            if status.is_success() || status == StatusCode::PARTIAL_CONTENT {
                Ok(res.bytes().await.map_err(|e| CloudError::Network(e.to_string()))?)
            } else {
                Err(CloudError::Api(ApiError { status: status.as_u16(), message: res.text().await.unwrap_or_default() }))
            }
        }).await
    }

    pub async fn export_workspace_file(&self, account: &str, file_id: &str, export_mime_type: &str) -> Result<Bytes, CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let req = self.client.get(format!("https://www.googleapis.com/drive/v3/files/{}/export", file_id))
                .bearer_auth(access_token)
                .query(&[("mimeType", export_mime_type)]);
            let res = req.send().await.map_err(|e| CloudError::Network(e.to_string()))?;
            if res.status().is_success() {
                Ok(res.bytes().await.map_err(|e| CloudError::Network(e.to_string()))?)
            } else {
                Err(CloudError::Api(ApiError { status: res.status().as_u16(), message: res.text().await.unwrap_or_default() }))
            }
        }).await
    }

    pub async fn initiate_resumable_upload(&self, account: &str, name: &str, parent_id: &str, mime_type: &str, _size: u64) -> Result<String, CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let body = serde_json::json!({
                "name": name,
                "parents": [parent_id]
            });
            let req = self.client.post("https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable")
                .bearer_auth(access_token)
                .header("X-Upload-Content-Type", mime_type)
                .json(&body);
            let res = req.send().await.map_err(|e| CloudError::Network(e.to_string()))?;
            if res.status().is_success() {
                let loc = res.headers().get("Location").and_then(|h| h.to_str().ok()).unwrap_or_default().to_string();
                Ok(loc)
            } else {
                Err(CloudError::Api(ApiError { status: res.status().as_u16(), message: res.text().await.unwrap_or_default() }))
            }
        }).await
    }

    pub async fn upload_chunk(&self, session_uri: &str, data: Vec<u8>, offset: u64, total: u64) -> Result<UploadStatus, CloudError> {
        let length = data.len() as u64;
        let req = self.client.put(session_uri)
            .header("Content-Range", format!("bytes {}-{}/{}", offset, offset + length - 1, total))
            .body(data);
        
        let res = req.send().await.map_err(|e| CloudError::Network(e.to_string()))?;
        let status = res.status();
        if status == StatusCode::PERMANENT_REDIRECT || status.as_u16() == 308 {
            let range = res.headers().get("Range").and_then(|h| h.to_str().ok()).unwrap_or("");
            let confirmed = range.split('-').last().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            Ok(UploadStatus::Incomplete(confirmed + 1))
        } else if status.is_success() {
            let json = res.json::<serde_json::Value>().await.map_err(|e| CloudError::Network(e.to_string()))?;
            let mut file: DriveFile = serde_json::from_value(json).map_err(|e| CloudError::Api(ApiError{status: 200, message: e.to_string()}))?;
            file.is_folder = file.mime_type == "application/vnd.google-apps.folder";
            Ok(UploadStatus::Complete(file))
        } else if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
            Ok(UploadStatus::SessionExpired)
        } else {
            Err(CloudError::Api(ApiError { status: status.as_u16(), message: res.text().await.unwrap_or_default() }))
        }
    }

    pub async fn check_upload_status(&self, session_uri: &str, total: u64) -> Result<UploadStatus, CloudError> {
        let req = self.client.put(session_uri)
            .header("Content-Range", format!("bytes */{}", total));
        let res = req.send().await.map_err(|e| CloudError::Network(e.to_string()))?;
        let status = res.status();
        if status == StatusCode::PERMANENT_REDIRECT || status.as_u16() == 308 {
            let range = res.headers().get("Range").and_then(|h| h.to_str().ok()).unwrap_or("");
            let confirmed = range.split('-').last().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            Ok(UploadStatus::Incomplete(confirmed + 1))
        } else if status.is_success() {
            Ok(UploadStatus::Incomplete(total)) // Assuming fully uploaded but checking status doesn't return file
        } else {
            Ok(UploadStatus::SessionExpired)
        }
    }

    pub async fn create_folder(&self, account: &str, name: &str, parent_id: &str) -> Result<DriveFile, CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let body = serde_json::json!({
                "name": name,
                "mimeType": "application/vnd.google-apps.folder",
                "parents": [parent_id]
            });
            let req = self.client.post("https://www.googleapis.com/drive/v3/files")
                .bearer_auth(access_token)
                .query(&[("fields", "id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed")])
                .json(&body);
            let mut file: DriveFile = self.handle_json_response(req).await?;
            file.is_folder = true;
            Ok(file)
        }).await
    }

    pub async fn rename_file(&self, account: &str, file_id: &str, new_name: &str) -> Result<(), CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let body = serde_json::json!({ "name": new_name });
            let req = self.client.patch(format!("https://www.googleapis.com/drive/v3/files/{}", file_id))
                .bearer_auth(access_token)
                .json(&body);
            self.handle_empty_response(req).await
        }).await
    }

    pub async fn move_file(&self, account: &str, file_id: &str, new_parent_id: &str, old_parent_id: &str) -> Result<(), CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let req = self.client.patch(format!("https://www.googleapis.com/drive/v3/files/{}", file_id))
                .bearer_auth(access_token)
                .query(&[
                    ("addParents", new_parent_id),
                    ("removeParents", old_parent_id)
                ]);
            self.handle_empty_response(req).await
        }).await
    }

    pub async fn trash_file(&self, account: &str, file_id: &str) -> Result<(), CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let body = serde_json::json!({ "trashed": true });
            let req = self.client.patch(format!("https://www.googleapis.com/drive/v3/files/{}", file_id))
                .bearer_auth(access_token)
                .json(&body);
            self.handle_empty_response(req).await
        }).await
    }

    pub async fn delete_permanently(&self, account: &str, file_id: &str) -> Result<(), CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let req = self.client.delete(format!("https://www.googleapis.com/drive/v3/files/{}", file_id))
                .bearer_auth(access_token);
            self.handle_empty_response(req).await
        }).await
    }

    pub async fn get_changes(&self, account: &str, page_token: &str) -> Result<(Vec<Change>, String), CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let req = self.client.get("https://www.googleapis.com/drive/v3/changes")
                .bearer_auth(access_token)
                .query(&[
                    ("pageToken", page_token),
                    ("fields", "nextPageToken, newStartPageToken, changes(fileId,removed,file(id,name,mimeType,parents,size,modifiedTime,md5Checksum,trashed,thumbnailLink))")
                ]);
            let res: serde_json::Value = self.handle_json_response(req).await?;
            let mut changes = Vec::new();
            if let Some(arr) = res.get("changes").and_then(|v| v.as_array()) {
                for c in arr {
                    if let Ok(mut change) = serde_json::from_value::<Change>(c.clone()) {
                        if let Some(ref mut f) = change.file {
                            f.is_folder = f.mime_type == "application/vnd.google-apps.folder";
                        }
                        changes.push(change);
                    }
                }
            }
            let next_token = res.get("nextPageToken").or(res.get("newStartPageToken"))
                .and_then(|s| s.as_str()).unwrap_or(page_token).to_string();
            Ok((changes, next_token))
        }).await
    }

    pub async fn get_start_page_token(&self, account: &str) -> Result<String, CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let req = self.client.get("https://www.googleapis.com/drive/v3/changes/startPageToken")
                .bearer_auth(access_token);
            let res: serde_json::Value = self.handle_json_response(req).await?;
            let token = res.get("startPageToken").and_then(|s| s.as_str()).ok_or_else(|| CloudError::Api(ApiError{status:500, message: "Missing token".into()}))?.to_string();
            Ok(token)
        }).await
    }

    pub async fn get_quota(&self, account: &str) -> Result<StorageQuota, CloudError> {
        self.execute_with_retry(account, |access_token| async move {
            let req = self.client.get("https://www.googleapis.com/drive/v3/about")
                .bearer_auth(access_token)
                .query(&[("fields", "storageQuota")]);
            let res: serde_json::Value = self.handle_json_response(req).await?;
            if let Some(quota) = res.get("storageQuota") {
                Ok(StorageQuota {
                    limit: quota.get("limit").and_then(|s| s.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0),
                    usage: quota.get("usage").and_then(|s| s.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0),
                    usage_in_drive: quota.get("usageInDrive").and_then(|s| s.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0),
                })
            } else {
                Err(CloudError::Api(ApiError{status:500, message: "Missing quota".into()}))
            }
        }).await
    }

    pub async fn get_user_email(&self, access_token: &str) -> Result<String, CloudError> {
        let req = self.client.get("https://www.googleapis.com/oauth2/v2/userinfo")
            .bearer_auth(access_token);
        let res: serde_json::Value = self.handle_json_response(req).await?;
        let email = res.get("email").and_then(|s| s.as_str()).ok_or_else(|| CloudError::Api(ApiError{status:500, message: "Missing email".into()}))?.to_string();
        Ok(email)
    }

    async fn handle_json_response<T: serde::de::DeserializeOwned>(&self, req: RequestBuilder) -> Result<T, CloudError> {
        let res = req.send().await.map_err(|e| CloudError::Network(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            res.json().await.map_err(|e| CloudError::Network(e.to_string()))
        } else {
            let text = res.text().await.unwrap_or_default();
            let message = if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                val.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or(text)
            } else {
                text
            };
            Err(CloudError::Api(ApiError { status: status.as_u16(), message }))
        }
    }

    async fn handle_empty_response(&self, req: RequestBuilder) -> Result<(), CloudError> {
        let res = req.send().await.map_err(|e| CloudError::Network(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            Ok(())
        } else {
            let text = res.text().await.unwrap_or_default();
            let message = if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                val.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or(text)
            } else {
                text
            };
            Err(CloudError::Api(ApiError { status: status.as_u16(), message }))
        }
    }
}
