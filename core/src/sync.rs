//! Background synchronisation of every account into the metadata cache.

use crate::api::DriveClient;
use crate::cache::MetadataCache;
use crate::error::CloudError;
use crate::tree::VirtualTree;
use crate::types::{DriveFile, EngineEvent, SyncStatus};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub type EventBus = broadcast::Sender<EngineEvent>;

struct AccountTask {
    cancel: CancellationToken,
    kick: Arc<Notify>,
}

struct Inner {
    api: Arc<DriveClient>,
    cache: Arc<MetadataCache>,
    tree: Arc<VirtualTree>,
    events: EventBus,
    /// Shared with the engine: the single source of truth for linked accounts.
    accounts: Arc<RwLock<Vec<String>>>,
    statuses: RwLock<HashMap<String, SyncStatus>>,
    tasks: Mutex<HashMap<String, AccountTask>>,
    interval_secs: AtomicU64,
    rebuild_lock: Mutex<()>,
}

#[derive(Clone)]
pub struct SyncEngine {
    inner: Arc<Inner>,
}

impl SyncEngine {
    pub fn new(
        api: Arc<DriveClient>,
        cache: Arc<MetadataCache>,
        tree: Arc<VirtualTree>,
        accounts: Arc<RwLock<Vec<String>>>,
        events: EventBus,
        interval_secs: u64,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                api,
                cache,
                tree,
                events,
                accounts,
                statuses: RwLock::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
                interval_secs: AtomicU64::new(interval_secs.max(5)),
                rebuild_lock: Mutex::new(()),
            }),
        }
    }

    pub fn set_interval(&self, secs: u64) {
        self.inner.interval_secs.store(secs.max(5), Ordering::SeqCst);
        self.kick_all(); // pick up the new interval immediately
    }

    pub fn status(&self, account: &str) -> SyncStatus {
        self.inner.statuses.read().get(account).cloned().unwrap_or(SyncStatus::Idle)
    }

    pub fn is_running(&self, account: &str) -> bool {
        self.inner.tasks.lock().contains_key(account)
    }

    /// Start (or leave running) the sync loop of one account.
    pub fn start_account(&self, handle: &Handle, account: &str) {
        let mut tasks = self.inner.tasks.lock();
        if tasks.contains_key(account) {
            return;
        }
        let cancel = CancellationToken::new();
        let kick = Arc::new(Notify::new());
        tasks.insert(account.to_string(), AccountTask { cancel: cancel.clone(), kick: kick.clone() });
        let inner = self.inner.clone();
        let account = account.to_string();
        handle.spawn(async move { inner.run_account(account, cancel, kick).await });
    }

    pub fn stop_account(&self, account: &str) {
        if let Some(t) = self.inner.tasks.lock().remove(account) {
            t.cancel.cancel();
        }
        self.inner.statuses.write().remove(account);
    }

    pub fn stop_all(&self) {
        for (_, t) in self.inner.tasks.lock().drain() {
            t.cancel.cancel();
        }
    }

    /// Sync this account as soon as possible (also resumes a paused-for-login one).
    pub fn kick(&self, account: &str) {
        if let Some(t) = self.inner.tasks.lock().get(account) {
            t.kick.notify_one();
        }
    }

    pub fn kick_all(&self) {
        for t in self.inner.tasks.lock().values() {
            t.kick.notify_one();
        }
    }

    /// One synchronous round for an account (full or incremental).
    pub async fn sync_once(&self, account: &str) -> Result<(), CloudError> {
        self.inner.sync_once(account, &CancellationToken::new()).await
    }

    /// Rebuild the virtual tree from the cache and tell the UIs.
    pub async fn rebuild_tree(&self) -> Result<(), CloudError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.rebuild_tree_blocking())
            .await
            .map_err(|e| CloudError::Cache(e.to_string()))?
    }

    pub fn rebuild_tree_blocking(&self) -> Result<(), CloudError> {
        self.inner.rebuild_tree_blocking()
    }
}

impl Inner {
    fn emit_status(&self, account: &str, status: SyncStatus) {
        self.statuses.write().insert(account.to_string(), status.clone());
        let files_indexed = self.cache.get_file_count_for_account(account).unwrap_or(0) as u64;
        let _ = self.events.send(EngineEvent::Sync { account: account.to_string(), status, files_indexed });
    }

    fn rebuild_tree_blocking(&self) -> Result<(), CloudError> {
        let _g = self.rebuild_lock.lock(); // serialise: an older rebuild must never overwrite a newer one
        let accounts = self.accounts.read().clone();
        self.tree.rebuild_from_cache(&self.cache, &accounts)?;
        let _ = self.events.send(EngineEvent::TreeChanged);
        Ok(())
    }

    async fn rebuild(self: &Arc<Self>) {
        let me = self.clone();
        match tokio::task::spawn_blocking(move || me.rebuild_tree_blocking()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Tree rebuild failed: {e}"),
            Err(e) => warn!("Tree rebuild task failed: {e}"),
        }
    }

    async fn run_account(self: Arc<Self>, account: String, cancel: CancellationToken, kick: Arc<Notify>) {
        let mut failures = 0u32;
        loop {
            if cancel.is_cancelled() {
                break;
            }
            self.emit_status(&account, SyncStatus::Syncing);
            let wait = match self.sync_once(&account, &cancel).await {
                Ok(()) => {
                    failures = 0;
                    self.emit_status(&account, SyncStatus::Idle);
                    let base = self.interval_secs.load(Ordering::SeqCst);
                    Duration::from_secs(base) + Duration::from_millis(rand::random::<u64>() % 3000)
                }
                Err(CloudError::Cancelled) => break,
                Err(e) if e.needs_reauth() => {
                    warn!("{account}: sign-in required ({e})");
                    self.emit_status(&account, SyncStatus::NeedsReauth);
                    // Nothing to do until the user reconnects (which kicks us).
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = kick.notified() => {}
                    }
                    continue;
                }
                Err(e) => {
                    failures += 1;
                    warn!("{account}: sync failed ({e}); attempt {failures}");
                    let status = match &e {
                        CloudError::Network(_) => SyncStatus::Offline,
                        other => SyncStatus::Error(other.to_string()),
                    };
                    self.emit_status(&account, status);
                    let base = self.interval_secs.load(Ordering::SeqCst);
                    Duration::from_secs((5u64 << failures.min(6)).min(base.max(300)))
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = kick.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
        info!("sync loop for {account} stopped");
    }

    async fn sync_once(self: &Arc<Self>, account: &str, cancel: &CancellationToken) -> Result<(), CloudError> {
        match self.cache.get_change_token(account)? {
            None => self.full_sync(account, cancel).await,
            Some(token) => self.incremental_sync(account, token, cancel).await,
        }
    }

    async fn full_sync(self: &Arc<Self>, account: &str, cancel: &CancellationToken) -> Result<(), CloudError> {
        info!("{account}: full sync");
        // Take the change token BEFORE listing: anything modified while we
        // list is then replayed by the next incremental sync instead of lost.
        let start_token = self.api.get_start_page_token(account).await?;
        let first_time = self.cache.get_file_count_for_account(account)? == 0;

        let mut collected: Vec<DriveFile> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut total = 0usize;
        let mut pages = 0u32;
        loop {
            if cancel.is_cancelled() {
                return Err(CloudError::Cancelled);
            }
            let (files, next) = self.api.list_files_page(account, page_token.as_deref()).await?;
            let files: Vec<DriveFile> = files.into_iter().filter(|f| f.owned_by_me && !f.trashed).collect();
            total += files.len();
            pages += 1;
            if first_time {
                // Show results as they arrive on a first sync.
                self.cache.upsert_files(account, &files)?;
                if pages % 2 == 0 {
                    self.rebuild().await;
                }
            } else {
                collected.extend(files);
            }
            let _ = self.events.send(EngineEvent::Sync {
                account: account.to_string(),
                status: SyncStatus::Syncing,
                files_indexed: total as u64,
            });
            match next {
                Some(n) => page_token = Some(n),
                None => break,
            }
        }
        if first_time {
            self.cache.save_change_token(account, &start_token)?;
        } else {
            self.cache.replace_account_files(account, &collected, &start_token)?;
        }
        self.rebuild().await;
        info!("{account}: indexed {total} files");
        Ok(())
    }

    async fn incremental_sync(
        self: &Arc<Self>,
        account: &str,
        mut token: String,
        cancel: &CancellationToken,
    ) -> Result<(), CloudError> {
        let mut changed = 0usize;
        loop {
            if cancel.is_cancelled() {
                return Err(CloudError::Cancelled);
            }
            let page = match self.api.get_changes_page(account, &token).await {
                Ok(p) => p,
                // The saved token is no longer valid → start over.
                Err(CloudError::Api(e)) if matches!(e.status, 400 | 404 | 410) => {
                    warn!("{account}: change token rejected ({e}); doing a full sync");
                    self.cache.clear_change_token(account)?;
                    return self.full_sync(account, cancel).await;
                }
                Err(e) => return Err(e),
            };
            let next_token = page
                .next_page_token
                .clone()
                .or_else(|| page.new_start_page_token.clone())
                .unwrap_or_else(|| token.clone());
            if page.changes.is_empty() {
                self.cache.save_change_token(account, &next_token)?;
            } else {
                changed += self.cache.apply_changes(account, &page.changes, &next_token)?;
            }
            token = next_token;
            if page.next_page_token.is_none() {
                break;
            }
        }
        if changed > 0 {
            info!("{account}: applied {changed} change(s)");
            self.rebuild().await;
        }
        Ok(())
    }
}
