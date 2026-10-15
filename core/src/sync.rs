use crate::api::DriveClient;
use crate::cache::MetadataCache;
use crate::error::CloudError;
use crate::tree::VirtualTree;
use crate::types::SyncStatus;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

pub struct SyncEngine {
    statuses: Arc<RwLock<HashMap<String, SyncStatus>>>,
    stop_signal: Arc<AtomicBool>,
    all_accounts: Arc<RwLock<Vec<String>>>,
}

impl SyncEngine {
    pub fn new() -> Self {
        Self {
            statuses: Arc::new(RwLock::new(HashMap::new())),
            stop_signal: Arc::new(AtomicBool::new(false)),
            all_accounts: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn start(&self, accounts: Vec<String>, interval: u64, api: Arc<DriveClient>, cache: Arc<MetadataCache>, tree: Arc<VirtualTree>) -> Result<(), CloudError> {
        self.stop_signal.store(false, Ordering::SeqCst);
        *self.all_accounts.write() = accounts.clone();

        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                tracing::warn!("No active Tokio runtime found to start background sync; sync will start when runtime is available");
                return Ok(());
            }
        };
        
        for account in accounts {
            let statuses = self.statuses.clone();
            let stop_signal = self.stop_signal.clone();
            let api = api.clone();
            let cache = cache.clone();
            let tree = tree.clone();
            let acc = account.clone();
            let all_accounts = self.all_accounts.clone();

            handle.spawn(async move {
                statuses.write().insert(acc.clone(), SyncStatus::Idle);
                
                let file_count = cache.get_file_count_for_account(&acc).unwrap_or(0);
                let mut current_token = cache.get_change_token(&acc).ok().flatten().unwrap_or_default();

                if file_count == 0 || current_token.is_empty() {
                    let accs = all_accounts.read().clone();
                    if let Err(e) = Self::execute_full_sync(&acc, api.clone(), cache.clone(), tree.clone(), &accs).await {
                        statuses.write().insert(acc.clone(), SyncStatus::Error(e.to_string()));
                        return;
                    }
                    current_token = cache.get_change_token(&acc).ok().flatten().unwrap_or_default();
                }

                loop {
                    if stop_signal.load(Ordering::SeqCst) {
                        break;
                    }

                    statuses.write().insert(acc.clone(), SyncStatus::Syncing);

                    match api.get_changes(&acc, &current_token).await {
                        Ok((changes, new_token)) => {
                            for change in changes {
                                if change.removed {
                                    let _ = cache.remove_file(&change.file_id, &acc);
                                } else if let Some(file) = change.file {
                                    let _ = cache.upsert_file(&file, &acc);
                                }
                            }
                            
                            if current_token != new_token {
                                current_token = new_token;
                                let _ = cache.save_change_token(&acc, &current_token);
                                
                                let accs = all_accounts.read().clone();
                                let _ = tree.rebuild_from_cache(&cache, &accs);
                            }
                            statuses.write().insert(acc.clone(), SyncStatus::Idle);
                        },
                        Err(CloudError::Auth(_)) => {
                            statuses.write().insert(acc.clone(), SyncStatus::NeedsReauth);
                        },
                        Err(CloudError::Network(_)) => {
                            statuses.write().insert(acc.clone(), SyncStatus::Offline);
                        },
                        Err(e) => {
                            statuses.write().insert(acc.clone(), SyncStatus::Error(e.to_string()));
                        }
                    }

                    sleep(Duration::from_secs(interval)).await;
                }
            });
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.stop_signal.store(true, Ordering::SeqCst);
    }

    pub async fn force_sync(&self, account: &str, api: Arc<DriveClient>, cache: Arc<MetadataCache>, tree: Arc<VirtualTree>, all_accounts: &[String]) -> Result<(), CloudError> {
        self.statuses.write().insert(account.to_string(), SyncStatus::Syncing);
        let res = self.force_sync_inner(account, api, cache, tree, all_accounts).await;
        match &res {
            Ok(_) => { self.statuses.write().insert(account.to_string(), SyncStatus::Idle); },
            Err(CloudError::Auth(_)) => { self.statuses.write().insert(account.to_string(), SyncStatus::NeedsReauth); },
            Err(e) => { self.statuses.write().insert(account.to_string(), SyncStatus::Error(e.to_string())); },
        }
        res
    }

    async fn force_sync_inner(&self, account: &str, api: Arc<DriveClient>, cache: Arc<MetadataCache>, tree: Arc<VirtualTree>, all_accounts: &[String]) -> Result<(), CloudError> {
        let count = cache.get_file_count_for_account(account).unwrap_or(0);
        let current_token = cache.get_change_token(account)?.unwrap_or_default();
        if current_token.is_empty() || count == 0 {
            return Self::execute_full_sync(account, api, cache, tree, all_accounts).await;
        }

        let (changes, new_token) = api.get_changes(account, &current_token).await?;
        for change in changes {
            if change.removed {
                cache.remove_file(&change.file_id, account)?;
            } else if let Some(file) = change.file {
                cache.upsert_file(&file, account)?;
            }
        }
        cache.save_change_token(account, &new_token)?;
        tree.rebuild_from_cache(&cache, all_accounts)?;
        Ok(())
    }

    pub async fn full_sync(&self, account: &str, api: Arc<DriveClient>, cache: Arc<MetadataCache>, tree: Arc<VirtualTree>, all_accounts: &[String]) -> Result<(), CloudError> {
        self.statuses.write().insert(account.to_string(), SyncStatus::Syncing);
        let res = Self::execute_full_sync(account, api, cache, tree, all_accounts).await;
        match &res {
            Ok(_) => { self.statuses.write().insert(account.to_string(), SyncStatus::Idle); },
            Err(CloudError::Auth(_)) => { self.statuses.write().insert(account.to_string(), SyncStatus::NeedsReauth); },
            Err(e) => { self.statuses.write().insert(account.to_string(), SyncStatus::Error(e.to_string())); },
        }
        res
    }

    pub async fn execute_full_sync(account: &str, api: Arc<DriveClient>, cache: Arc<MetadataCache>, tree: Arc<VirtualTree>, all_accounts: &[String]) -> Result<(), CloudError> {
        let files = api.list_all_files(account).await?;
        cache.clear_account(account)?;
        for file in files {
            cache.upsert_file(&file, account)?;
        }
        let token = api.get_start_page_token(account).await?;
        cache.save_change_token(account, &token)?;
        tree.rebuild_from_cache(&cache, all_accounts)?;
        Ok(())
    }

    pub fn get_status(&self, account: &str) -> SyncStatus {
        self.statuses.read().get(account).cloned().unwrap_or(SyncStatus::Idle)
    }

    pub fn get_all_statuses(&self) -> HashMap<String, SyncStatus> {
        self.statuses.read().clone()
    }
}
