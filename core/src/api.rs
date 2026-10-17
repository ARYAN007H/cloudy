//! Google Drive v3 REST client.

use crate::auth::OAuthManager;
use crate::endpoints::Endpoints;
use crate::error::{ApiError, AuthError, CloudError};
use crate::types::{Change, DriveFile, StorageQuota, FILE_FIELDS, FOLDER_MIME};
use bytes::Bytes;
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

/// Only files the user owns count against their 15 GB (and are what the pool is about).
const OWNED_QUERY: &str = "'me' in owners and trashed = false";
const MAX_ATTEMPTS: u32 = 5;

pub enum UploadStatus {
    Complete(DriveFile),
    /// Next byte the server expects.
    Incomplete(u64),
    SessionExpired,
}

#[derive(Debug, Clone)]
pub struct MediaChunk {
    pub bytes: Bytes,
    pub start: u64,
    /// Total object size when the server reported it.
    pub total: Option<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct ChangesPage {
    pub changes: Vec<Change>,
    pub next_page_token: Option<String>,
    pub new_start_page_token: Option<String>,
}

pub struct DriveClient {
    client: Client,
    auth: Arc<OAuthManager>,
    ep: Endpoints,
}

// ------------------------------------------------------------------ parsing

/// Parse a `files.list` page. Returns (files, nextPageToken, skipped_count).
/// Entries that fail to parse are *counted and logged* – never silently lost.
pub fn parse_files_page(v: &Value) -> (Vec<DriveFile>, Option<String>, usize) {
    let mut files = Vec::new();
    let mut skipped = 0;
    if let Some(arr) = v.get("files").and_then(|f| f.as_array()) {
        for item in arr {
            match serde_json::from_value::<DriveFile>(item.clone()) {
                Ok(f) => files.push(f),
                Err(e) => {
                    skipped += 1;
                    warn!("Skipping unparsable Drive file entry: {e}");
                }
            }
        }
    }
    let next = v.get("nextPageToken").and_then(|s| s.as_str()).map(str::to_string);
    (files, next, skipped)
}

pub fn parse_changes_page(v: &Value) -> ChangesPage {
    let mut page = ChangesPage::default();
    if let Some(arr) = v.get("changes").and_then(|c| c.as_array()) {
        for item in arr {
            match serde_json::from_value::<Change>(item.clone()) {
                Ok(c) if !c.file_id.is_empty() => page.changes.push(c),
                Ok(_) => {}
                Err(e) => warn!("Skipping unparsable change entry: {e}"),
            }
        }
    }
    page.next_page_token = v.get("nextPageToken").and_then(|s| s.as_str()).map(str::to_string);
    page.new_start_page_token = v.get("newStartPageToken").and_then(|s| s.as_str()).map(str::to_string);
    page
}

fn json_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

pub fn parse_quota(about: &Value) -> Option<StorageQuota> {
    let q = about.get("storageQuota")?;
    Some(StorageQuota {
        limit: json_u64(q.get("limit")),
        usage: json_u64(q.get("usage")),
        usage_in_drive: json_u64(q.get("usageInDrive")),
    })
}

/// Turn a non-success response body into an [`ApiError`].
fn api_error(status: u16, body: &str) -> ApiError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let err = parsed.as_ref().and_then(|v| v.get("error"));
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(300).collect());
    let reason = err
        .and_then(|e| e.get("errors"))
        .and_then(|e| e.get(0))
        .and_then(|e| e.get("reason"))
        .and_then(|r| r.as_str())
        .map(str::to_string);
    ApiError { status, message, reason }
}

fn parse_content_range(h: &str) -> Option<(u64, u64, Option<u64>)> {
    // "bytes 0-99/1000" | "bytes */1000"
    let rest = h.trim().strip_prefix("bytes")?.trim();
    let (range, total) = rest.split_once('/')?;
    let total = total.trim().parse::<u64>().ok();
    if range.trim() == "*" {
        return Some((0, 0, total));
    }
    let (a, b) = range.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, total))
}

fn retry_after(res: &Response) -> Option<Duration> {
    res.headers()
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|s| Duration::from_secs(s.min(60)))
}

fn backoff(attempt: u32) -> Duration {
    let base = 1u64 << attempt.min(5); // 1,2,4,8,16,32
    Duration::from_millis(base * 1000 + rand::random::<u64>() % 700)
}

impl DriveClient {
    pub fn new(auth: Arc<OAuthManager>, ep: Endpoints) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60)) // per read, so big transfers are fine
            .user_agent(concat!("Cloudy/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self { client, auth, ep }
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.ep
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.ep.drive_base, path)
    }

    /// Core request loop: auth, 401 refresh, rate-limit / 5xx back-off.
    ///
    /// `idempotent` requests are also retried on network errors; others are
    /// not, to avoid e.g. creating a folder twice.
    pub async fn call<F>(&self, account: &str, idempotent: bool, build: F) -> Result<Response, CloudError>
    where
        F: Fn(&Client) -> RequestBuilder,
    {
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            let token = self.auth.get_access_token(account).await?;
            let sent = build(&self.client).bearer_auth(&token).send().await;
            let res = match sent {
                Ok(r) => r,
                Err(e) => {
                    attempt += 1;
                    if idempotent && attempt < MAX_ATTEMPTS {
                        debug!("network error ({e}); retry {attempt}");
                        sleep(backoff(attempt - 1)).await;
                        continue;
                    }
                    return Err(CloudError::Network(e.without_url().to_string()));
                }
            };
            let status = res.status();
            if status.is_success() || status == StatusCode::PARTIAL_CONTENT {
                return Ok(res);
            }
            let wait = retry_after(&res);
            let code = status.as_u16();
            let body = res.text().await.unwrap_or_default();
            let err = api_error(code, &body);

            if code == 401 {
                if !refreshed {
                    refreshed = true;
                    self.auth.invalidate(account);
                    continue;
                }
                self.auth.mark_needs_reauth(account);
                return Err(AuthError::RefreshTokenExpired.into());
            }
            let retryable = code == 429
                || err.is_rate_limit()
                || code == 503
                || (code >= 500 && idempotent);
            attempt += 1;
            if retryable && attempt < MAX_ATTEMPTS {
                let d = wait.unwrap_or_else(|| backoff(attempt - 1));
                warn!("Drive returned {code}; retrying in {:.1}s", d.as_secs_f32());
                sleep(d).await;
                continue;
            }
            if code == 429 || err.is_rate_limit() {
                return Err(CloudError::RateLimit);
            }
            return Err(CloudError::Api(err));
        }
    }

    async fn json<F>(&self, account: &str, idempotent: bool, build: F) -> Result<Value, CloudError>
    where
        F: Fn(&Client) -> RequestBuilder,
    {
        let res = self.call(account, idempotent, build).await?;
        res.json::<Value>().await.map_err(|e| CloudError::Network(format!("bad response from Drive: {e}")))
    }

    // ------------------------------------------------------------- listing

    pub async fn list_files_page(
        &self,
        account: &str,
        page_token: Option<&str>,
    ) -> Result<(Vec<DriveFile>, Option<String>), CloudError> {
        let url = self.url("/files");
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let v = self
            .json(account, true, |c| {
                let mut r = c.get(&url).query(&[
                    ("q", OWNED_QUERY),
                    ("pageSize", "1000"),
                    ("spaces", "drive"),
                    ("fields", fields.as_str()),
                ]);
                if let Some(pt) = page_token {
                    r = r.query(&[("pageToken", pt)]);
                }
                r
            })
            .await?;
        let (files, next, skipped) = parse_files_page(&v);
        if skipped > 0 {
            warn!("{skipped} file entries on this page could not be read");
        }
        Ok((files, next))
    }

    /// Convenience: every owned, non-trashed file (all pages).
    pub async fn list_all_files(&self, account: &str) -> Result<Vec<DriveFile>, CloudError> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let (mut files, next) = self.list_files_page(account, token.as_deref()).await?;
            all.append(&mut files);
            match next {
                Some(n) => token = Some(n),
                None => break,
            }
        }
        Ok(all)
    }

    /// Trashed files still occupy quota; list them so users can free space.
    pub async fn list_trashed(&self, account: &str) -> Result<Vec<DriveFile>, CloudError> {
        let url = self.url("/files");
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let v = self
                .json(account, true, |c| {
                    let mut r = c.get(&url).query(&[
                        ("q", "'me' in owners and trashed = true"),
                        ("pageSize", "1000"),
                        ("spaces", "drive"),
                        ("fields", fields.as_str()),
                    ]);
                    if let Some(pt) = &token {
                        r = r.query(&[("pageToken", pt.as_str())]);
                    }
                    r
                })
                .await?;
            let (mut files, next, _) = parse_files_page(&v);
            all.append(&mut files);
            match next {
                Some(n) => token = Some(n),
                None => break,
            }
        }
        Ok(all)
    }

    pub async fn get_file_metadata(&self, account: &str, file_id: &str) -> Result<DriveFile, CloudError> {
        let url = self.url(&format!("/files/{file_id}"));
        let v = self.json(account, true, |c| c.get(&url).query(&[("fields", FILE_FIELDS)])).await?;
        serde_json::from_value(v).map_err(|e| CloudError::Network(format!("unreadable file metadata: {e}")))
    }

    // ------------------------------------------------------------- content

    /// Fetch bytes `[start, end]` (inclusive; `None` = to the end).
    pub async fn fetch_range(
        &self,
        account: &str,
        file_id: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<MediaChunk, CloudError> {
        let url = self.url(&format!("/files/{file_id}"));
        let range = match end {
            Some(e) => format!("bytes={start}-{e}"),
            None => format!("bytes={start}-"),
        };
        let res = match self
            .call(account, true, |c| c.get(&url).query(&[("alt", "media")]).header("Range", range.as_str()))
            .await
        {
            Ok(r) => r,
            // Range beyond EOF – an empty result, not a failure.
            Err(CloudError::Api(e)) if e.status == 416 => {
                return Ok(MediaChunk { bytes: Bytes::new(), start, total: None });
            }
            Err(e) => return Err(e),
        };
        let status = res.status();
        let cr = res
            .headers()
            .get("content-range")
            .and_then(|h| h.to_str().ok())
            .and_then(parse_content_range);
        let content_len = res.content_length();
        let bytes = res.bytes().await.map_err(|e| CloudError::Network(e.to_string()))?;
        let (actual_start, total) = if status == StatusCode::PARTIAL_CONTENT {
            match cr {
                Some((s, _, t)) => (s, t),
                None => (start, None),
            }
        } else {
            // Server ignored Range and sent everything.
            (0, content_len.or(Some(bytes.len() as u64)))
        };
        // If the full body came back, slice out what was asked for.
        let bytes = if actual_start < start {
            let skip = (start - actual_start) as usize;
            let b = if skip >= bytes.len() { Bytes::new() } else { bytes.slice(skip..) };
            match end {
                Some(e) => {
                    let want = (e - start + 1) as usize;
                    if b.len() > want { b.slice(..want) } else { b }
                }
                None => b,
            }
        } else {
            bytes
        };
        Ok(MediaChunk { bytes, start, total })
    }

    pub async fn read_content_range(
        &self,
        account: &str,
        file_id: &str,
        offset: u64,
        length: u64,
    ) -> Result<Bytes, CloudError> {
        if length == 0 {
            return Ok(Bytes::new());
        }
        Ok(self.fetch_range(account, file_id, offset, Some(offset + length - 1)).await?.bytes)
    }

    /// Streaming download of a binary file.
    pub async fn open_media(&self, account: &str, file_id: &str) -> Result<Response, CloudError> {
        let url = self.url(&format!("/files/{file_id}"));
        self.call(account, true, |c| c.get(&url).query(&[("alt", "media")])).await
    }

    pub async fn open_export(&self, account: &str, file_id: &str, mime: &str) -> Result<Response, CloudError> {
        let url = self.url(&format!("/files/{file_id}/export"));
        self.call(account, true, |c| c.get(&url).query(&[("mimeType", mime)])).await
    }

    pub async fn export_workspace_file(
        &self,
        account: &str,
        file_id: &str,
        export_mime_type: &str,
    ) -> Result<Bytes, CloudError> {
        let res = self.open_export(account, file_id, export_mime_type).await?;
        res.bytes().await.map_err(|e| CloudError::Network(e.to_string()))
    }

    /// Download a thumbnail. Thumbnail links are short-lived and must be
    /// fetched with credentials; the caller refreshes the link on failure.
    pub async fn fetch_thumbnail(&self, account: &str, link: &str, size: u32) -> Result<(Bytes, String), CloudError> {
        // Links end in `=s220`; request the size we want instead.
        let link = match link.rfind("=s") {
            Some(i) if link[i + 2..].chars().all(|c| c.is_ascii_digit()) => format!("{}=s{size}", &link[..i]),
            _ => link.to_string(),
        };
        let token = self.auth.get_access_token(account).await?;
        let res = self
            .client
            .get(&link)
            .bearer_auth(&token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| CloudError::Network(e.without_url().to_string()))?;
        if !res.status().is_success() {
            let code = res.status().as_u16();
            return Err(CloudError::Api(ApiError::new(code, "thumbnail unavailable")));
        }
        let ctype = res
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = res.bytes().await.map_err(|e| CloudError::Network(e.to_string()))?;
        Ok((bytes, ctype))
    }

    // -------------------------------------------------------------- upload

    pub async fn initiate_resumable_upload(
        &self,
        account: &str,
        name: &str,
        parent_id: &str,
        mime_type: &str,
        size: u64,
    ) -> Result<String, CloudError> {
        let url = format!("{}/files", self.ep.upload_base);
        let body = serde_json::json!({ "name": name, "parents": [parent_id] });
        let res = self
            .call(account, false, |c| {
                c.post(&url)
                    .query(&[("uploadType", "resumable"), ("fields", FILE_FIELDS)])
                    .header("X-Upload-Content-Type", mime_type)
                    .header("X-Upload-Content-Length", size.to_string())
                    .json(&body)
            })
            .await?;
        res.headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| CloudError::Api(ApiError::new(502, "Drive did not return an upload session URL")))
    }

    async fn interpret_upload_response(res: Response) -> Result<UploadStatus, CloudError> {
        let status = res.status();
        if status.as_u16() == 308 {
            let confirmed = res
                .headers()
                .get("range")
                .and_then(|h| h.to_str().ok())
                .and_then(|r| r.rsplit('-').next())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|n| n + 1)
                .unwrap_or(0);
            return Ok(UploadStatus::Incomplete(confirmed));
        }
        if status.is_success() {
            let v: Value = res.json().await.map_err(|e| CloudError::Network(e.to_string()))?;
            let file: DriveFile = serde_json::from_value(v)
                .map_err(|e| CloudError::Api(ApiError::new(200, format!("unreadable upload result: {e}"))))?;
            return Ok(UploadStatus::Complete(file));
        }
        if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
            return Ok(UploadStatus::SessionExpired);
        }
        let code = status.as_u16();
        let body = res.text().await.unwrap_or_default();
        Err(CloudError::Api(api_error(code, &body)))
    }

    pub async fn upload_chunk(
        &self,
        session_uri: &str,
        data: Vec<u8>,
        offset: u64,
        total: u64,
    ) -> Result<UploadStatus, CloudError> {
        let length = data.len() as u64;
        let range = if length == 0 {
            format!("bytes */{total}")
        } else {
            format!("bytes {}-{}/{}", offset, offset + length - 1, total)
        };
        let res = self
            .client
            .put(session_uri)
            .header("Content-Range", range)
            .timeout(Duration::from_secs(300))
            .body(data)
            .send()
            .await
            .map_err(|e| CloudError::Network(e.without_url().to_string()))?;
        Self::interpret_upload_response(res).await
    }

    pub async fn check_upload_status(&self, session_uri: &str, total: u64) -> Result<UploadStatus, CloudError> {
        let res = self
            .client
            .put(session_uri)
            .header("Content-Range", format!("bytes */{total}"))
            .header("Content-Length", "0")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| CloudError::Network(e.without_url().to_string()))?;
        Self::interpret_upload_response(res).await
    }

    // ----------------------------------------------------------- mutations

    pub async fn create_folder(&self, account: &str, name: &str, parent_id: &str) -> Result<DriveFile, CloudError> {
        let url = self.url("/files");
        let body = serde_json::json!({ "name": name, "mimeType": FOLDER_MIME, "parents": [parent_id] });
        let v = self.json(account, false, |c| c.post(&url).query(&[("fields", FILE_FIELDS)]).json(&body)).await?;
        serde_json::from_value(v).map_err(|e| CloudError::Network(format!("unreadable folder result: {e}")))
    }

    pub async fn rename_file(&self, account: &str, file_id: &str, new_name: &str) -> Result<DriveFile, CloudError> {
        let url = self.url(&format!("/files/{file_id}"));
        let body = serde_json::json!({ "name": new_name });
        let v = self.json(account, true, |c| c.patch(&url).query(&[("fields", FILE_FIELDS)]).json(&body)).await?;
        serde_json::from_value(v).map_err(|e| CloudError::Network(format!("unreadable result: {e}")))
    }

    pub async fn move_file(
        &self,
        account: &str,
        file_id: &str,
        new_parent_id: &str,
        old_parent_id: &str,
    ) -> Result<DriveFile, CloudError> {
        let url = self.url(&format!("/files/{file_id}"));
        let v = self
            .json(account, true, |c| {
                c.patch(&url)
                    .query(&[("addParents", new_parent_id), ("removeParents", old_parent_id), ("fields", FILE_FIELDS)])
                    .json(&serde_json::json!({}))
            })
            .await?;
        serde_json::from_value(v).map_err(|e| CloudError::Network(format!("unreadable result: {e}")))
    }

    async fn set_trashed(&self, account: &str, file_id: &str, trashed: bool) -> Result<(), CloudError> {
        let url = self.url(&format!("/files/{file_id}"));
        let body = serde_json::json!({ "trashed": trashed });
        self.call(account, true, |c| c.patch(&url).query(&[("fields", "id")]).json(&body)).await?;
        Ok(())
    }

    pub async fn trash_file(&self, account: &str, file_id: &str) -> Result<(), CloudError> {
        self.set_trashed(account, file_id, true).await
    }

    pub async fn restore_file(&self, account: &str, file_id: &str) -> Result<(), CloudError> {
        self.set_trashed(account, file_id, false).await
    }

    pub async fn delete_permanently(&self, account: &str, file_id: &str) -> Result<(), CloudError> {
        let url = self.url(&format!("/files/{file_id}"));
        match self.call(account, true, |c| c.delete(&url)).await {
            Ok(_) => Ok(()),
            // Already gone – the goal is reached.
            Err(CloudError::Api(e)) if e.status == 404 => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn empty_trash(&self, account: &str) -> Result<(), CloudError> {
        let url = self.url("/files/trash");
        self.call(account, true, |c| c.delete(&url)).await?;
        Ok(())
    }

    /// Id of the account's real "My Drive" root folder.
    pub async fn get_root_id(&self, account: &str) -> Result<String, CloudError> {
        let url = self.url("/files/root");
        let v = self.json(account, true, |c| c.get(&url).query(&[("fields", "id")])).await?;
        v.get("id")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| CloudError::Api(ApiError::new(502, "Drive returned no root folder id")))
    }

    // ------------------------------------------------------------- changes

    pub async fn get_start_page_token(&self, account: &str) -> Result<String, CloudError> {
        let url = self.url("/changes/startPageToken");
        let v = self.json(account, true, |c| c.get(&url)).await?;
        v.get("startPageToken")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| CloudError::Api(ApiError::new(502, "Drive returned no start page token")))
    }

    pub async fn get_changes_page(&self, account: &str, page_token: &str) -> Result<ChangesPage, CloudError> {
        let url = self.url("/changes");
        let fields = format!("nextPageToken,newStartPageToken,changes(fileId,removed,file({FILE_FIELDS}))");
        let v = self
            .json(account, true, |c| {
                c.get(&url).query(&[
                    ("pageToken", page_token),
                    ("pageSize", "1000"),
                    ("includeRemoved", "true"),
                    ("spaces", "drive"),
                    ("restrictToMyDrive", "true"),
                    ("fields", fields.as_str()),
                ])
            })
            .await?;
        Ok(parse_changes_page(&v))
    }

    // --------------------------------------------------------------- about

    pub async fn get_about(&self, account: &str) -> Result<(Option<String>, Option<StorageQuota>), CloudError> {
        let url = self.url("/about");
        let v = self
            .json(account, true, |c| c.get(&url).query(&[("fields", "user(displayName),storageQuota")]))
            .await?;
        let name = v["user"]["displayName"].as_str().map(str::to_string);
        Ok((name, parse_quota(&v)))
    }

    pub async fn get_quota(&self, account: &str) -> Result<StorageQuota, CloudError> {
        self.get_about(account)
            .await?
            .1
            .ok_or_else(|| CloudError::Api(ApiError::new(502, "Drive did not report storage quota")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn files_page_parses_string_sizes_and_counts_skips() {
        let v = json!({
            "nextPageToken": "NEXT",
            "files": [
                {"id":"a","name":"x.jpg","mimeType":"image/jpeg","size":"123","parents":["r"]},
                {"id":"f","name":"Dir","mimeType":"application/vnd.google-apps.folder"},
                {"name":"no id"}
            ]
        });
        let (files, next, skipped) = parse_files_page(&v);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].size, Some(123));
        assert!(files[1].is_folder);
        assert_eq!(skipped, 1);
        assert_eq!(next.as_deref(), Some("NEXT"));
    }

    #[test]
    fn changes_page_handles_removed_and_tokens() {
        let v = json!({
            "newStartPageToken": "T2",
            "changes": [
                {"fileId":"a","removed":true},
                {"fileId":"b","removed":false,"file":{"id":"b","name":"n","mimeType":"text/plain","size":"5"}}
            ]
        });
        let p = parse_changes_page(&v);
        assert_eq!(p.changes.len(), 2);
        assert!(p.changes[0].removed && p.changes[0].file.is_none());
        assert_eq!(p.changes[1].file.as_ref().unwrap().size, Some(5));
        assert_eq!(p.new_start_page_token.as_deref(), Some("T2"));
        assert!(p.next_page_token.is_none());
    }

    #[test]
    fn quota_strings_and_unlimited() {
        let q = parse_quota(&json!({"storageQuota":{"limit":"16106127360","usage":"1000","usageInDrive":"400"}})).unwrap();
        assert_eq!((q.limit, q.usage, q.usage_in_drive), (16106127360, 1000, 400));
        // Workspace accounts may omit `limit` (unlimited).
        let q = parse_quota(&json!({"storageQuota":{"usage":"5"}})).unwrap();
        assert_eq!(q.limit, 0);
        assert_eq!(q.free(), u64::MAX);
    }

    #[test]
    fn error_body_reason_is_extracted() {
        let e = api_error(
            403,
            r#"{"error":{"code":403,"message":"Rate Limit Exceeded","errors":[{"reason":"userRateLimitExceeded"}]}}"#,
        );
        assert!(e.is_rate_limit());
        let e = api_error(403, r#"{"error":{"message":"The user's Drive storage quota has been exceeded.","errors":[{"reason":"storageQuotaExceeded"}]}}"#);
        assert!(e.is_storage_full());
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 0-99/1000"), Some((0, 99, Some(1000))));
        assert_eq!(parse_content_range("bytes */1000"), Some((0, 0, Some(1000))));
        assert_eq!(parse_content_range("garbage"), None);
    }
}
