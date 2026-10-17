//! The façade used by every front-end (GUI, FUSE): accounts, tree, content,
//! uploads/downloads, trash and settings.

use crate::api::DriveClient;
use crate::auth::OAuthManager;
use crate::cache::MetadataCache;
use crate::config::{paths, AppConfig};
use crate::content_cache::ContentCache;
use crate::endpoints::Endpoints;
use crate::error::CloudError;
use crate::router::UploadRouter;
use crate::store::{Backend, SecretStore};
use crate::sync::SyncEngine;
use crate::tree::VirtualTree;
use crate::types::*;
use crate::upload::{TransferManager, UploadCtx};
use bytes::Bytes;
use futures_util::{future::join_all, StreamExt};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, Semaphore};
use tokio_util::sync::CancellationToken;

/// Files up to this size are fetched whole and kept in the disk cache.
const SMALL_FILE: u64 = 16 * 1024 * 1024;
/// Largest slice returned for a single ranged media request.
const MAX_RANGE: u64 = 8 * 1024 * 1024;
const THUMB_CACHE_BYTES: u64 = 512 * 1024 * 1024;

pub struct EngineDirs {
    pub data: PathBuf,
    pub cache: PathBuf,
}

impl Default for EngineDirs {
    fn default() -> Self {
        Self { data: paths::data_dir(), cache: paths::cache_dir() }
    }
}

/// Result of [`CoreEngine::fetch_media`].
pub struct MediaResponse {
    pub bytes: Bytes,
    pub start: u64,
    pub total: u64,
    pub mime: String,
    /// True when `bytes` is only a slice of the file.
    pub partial: bool,
}

/// Coalesces many "tree changed" signals (e.g. a batch of finished uploads)
/// into one rebuild.
struct Rebuilder {
    pending: AtomicBool,
    sync: SyncEngine,
}

impl Rebuilder {
    fn schedule(self: &Arc<Self>) {
        if self.pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            me.pending.store(false, Ordering::SeqCst);
            let _ = me.sync.rebuild_tree().await;
        });
    }
}

type AboutEntry = (Instant, Option<String>, Option<StorageQuota>);

pub struct CoreEngine {
    config: RwLock<AppConfig>,
    auth: Arc<OAuthManager>,
    api: Arc<DriveClient>,
    cache: Arc<MetadataCache>,
    content_cache: Arc<ContentCache>,
    thumb_cache: Arc<ContentCache>,
    tree: Arc<VirtualTree>,
    sync: SyncEngine,
    transfers: Arc<TransferManager>,
    router: Arc<UploadRouter>,
    accounts: Arc<RwLock<Vec<String>>>,
    events: broadcast::Sender<EngineEvent>,
    rebuilder: Arc<Rebuilder>,
    handle: Mutex<Option<Handle>>,
    pending_auth: Mutex<Option<CancellationToken>>,
    about_cache: RwLock<HashMap<String, AboutEntry>>,
    root_ids: RwLock<HashMap<String, String>>,
    path_lock: tokio::sync::Mutex<()>,
    thumb_sem: Arc<Semaphore>,
    dl_sem: Arc<Semaphore>,
}

fn is_pool(n: &VirtualNode) -> bool {
    n.id == "pool" || n.id.starts_with("pool_")
}

fn valid_name(name: &str) -> Result<String, CloudError> {
    let n = name.trim();
    if n.is_empty() || n.contains('/') || n == "." || n == ".." {
        return Err(CloudError::Config("Names can't be empty or contain \"/\"".into()));
    }
    Ok(n.to_string())
}

fn safe_local_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect();
    if s.is_empty() || s == "." || s == ".." { "_".into() } else { s }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..10_000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or(first)
}

fn sniff_image_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if b.starts_with(b"GIF8") {
        Some("image/gif")
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

impl CoreEngine {
    pub fn new(config: AppConfig) -> Result<Self, CloudError> {
        let store = Arc::new(SecretStore::open());
        Self::with_parts(config, Endpoints::default(), store, EngineDirs::default())
    }

    /// Fully explicit constructor (used by tests and embedders).
    pub fn with_parts(
        config: AppConfig,
        endpoints: Endpoints,
        store: Arc<SecretStore>,
        dirs: EngineDirs,
    ) -> Result<Self, CloudError> {
        let config = config.sanitized();
        crate::logging::init_logging(&config.log_level);

        let (events, _) = broadcast::channel(1024);
        let auth = Arc::new(OAuthManager::new(endpoints.clone(), store.clone()));
        let api = Arc::new(DriveClient::new(auth.clone(), endpoints));
        let cache = Arc::new(MetadataCache::new(dirs.data.join("metadata.db"))?);
        let content_cache = Arc::new(ContentCache::new(dirs.cache.join("content"), config.cache_size_bytes)?);
        let thumb_cache = Arc::new(ContentCache::new(dirs.cache.join("thumbs"), THUMB_CACHE_BYTES)?);
        let tree = Arc::new(VirtualTree::new());
        let router = Arc::new(UploadRouter::new(config.upload_policy.clone()));
        let accounts = Arc::new(RwLock::new(auth.list_accounts()));
        let sync = SyncEngine::new(
            api.clone(),
            cache.clone(),
            tree.clone(),
            accounts.clone(),
            events.clone(),
            config.sync_interval_secs,
        );
        // Whatever was indexed in a previous session is browsable immediately.
        if let Err(e) = sync.rebuild_tree_blocking() {
            tracing::warn!("Could not build the tree from the cache: {e}");
        }
        let transfers = Arc::new(TransferManager::new(events.clone()));
        let rebuilder = Arc::new(Rebuilder { pending: AtomicBool::new(false), sync: sync.clone() });

        Ok(Self {
            config: RwLock::new(config),
            auth,
            api,
            cache,
            content_cache,
            thumb_cache,
            tree,
            sync,
            transfers,
            router,
            accounts,
            events,
            rebuilder,
            handle: Mutex::new(Handle::try_current().ok()),
            pending_auth: Mutex::new(None),
            about_cache: RwLock::new(HashMap::new()),
            root_ids: RwLock::new(HashMap::new()),
            path_lock: tokio::sync::Mutex::new(()),
            thumb_sem: Arc::new(Semaphore::new(6)),
            dl_sem: Arc::new(Semaphore::new(3)),
        })
    }

    /// Tell the engine which Tokio runtime runs its background work.
    pub fn set_runtime(&self, handle: Handle) {
        *self.handle.lock() = Some(handle);
    }

    fn runtime(&self) -> Result<Handle, CloudError> {
        if let Some(h) = self.handle.lock().clone() {
            return Ok(h);
        }
        Handle::try_current().map_err(|_| CloudError::Config("No Tokio runtime available for background work".into()))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    // ================================================================ accounts

    pub fn list_accounts(&self) -> Vec<String> {
        self.accounts.read().clone()
    }

    pub fn has_client_credentials(&self) -> bool {
        self.auth.has_client_credentials()
    }

    pub fn set_client_credentials(&self, id: &str, secret: &str) -> Result<(), CloudError> {
        self.auth.set_client_credentials(id, secret)
    }

    /// Masked Client ID for display ("1234567890-ab…").
    pub fn client_id_hint(&self) -> Option<String> {
        let (id, _) = self.auth.client_credentials().ok()?;
        Some(format!("{}…", id.chars().take(14).collect::<String>()))
    }

    pub fn storage_backend(&self) -> Backend {
        self.auth.store().backend()
    }

    /// Link a Google account. `on_url` receives the consent URL (so the UI
    /// can show/copy it) before the browser is opened. Resolves when the
    /// user finished signing in – or fails/cancels/times out.
    pub async fn add_account(
        &self,
        email_hint: Option<&str>,
        on_url: impl FnOnce(&str),
    ) -> Result<AddAccountResult, CloudError> {
        self.run_sign_in(email_hint, false, on_url).await
    }

    /// Re-authorise an existing account (must sign in as the same address).
    pub async fn reconnect_account(
        &self,
        email: &str,
        on_url: impl FnOnce(&str),
    ) -> Result<AddAccountResult, CloudError> {
        self.run_sign_in(Some(email), true, on_url).await
    }

    async fn run_sign_in(
        &self,
        hint: Option<&str>,
        strict: bool,
        on_url: impl FnOnce(&str),
    ) -> Result<AddAccountResult, CloudError> {
        let pending = self.auth.begin_auth(hint, strict).await?;
        let cancel = CancellationToken::new();
        if let Some(prev) = self.pending_auth.lock().replace(cancel.clone()) {
            prev.cancel(); // a newer sign-in supersedes an older one
        }
        on_url(&pending.url);
        if std::env::var_os("CLOUDY_NO_BROWSER").is_none() {
            if let Err(e) = open::that_detached(&pending.url) {
                tracing::warn!("Could not open the browser automatically: {e}");
            }
        }
        let result = self.auth.complete_auth(pending, cancel.clone()).await;
        {
            let mut slot = self.pending_auth.lock();
            if slot.as_ref().map_or(false, |t| t.is_cancelled() == cancel.is_cancelled()) {
                *slot = None;
            }
        }
        let outcome = result?;

        {
            let mut accts = self.accounts.write();
            if !accts.contains(&outcome.email) {
                accts.push(outcome.email.clone());
                accts.sort();
            }
        }
        self.router.invalidate_quota_cache();
        self.about_cache.write().remove(&outcome.email);
        if let Ok(h) = self.runtime() {
            self.sync.start_account(&h, &outcome.email);
        }
        self.sync.kick(&outcome.email); // resume if it was waiting for a login
        let _ = self.events.send(EngineEvent::AccountsChanged);
        let _ = self.sync.rebuild_tree().await;
        Ok(AddAccountResult {
            email: outcome.email,
            display_name: outcome.display_name,
            already_linked: outcome.already_linked,
        })
    }

    /// Abort a sign-in that is waiting for the browser.
    pub fn cancel_add_account(&self) {
        if let Some(t) = self.pending_auth.lock().take() {
            t.cancel();
        }
    }

    pub fn remove_account(&self, email: &str) -> Result<(), CloudError> {
        self.sync.stop_account(email);
        self.transfers.cancel_account(email);
        self.auth.remove_account(email);
        self.cache.clear_account(email)?;
        self.accounts.write().retain(|e| e != email);
        self.router.forget_account(email);
        self.about_cache.write().remove(email);
        self.root_ids.write().remove(email);
        self.sync.rebuild_tree_blocking()?;
        let _ = self.events.send(EngineEvent::AccountsChanged);
        Ok(())
    }

    /// Everything the UI needs to draw the account list.
    pub async fn account_summaries(&self, force: bool) -> Vec<AccountSummary> {
        let accounts = self.list_accounts();
        join_all(accounts.iter().map(|a| self.summary_for(a, force))).await
    }

    /// Account list built purely from local state (no network) so a UI can
    /// paint immediately; quotas appear once [`account_summaries`] has run.
    pub fn account_summaries_local(&self) -> Vec<AccountSummary> {
        self.list_accounts()
            .into_iter()
            .map(|email| {
                let (display_name, quota) = self
                    .about_cache
                    .read()
                    .get(&email)
                    .map(|(_, n, q)| (n.clone(), q.clone()))
                    .unwrap_or((None, None));
                let sync = if self.auth.needs_reauth(&email) { SyncStatus::NeedsReauth } else { self.sync.status(&email) };
                let files_indexed = self.cache.get_file_count_for_account(&email).unwrap_or(0) as u64;
                AccountSummary { email, display_name, quota, sync, files_indexed, error: None }
            })
            .collect()
    }

    async fn summary_for(&self, email: &str, force: bool) -> AccountSummary {
        let files_indexed = self.cache.get_file_count_for_account(email).unwrap_or(0) as u64;
        let mut sync = self.sync.status(email);
        let mut error = None;
        let mut name = None;
        let mut quota = None;

        let cached = self.about_cache.read().get(email).cloned();
        let fresh = cached.as_ref().filter(|(t, _, _)| !force && t.elapsed() < Duration::from_secs(30));
        if let Some((_, n, q)) = fresh {
            name = n.clone();
            quota = q.clone();
        } else if self.auth.needs_reauth(email) {
            sync = SyncStatus::NeedsReauth;
            if let Some((_, n, q)) = &cached {
                name = n.clone();
                quota = q.clone();
            }
        } else {
            match tokio::time::timeout(Duration::from_secs(10), self.api.get_about(email)).await {
                Ok(Ok((n, q))) => {
                    self.about_cache.write().insert(email.to_string(), (Instant::now(), n.clone(), q.clone()));
                    name = n;
                    quota = q;
                }
                Ok(Err(e)) => {
                    if e.needs_reauth() {
                        sync = SyncStatus::NeedsReauth;
                    } else {
                        error = Some(match &e {
                            CloudError::Network(_) => {
                                "Can't reach Google right now. Cloudy keeps retrying and shows your last known data.".to_string()
                            }
                            other => other.to_string(),
                        });
                    }
                    if let Some((_, n, q)) = &cached {
                        name = n.clone();
                        quota = q.clone();
                    }
                }
                Err(_) => error = Some("Google did not respond in time. Cloudy will keep trying.".into()),
            }
        }
        AccountSummary { email: email.to_string(), display_name: name, quota, sync, files_indexed, error }
    }

    pub async fn get_quota(&self, account: &str) -> Result<StorageQuota, CloudError> {
        self.router.quota(account, &self.api).await
    }

    /// Quotas of all reachable accounts (accounts that fail are omitted).
    pub async fn get_all_quotas(&self) -> Result<HashMap<String, StorageQuota>, CloudError> {
        let accounts = self.list_accounts();
        let results = join_all(accounts.iter().map(|a| async move { (a.clone(), self.get_quota(a).await) })).await;
        Ok(results.into_iter().filter_map(|(a, r)| r.ok().map(|q| (a, q))).collect())
    }

    // ================================================================= browse

    pub fn list_children(&self, parent_id: &str) -> Vec<VirtualNode> {
        self.tree.get_children(parent_id)
    }
    pub fn get_node(&self, id: &str) -> Option<VirtualNode> {
        self.tree.get_node(id)
    }
    pub fn get_children(&self, node_id: &str) -> Vec<VirtualNode> {
        self.tree.get_children(node_id)
    }
    pub fn lookup_child(&self, parent_id: &str, name: &str) -> Option<VirtualNode> {
        self.tree.lookup_child(parent_id, name)
    }
    pub fn get_ancestors(&self, node_id: &str) -> Vec<VirtualNode> {
        self.tree.get_ancestors(node_id)
    }
    pub fn node_path(&self, node_id: &str) -> String {
        self.tree.get_node_path(node_id)
    }

    fn to_nodes(&self, rows: Vec<(DriveFile, String)>) -> Vec<VirtualNode> {
        rows.into_iter()
            .filter_map(|(f, account)| {
                self.tree.get_node(&format!("pool_{}", f.id)).or_else(|| self.tree.get_node(&format!("{account}_{}", f.id)))
            })
            .collect()
    }

    pub fn search(&self, query: &str) -> Result<Vec<VirtualNode>, CloudError> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        Ok(self.to_nodes(self.cache.search(q, 300)?))
    }

    pub fn list_media_files(&self) -> Result<Vec<VirtualNode>, CloudError> {
        Ok(self.to_nodes(self.cache.get_all_media_files()?))
    }

    pub fn list_media_page(&self, offset: i64, limit: i64) -> Result<Vec<VirtualNode>, CloudError> {
        Ok(self.to_nodes(self.cache.get_media_files(limit, offset)?))
    }

    // ============================================================ sync control

    pub fn start_sync(&self) -> Result<(), CloudError> {
        let h = self.runtime()?;
        for a in self.list_accounts() {
            self.sync.start_account(&h, &a);
        }
        Ok(())
    }

    pub fn stop_sync(&self) {
        self.sync.stop_all();
    }

    /// Ask the background task to sync an account right now.
    pub async fn force_sync(&self, account: &str) -> Result<(), CloudError> {
        if !self.list_accounts().iter().any(|a| a == account) {
            return Err(CloudError::NotFound(account.into()));
        }
        if let Ok(h) = self.runtime() {
            self.sync.start_account(&h, account);
        }
        self.sync.kick(account);
        Ok(())
    }

    pub fn sync_all_now(&self) {
        self.sync.kick_all();
    }

    pub fn get_sync_status(&self, account: &str) -> SyncStatus {
        if self.auth.needs_reauth(account) {
            return SyncStatus::NeedsReauth;
        }
        self.sync.status(account)
    }

    // ================================================================= content

    fn source_of(&self, node: &VirtualNode) -> Result<(String, String), CloudError> {
        node.all_sources()
            .into_iter()
            .next()
            .map(|s| (s.account, s.drive_file_id))
            .ok_or_else(|| CloudError::NotFound(node.id.clone()))
    }

    fn row(&self, account: &str, drive_id: &str) -> Result<DriveFile, CloudError> {
        self.cache.get_file(drive_id, account)?.ok_or_else(|| CloudError::NotFound(drive_id.into()))
    }

    fn node(&self, id: &str) -> Result<VirtualNode, CloudError> {
        self.tree.get_node(id).ok_or_else(|| CloudError::NotFound(id.into()))
    }

    /// Whole Google-Docs-style file exported to Office format, cached on disk.
    async fn export_cached(&self, account: &str, file: &DriveFile) -> Result<(PathBuf, String), CloudError> {
        let (mime, _) = workspace_export_mime_and_ext(&file.mime_type)
            .ok_or_else(|| CloudError::Config("This Google file type cannot be exported".into()))?;
        let key = format!("export:{account}:{}", file.id);
        if let Some(p) = self.content_cache.get(&key, None, &file.modified_time) {
            return Ok((p, mime.to_string()));
        }
        let bytes = self.api.export_workspace_file(account, &file.id, mime).await?;
        let p = self.content_cache.put(&key, &bytes, "", &file.modified_time)?;
        Ok((p, mime.to_string()))
    }

    async fn full_cached(&self, account: &str, file: &DriveFile) -> Result<PathBuf, CloudError> {
        let key = format!("file:{account}:{}", file.id);
        if let Some(p) = self.content_cache.get(&key, None, &file.modified_time) {
            return Ok(p);
        }
        let chunk = self.api.fetch_range(account, &file.id, 0, None).await?;
        let md5 = file.md5_checksum.clone().unwrap_or_default();
        self.content_cache.put(&key, &chunk.bytes, &md5, &file.modified_time)
    }

    fn slice_file(path: &Path, offset: u64, length: u64) -> Result<Bytes, CloudError> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(path)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        f.take(length).read_to_end(&mut buf)?;
        Ok(Bytes::from(buf))
    }

    /// Byte-range read (used by FUSE).
    pub async fn read_content(&self, node_id: &str, offset: u64, length: u64) -> Result<Bytes, CloudError> {
        let node = self.node(node_id)?;
        if node.is_dir {
            return Err(CloudError::Config("Cannot read a folder".into()));
        }
        let (account, drive_id) = self.source_of(&node)?;
        let file = self.row(&account, &drive_id)?;
        if node.is_workspace_file {
            let (p, _) = self.export_cached(&account, &file).await?;
            return Self::slice_file(&p, offset, length);
        }
        if length == 0 {
            return Ok(Bytes::new());
        }
        if file.size.unwrap_or(0) <= SMALL_FILE {
            let p = self.full_cached(&account, &file).await?;
            return Self::slice_file(&p, offset, length);
        }
        self.api.read_content_range(&account, &drive_id, offset, length).await
    }

    /// Serve a file (or a byte range of it) to the GUI's media protocol.
    pub async fn fetch_media(
        &self,
        node_id: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<MediaResponse, CloudError> {
        let node = self.node(node_id)?;
        if node.is_dir {
            return Err(CloudError::Config("Cannot preview a folder".into()));
        }
        let (account, drive_id) = self.source_of(&node)?;
        let file = self.row(&account, &drive_id)?;

        if node.is_workspace_file {
            let (p, mime) = self.export_cached(&account, &file).await?;
            let total = std::fs::metadata(&p)?.len();
            let (start, end) = Self::resolve_range(range, total, total.max(1));
            let bytes = Self::slice_file(&p, start, end.saturating_sub(start) + 1)?;
            return Ok(MediaResponse { bytes, start, total, mime, partial: range.is_some() });
        }

        let total = file.size.unwrap_or(0);
        let mime = file.mime_type.clone();
        if total == 0 {
            return Ok(MediaResponse { bytes: Bytes::new(), start: 0, total: 0, mime, partial: false });
        }
        let key = format!("file:{account}:{drive_id}");
        let cached = self.content_cache.get(&key, None, &file.modified_time);

        // Whole-file response for modest sizes (images, small clips).
        if range.is_none() && total <= SMALL_FILE * 2 {
            let p = match cached {
                Some(p) => p,
                None => self.full_cached(&account, &file).await?,
            };
            let bytes = Self::slice_file(&p, 0, total)?;
            return Ok(MediaResponse { bytes, start: 0, total, mime, partial: false });
        }

        let (start, end) = Self::resolve_range(range, total, MAX_RANGE);
        if start >= total {
            return Ok(MediaResponse { bytes: Bytes::new(), start, total, mime, partial: true });
        }
        let bytes = match cached {
            Some(p) => Self::slice_file(&p, start, end - start + 1)?,
            None => self.api.fetch_range(&account, &drive_id, start, Some(end)).await?.bytes,
        };
        Ok(MediaResponse { bytes, start, total, mime, partial: true })
    }

    /// Clamp a requested range to `[0,total)` and at most `max` bytes.
    fn resolve_range(range: Option<(u64, Option<u64>)>, total: u64, max: u64) -> (u64, u64) {
        let last = total.saturating_sub(1);
        match range {
            None => (0, last.min(max.saturating_sub(1))),
            Some((s, e)) => {
                let end = e.unwrap_or(u64::MAX).min(last).min(s.saturating_add(max).saturating_sub(1));
                (s, end)
            }
        }
    }

    /// Thumbnail bytes for the gallery (disk-cached, link refreshed when stale).
    pub async fn thumbnail(&self, node_id: &str, size: u32) -> Result<(Bytes, String), CloudError> {
        let size = size.clamp(64, 1600);
        let node = self.node(node_id)?;
        let (account, drive_id) = self.source_of(&node)?;
        let mut file = self.row(&account, &drive_id)?;
        let key = format!("thumb:{account}:{drive_id}:{size}");
        if let Some(b) = self.thumb_cache.get_bytes(&key, &file.modified_time) {
            let mime = sniff_image_mime(&b).unwrap_or("image/jpeg").to_string();
            return Ok((Bytes::from(b), mime));
        }

        let _permit = self.thumb_sem.acquire().await.map_err(|_| CloudError::Cancelled)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            if let Some(link) = file.thumbnail_link.clone() {
                match self.api.fetch_thumbnail(&account, &link, size).await {
                    Ok((bytes, ctype)) => {
                        let _ = self.thumb_cache.put(&key, &bytes, "", &file.modified_time);
                        let mime = sniff_image_mime(&bytes).map(str::to_string).unwrap_or(ctype);
                        return Ok((bytes, mime));
                    }
                    Err(CloudError::Api(e)) if attempt == 1 && matches!(e.status, 401 | 403 | 404 | 410) => {
                        // Thumbnail links expire after a while: ask Drive for a fresh one.
                        let fresh = self.api.get_file_metadata(&account, &drive_id).await?;
                        let _ = self.cache.upsert_file(&fresh, &account);
                        file = fresh;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            break;
        }
        // No Drive thumbnail: fall back to the original for small plain images.
        let plain = matches!(file.mime_type.as_str(), "image/jpeg" | "image/png" | "image/gif" | "image/webp");
        if plain && file.size.unwrap_or(u64::MAX) <= 8 * 1024 * 1024 {
            let chunk = self.api.fetch_range(&account, &drive_id, 0, None).await?;
            let _ = self.thumb_cache.put(&key, &chunk.bytes, "", &file.modified_time);
            return Ok((chunk.bytes, file.mime_type));
        }
        Err(CloudError::NotFound("no thumbnail available".into()))
    }

    // ================================================================ uploads

    fn upload_ctx(&self) -> UploadCtx {
        let cache = self.cache.clone();
        let rebuilder = self.rebuilder.clone();
        UploadCtx {
            api: self.api.clone(),
            cache: self.cache.clone(),
            router: self.router.clone(),
            on_uploaded: Arc::new(move |account, file| {
                let _ = cache.upsert_file(file, account);
                rebuilder.schedule();
            }),
        }
    }

    pub fn get_transfers(&self) -> Vec<TransferInfo> {
        self.transfers.get_transfers()
    }
    pub fn cancel_transfer(&self, id: &str) -> Result<(), CloudError> {
        self.transfers.cancel(id);
        Ok(())
    }
    pub fn clear_finished_transfers(&self) {
        self.transfers.clear_finished();
    }
    pub fn retry_transfer(&self, id: &str) -> bool {
        self.transfers.retry(id, self.upload_ctx())
    }
    pub async fn resume_pending_uploads(&self) -> Result<Vec<String>, CloudError> {
        self.transfers.resume_pending(self.upload_ctx()).await
    }

    async fn root_id(&self, account: &str) -> Result<String, CloudError> {
        if let Some(id) = self.root_ids.read().get(account) {
            return Ok(id.clone());
        }
        let id = self.api.get_root_id(account).await?;
        self.root_ids.write().insert(account.to_string(), id.clone());
        Ok(id)
    }

    fn find_child_folder(&self, account: &str, parent: Option<&str>, name: &str) -> Result<Option<String>, CloudError> {
        let list = match parent {
            Some(p) => self.cache.get_children(p, account)?,
            None => self.cache.get_all_root_files(account)?,
        };
        Ok(list.into_iter().find(|f| f.is_folder && f.name == name).map(|f| f.id))
    }

    /// Make sure `names` exist as nested folders below `base` (None = Drive root)
    /// in `account`, creating what's missing. Returns the innermost folder's id.
    async fn ensure_folders(
        &self,
        account: &str,
        base: Option<String>,
        names: &[String],
    ) -> Result<Option<String>, CloudError> {
        if names.is_empty() {
            return Ok(base);
        }
        let _guard = self.path_lock.lock().await; // two uploads must not both create "Photos"
        let mut cur = base;
        for name in names {
            cur = match self.find_child_folder(account, cur.as_deref(), name)? {
                Some(id) => Some(id),
                None => {
                    let parent = match &cur {
                        Some(p) => p.clone(),
                        None => self.root_id(account).await?,
                    };
                    let f = self.api.create_folder(account, name, &parent).await?;
                    self.cache.upsert_file(&f, account)?;
                    Some(f.id)
                }
            };
        }
        Ok(cur)
    }

    /// Folder names from the pool root down to `node`.
    fn pool_path(&self, node: &VirtualNode) -> Vec<String> {
        let chain = self.tree.get_ancestors(&node.id);
        let after_pool: Vec<String> = chain.iter().skip_while(|n| n.id != "pool").skip(1).map(|n| n.name.clone()).collect();
        after_pool
    }

    /// Decide where a new item goes: (account, remote parent id).
    /// Reserves `size` bytes in the router; the caller must `release` them
    /// (the upload manager does so when a transfer ends).
    async fn resolve_target(
        &self,
        parent: &VirtualNode,
        subpath: &[String],
        size: u64,
    ) -> Result<(String, Option<String>), CloudError> {
        if is_pool(parent) {
            let accounts = self.list_accounts();
            let account = self.router.select_account(&accounts, size, &self.api).await?;
            let res = async {
                let base = match parent.all_sources().into_iter().find(|s| s.account == account) {
                    Some(s) => Some(s.drive_file_id),
                    None => self.ensure_folders(&account, None, &self.pool_path(parent)).await?,
                };
                self.ensure_folders(&account, base, subpath).await
            }
            .await;
            match res {
                Ok(remote) => Ok((account, remote)),
                Err(e) => {
                    self.router.release(&account, size);
                    Err(e)
                }
            }
        } else {
            let account = parent
                .account
                .clone()
                .ok_or_else(|| CloudError::Config("Open the Pool or one of your accounts first".into()))?;
            self.router.reserve(&account, size);
            match self.ensure_folders(&account, parent.drive_file_id.clone(), subpath).await {
                Ok(remote) => Ok((account, remote)),
                Err(e) => {
                    self.router.release(&account, size);
                    Err(e)
                }
            }
        }
    }

    /// Upload files and/or whole folders into `parent_node_id`.
    /// In the pool every file may land in a different account.
    pub async fn upload_paths(&self, paths: Vec<PathBuf>, parent_node_id: &str) -> Result<Vec<String>, CloudError> {
        let parent = self.node(parent_node_id)?;
        if parent.is_read_only && !is_pool(&parent) && parent.drive_file_id.is_none() && parent.account.is_none() {
            return Err(CloudError::Config("Uploads go into the Pool or into an account, not here".into()));
        }
        if parent.id == "accounts" || parent.id == "root" {
            return Err(CloudError::Config("Open the Pool or one of your accounts first".into()));
        }

        let plan = tokio::task::spawn_blocking(move || collect_upload_plan(paths))
            .await
            .map_err(|e| CloudError::Io(e.to_string()))??;

        let mut ids = Vec::new();
        for dir in &plan.empty_dirs {
            let (account, _) = self.resolve_target(&parent, dir, 0).await?;
            self.router.release(&account, 0);
        }
        for item in plan.files {
            let (account, remote) = self.resolve_target(&parent, &item.subpath, item.size).await?;
            let remote = match remote {
                Some(r) => r,
                None => self.root_id(&account).await?,
            };
            let name = item.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let job = TransferManager::new_job(item.path, &remote, &name, &account, item.size);
            ids.push(self.transfers.enqueue(job, self.upload_ctx()));
        }
        self.rebuilder.schedule();
        Ok(ids)
    }

    /// Single-file convenience (kept for older callers).
    pub async fn create_file(&self, local_path: &str, parent_node_id: &str) -> Result<String, CloudError> {
        let ids = self.upload_paths(vec![PathBuf::from(local_path)], parent_node_id).await?;
        ids.into_iter().next().ok_or_else(|| CloudError::Io("nothing to upload".into()))
    }

    // ============================================================== downloads

    /// Download a file or a whole folder into `dest_dir`. Returns transfer ids.
    pub async fn download_node(&self, node_id: &str, dest_dir: PathBuf) -> Result<Vec<String>, CloudError> {
        let root = self.node(node_id)?;
        let handle = self.runtime()?;
        let mut files: Vec<(VirtualNode, PathBuf)> = Vec::new();
        let mut stack: Vec<(VirtualNode, PathBuf)> = vec![(root, dest_dir)];
        while let Some((node, dir)) = stack.pop() {
            if node.is_dir {
                let sub = unique_path(&dir, &safe_local_name(&node.display_name));
                tokio::fs::create_dir_all(&sub).await?;
                for child in self.tree.get_children(&node.id) {
                    stack.push((child, sub.clone()));
                }
            } else {
                let target = unique_path(&dir, &safe_local_name(&node.display_name));
                files.push((node, target));
            }
        }

        let mut ids = Vec::new();
        for (node, target) in files {
            let (account, drive_id) = self.source_of(&node)?;
            let file = self.row(&account, &drive_id)?;
            let id = uuid::Uuid::new_v4().to_string();
            let token = self.transfers.register(TransferInfo {
                id: id.clone(),
                file_name: node.display_name.clone(),
                account: account.clone(),
                progress_bytes: 0,
                total_bytes: node.size,
                status: TransferStatus::Pending,
                direction: TransferDirection::Download,
            });
            let (api, transfers, sem) = (self.api.clone(), self.transfers.clone(), self.dl_sem.clone());
            let tid = id.clone();
            handle.spawn(async move {
                let permit = tokio::select! { p = sem.acquire_owned() => p.ok(), _ = token.cancelled() => None };
                if permit.is_none() {
                    return transfers.set_status(&tid, TransferStatus::Cancelled);
                }
                transfers.set_status(&tid, TransferStatus::InProgress);
                match run_download(&api, &transfers, &tid, &account, &file, node.is_workspace_file, &target, &token).await {
                    Ok(()) => transfers.set_status(&tid, TransferStatus::Completed),
                    Err(CloudError::Cancelled) => transfers.set_status(&tid, TransferStatus::Cancelled),
                    Err(e) => transfers.set_status(&tid, TransferStatus::Failed(e.to_string())),
                }
            });
            ids.push(id);
        }
        Ok(ids)
    }

    // ============================================================== mutations

    async fn refresh_tree(&self) -> Result<(), CloudError> {
        self.sync.rebuild_tree().await
    }

    pub async fn create_folder(&self, name: &str, parent_node_id: &str) -> Result<VirtualNode, CloudError> {
        let name = valid_name(name)?;
        let parent = self.node(parent_node_id)?;
        if parent.id == "accounts" || parent.id == "root" {
            return Err(CloudError::Config("Open the Pool or one of your accounts first".into()));
        }
        let (account, remote) = self.resolve_target(&parent, &[], 0).await?;
        self.router.release(&account, 0);
        let remote = match remote {
            Some(r) => r,
            None => self.root_id(&account).await?,
        };
        let file = self.api.create_folder(&account, &name, &remote).await?;
        self.cache.upsert_file(&file, &account)?;
        self.refresh_tree().await?;
        self.tree
            .lookup_child(parent_node_id, &name)
            .or_else(|| self.tree.get_node(&format!("pool_{}", file.id)))
            .or_else(|| self.tree.get_node(&format!("{account}_{}", file.id)))
            .ok_or_else(|| CloudError::NotFound(name))
    }

    pub async fn rename_file(&self, node_id: &str, new_name: &str) -> Result<(), CloudError> {
        let new_name = valid_name(new_name)?;
        let node = self.node(node_id)?;
        if node.is_read_only || node.is_workspace_file {
            return Err(CloudError::Config("This item cannot be renamed".into()));
        }
        let sources = node.all_sources();
        if sources.is_empty() {
            return Err(CloudError::Config("This item cannot be renamed".into()));
        }
        let mut first_err = None;
        for s in sources {
            match self.api.rename_file(&s.account, &s.drive_file_id, &new_name).await {
                Ok(f) => {
                    self.cache.upsert_file(&f, &s.account)?;
                }
                Err(e) => first_err = first_err.or(Some(e)),
            }
        }
        self.refresh_tree().await?;
        first_err.map_or(Ok(()), Err)
    }

    pub async fn trash_file(&self, node_id: &str) -> Result<(), CloudError> {
        let node = self.node(node_id)?;
        let sources = node.all_sources();
        if sources.is_empty() {
            return Err(CloudError::Config("This item cannot be deleted".into()));
        }
        let mut first_err = None;
        for s in sources {
            match self.api.trash_file(&s.account, &s.drive_file_id).await {
                Ok(()) => self.cache.remove_file(&s.drive_file_id, &s.account)?,
                Err(e) => first_err = first_err.or(Some(e)),
            }
        }
        self.router.invalidate_quota_cache();
        self.refresh_tree().await?;
        first_err.map_or(Ok(()), Err)
    }

    pub async fn delete_permanently(&self, node_id: &str) -> Result<(), CloudError> {
        let node = self.node(node_id)?;
        let sources = node.all_sources();
        if sources.is_empty() {
            return Err(CloudError::Config("This item cannot be deleted".into()));
        }
        let mut first_err = None;
        for s in sources {
            match self.api.delete_permanently(&s.account, &s.drive_file_id).await {
                Ok(()) => self.cache.remove_file(&s.drive_file_id, &s.account)?,
                Err(e) => first_err = first_err.or(Some(e)),
            }
        }
        self.router.invalidate_quota_cache();
        self.about_cache.write().clear();
        self.refresh_tree().await?;
        first_err.map_or(Ok(()), Err)
    }

    /// Move within one account. Moving between accounts would need a copy
    /// (and double the quota), so it is refused with a clear message.
    pub async fn move_file(&self, node_id: &str, new_parent_node_id: &str, _old_parent_node_id: &str) -> Result<(), CloudError> {
        let node = self.node(node_id)?;
        if node.is_read_only {
            return Err(CloudError::Config("This item cannot be moved".into()));
        }
        if node.sources.len() > 1 {
            return Err(CloudError::Config(
                "This folder exists in several accounts – move its contents instead".into(),
            ));
        }
        let (account, drive_id) = self.source_of(&node)?;
        let new_parent = self.node(new_parent_node_id)?;
        if new_parent.id == "accounts" || new_parent.id == "root" {
            return Err(CloudError::Config("Pick a folder in the Pool or in the same account".into()));
        }
        if new_parent.id == node.id || self.tree.get_ancestors(&new_parent.id).iter().any(|a| a.id == node.id) {
            return Err(CloudError::Config("A folder cannot be moved into itself".into()));
        }

        let target: Option<String> = if is_pool(&new_parent) {
            match new_parent.all_sources().into_iter().find(|s| s.account == account) {
                Some(s) => Some(s.drive_file_id),
                None => self.ensure_folders(&account, None, &self.pool_path(&new_parent)).await?,
            }
        } else if new_parent.account.as_deref() == Some(account.as_str()) {
            new_parent.drive_file_id.clone()
        } else {
            return Err(CloudError::Config("Files can't be moved between different Google accounts".into()));
        };
        let target = match target {
            Some(t) => t,
            None => self.root_id(&account).await?,
        };

        let row = self.row(&account, &drive_id)?;
        let old = row.parents.first().cloned().unwrap_or_default();
        if old == target {
            return Ok(());
        }
        let moved = self.api.move_file(&account, &drive_id, &target, &old).await?;
        self.cache.upsert_file(&moved, &account)?;
        self.refresh_tree().await
    }

    // =================================================================== trash

    /// Everything in every account's trash (still counts against quota).
    pub async fn list_trash(&self) -> Result<Vec<TrashItem>, CloudError> {
        let accounts = self.list_accounts();
        let results = join_all(accounts.iter().map(|a| async move { (a.clone(), self.api.list_trashed(a).await) })).await;
        let mut items = Vec::new();
        let mut last_err = None;
        let mut any_ok = false;
        for (account, r) in results {
            match r {
                Ok(files) => {
                    any_ok = true;
                    items.extend(files.into_iter().map(|f| TrashItem {
                        account: account.clone(),
                        id: f.id,
                        name: f.name,
                        size: f.size.unwrap_or(0),
                        mime_type: f.mime_type,
                        modified_time: f.modified_time,
                        is_folder: f.is_folder,
                    }));
                }
                Err(e) => last_err = Some(e),
            }
        }
        if !any_ok {
            if let Some(e) = last_err {
                return Err(e);
            }
        }
        items.sort_by(|a, b| b.size.cmp(&a.size));
        Ok(items)
    }

    pub async fn restore_trashed(&self, account: &str, drive_id: &str) -> Result<(), CloudError> {
        self.api.restore_file(account, drive_id).await?;
        let f = self.api.get_file_metadata(account, drive_id).await?;
        self.cache.upsert_file(&f, account)?;
        self.refresh_tree().await
    }

    pub async fn delete_trashed(&self, account: &str, drive_id: &str) -> Result<(), CloudError> {
        self.api.delete_permanently(account, drive_id).await?;
        self.router.invalidate_quota_cache();
        self.about_cache.write().remove(account);
        Ok(())
    }

    /// Empty the trash of one account, or of all of them.
    pub async fn empty_trash(&self, account: Option<&str>) -> Result<(), CloudError> {
        let targets: Vec<String> = match account {
            Some(a) => vec![a.to_string()],
            None => self.list_accounts(),
        };
        let results = join_all(targets.iter().map(|a| self.api.empty_trash(a))).await;
        self.router.invalidate_quota_cache();
        self.about_cache.write().clear();
        results.into_iter().collect::<Result<Vec<_>, _>>().map(|_| ())
    }

    // ================================================================= config

    pub fn get_config(&self) -> AppConfig {
        self.config.read().clone()
    }

    pub fn update_config(&self, config: AppConfig) -> Result<(), CloudError> {
        let config = config.sanitized();
        config.save()?;
        self.router.set_policy(config.upload_policy.clone());
        self.sync.set_interval(config.sync_interval_secs);
        self.content_cache.set_max_size(config.cache_size_bytes);
        *self.config.write() = config;
        Ok(())
    }

    pub fn clear_cache(&self) -> Result<(), CloudError> {
        self.content_cache.clear()?;
        self.thumb_cache.clear()
    }

    pub fn cache_usage(&self) -> u64 {
        self.content_cache.total_size().unwrap_or(0) + self.thumb_cache.total_size().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------

#[derive(Default)]
struct UploadPlan {
    files: Vec<PlannedFile>,
    empty_dirs: Vec<Vec<String>>,
}

struct PlannedFile {
    path: PathBuf,
    /// Folder names below the chosen destination (for dropped folders).
    subpath: Vec<String>,
    size: u64,
}

fn collect_upload_plan(paths: Vec<PathBuf>) -> Result<UploadPlan, CloudError> {
    let mut plan = UploadPlan::default();
    for p in paths {
        let meta = std::fs::symlink_metadata(&p).map_err(|e| CloudError::Io(format!("{}: {e}", p.display())))?;
        if meta.is_file() {
            plan.files.push(PlannedFile { path: p, subpath: vec![], size: meta.len() });
        } else if meta.is_dir() {
            let root = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Folder".into());
            walk_dir(&p, vec![root], &mut plan, 0);
        }
        // symlinks and special files are skipped on purpose
    }
    Ok(plan)
}

fn walk_dir(dir: &Path, subpath: Vec<String>, plan: &mut UploadPlan, depth: usize) {
    if depth > 64 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut had_entries = false;
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        if meta.is_file() {
            had_entries = true;
            plan.files.push(PlannedFile { path: e.path(), subpath: subpath.clone(), size: meta.len() });
        } else if meta.is_dir() {
            had_entries = true;
            let mut sp = subpath.clone();
            sp.push(e.file_name().to_string_lossy().to_string());
            walk_dir(&e.path(), sp, plan, depth + 1);
        }
    }
    if !had_entries {
        plan.empty_dirs.push(subpath);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_download(
    api: &DriveClient,
    transfers: &TransferManager,
    id: &str,
    account: &str,
    file: &DriveFile,
    workspace: bool,
    target: &Path,
    token: &CancellationToken,
) -> Result<(), CloudError> {
    let res = if workspace {
        let (mime, _) = workspace_export_mime_and_ext(&file.mime_type)
            .ok_or_else(|| CloudError::Config("This Google file type cannot be exported".into()))?;
        api.open_export(account, &file.id, mime).await?
    } else {
        api.open_media(account, &file.id).await?
    };
    let tmp = PathBuf::from(format!("{}.cloudy-part", target.display()));
    let result: Result<(), CloudError> = async {
        let mut out = tokio::fs::File::create(&tmp).await?;
        let mut stream = res.bytes_stream();
        let mut done = 0u64;
        loop {
            let next = tokio::select! {
                _ = token.cancelled() => return Err(CloudError::Cancelled),
                n = stream.next() => n,
            };
            match next {
                Some(Ok(chunk)) => {
                    out.write_all(&chunk).await?;
                    done += chunk.len() as u64;
                    transfers.set_progress(id, done);
                }
                Some(Err(e)) => return Err(CloudError::Network(e.to_string())),
                None => break,
            }
        }
        out.flush().await?;
        out.sync_all().await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => tokio::fs::rename(&tmp, target).await.map_err(Into::into),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_resolution() {
        assert_eq!(CoreEngine::resolve_range(None, 100, 50), (0, 49));
        assert_eq!(CoreEngine::resolve_range(Some((10, Some(19))), 100, 50), (10, 19));
        assert_eq!(CoreEngine::resolve_range(Some((90, None)), 100, 50), (90, 99));
        assert_eq!(CoreEngine::resolve_range(Some((0, Some(1_000_000))), 100, 50), (0, 49));
    }

    #[test]
    fn names_are_validated_and_local_names_safe() {
        assert!(valid_name("  ").is_err());
        assert!(valid_name("a/b").is_err());
        assert_eq!(valid_name(" ok ").unwrap(), "ok");
        assert_eq!(safe_local_name("a/b"), "a_b");
        assert_eq!(safe_local_name(".."), "_");
    }

    #[test]
    fn unique_path_does_not_overwrite() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "x").unwrap();
        assert_eq!(unique_path(d.path(), "a.txt"), d.path().join("a (1).txt"));
        assert_eq!(unique_path(d.path(), "new.txt"), d.path().join("new.txt"));
    }

    #[test]
    fn image_sniffing() {
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(b"\x89PNG\r\n"), Some("image/png"));
        assert_eq!(sniff_image_mime(b"RIFFxxxxWEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image_mime(b"nope"), None);
    }

    #[test]
    fn upload_plan_walks_folders_and_marks_empty_dirs() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Trip");
        std::fs::create_dir_all(root.join("day1/empty")).unwrap();
        std::fs::write(root.join("a.txt"), "aa").unwrap();
        std::fs::write(root.join("day1/b.txt"), "bbb").unwrap();
        let plan = collect_upload_plan(vec![root]).unwrap();
        let mut got: Vec<(String, Vec<String>)> = plan
            .files
            .iter()
            .map(|f| (f.path.file_name().unwrap().to_string_lossy().to_string(), f.subpath.clone()))
            .collect();
        got.sort();
        assert_eq!(got[0], ("a.txt".to_string(), vec!["Trip".to_string()]));
        assert_eq!(got[1], ("b.txt".to_string(), vec!["Trip".to_string(), "day1".to_string()]));
        assert_eq!(plan.empty_dirs, vec![vec!["Trip".to_string(), "day1".to_string(), "empty".to_string()]]);
    }
}
