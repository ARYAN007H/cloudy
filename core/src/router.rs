use crate::api::DriveClient;
use crate::config::UploadPolicy;
use crate::error::CloudError;
use crate::types::StorageQuota;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub struct UploadRouter {
    policy: RwLock<UploadPolicy>,
    cached_quotas: RwLock<HashMap<String, (StorageQuota, Instant)>>,
    round_robin_index: AtomicUsize,
}

impl UploadRouter {
    pub fn new(policy: UploadPolicy) -> Self {
        Self {
            policy: RwLock::new(policy),
            cached_quotas: RwLock::new(HashMap::new()),
            round_robin_index: AtomicUsize::new(0),
        }
    }

    pub fn set_policy(&self, policy: UploadPolicy) {
        *self.policy.write() = policy;
    }

    pub fn invalidate_quota_cache(&self) {
        self.cached_quotas.write().clear();
    }

    pub async fn select_account(&self, accounts: &[String], file_size: u64, api: &DriveClient) -> Result<String, CloudError> {
        if accounts.is_empty() {
            return Err(CloudError::Io("No connected accounts".into()));
        }

        let policy = self.policy.read().clone();

        match policy {
            UploadPolicy::MostFreeSpace => {
                let mut best_account = None;
                let mut max_free: u64 = 0;

                for account in accounts {
                    let quota = self.get_or_fetch_quota(account, api).await?;
                    if quota.limit > 0 {
                        let free = quota.limit.saturating_sub(quota.usage);
                        if free > file_size && free > max_free {
                            max_free = free;
                            best_account = Some(account.clone());
                        }
                    } else {
                        // Unlimited quota?
                        return Ok(account.clone());
                    }
                }
                
                if let Some(acc) = best_account {
                    Ok(acc)
                } else {
                    Err(CloudError::Quota)
                }
            }
            UploadPolicy::RoundRobin => {
                let start_idx = self.round_robin_index.fetch_add(1, Ordering::SeqCst);
                for i in 0..accounts.len() {
                    let idx = (start_idx + i) % accounts.len();
                    let account = &accounts[idx];
                    let quota = self.get_or_fetch_quota(account, api).await?;
                    if quota.limit == 0 || quota.limit.saturating_sub(quota.usage) > file_size {
                        return Ok(account.clone());
                    }
                }
                Err(CloudError::Quota)
            }
            UploadPolicy::PinnedAccount(email) => {
                if !accounts.contains(&email) {
                    return Err(CloudError::Config(format!("Pinned account {} not connected", email)));
                }
                let quota = self.get_or_fetch_quota(&email, api).await?;
                if quota.limit > 0 && quota.limit.saturating_sub(quota.usage) <= file_size {
                    return Err(CloudError::Quota);
                }
                Ok(email)
            }
        }
    }

    async fn get_or_fetch_quota(&self, account: &str, api: &DriveClient) -> Result<StorageQuota, CloudError> {
        {
            let cache = self.cached_quotas.read();
            if let Some((quota, time)) = cache.get(account) {
                if time.elapsed() < Duration::from_secs(60) {
                    return Ok(quota.clone());
                }
            }
        }
        
        let quota = api.get_quota(account).await?;
        self.cached_quotas.write().insert(account.to_string(), (quota.clone(), Instant::now()));
        Ok(quota)
    }
}
