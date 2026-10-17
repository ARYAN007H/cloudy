//! Decides which account a new file is stored in.

use crate::api::DriveClient;
use crate::config::UploadPolicy;
use crate::error::CloudError;
use crate::types::StorageQuota;
use futures_util::future::join_all;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const QUOTA_TTL: Duration = Duration::from_secs(60);

pub struct UploadRouter {
    policy: RwLock<UploadPolicy>,
    quotas: RwLock<HashMap<String, (StorageQuota, Instant)>>,
    /// Bytes promised to uploads that are still running. Without this, a batch
    /// of files could all be sent to the same almost-full account.
    reserved: Mutex<HashMap<String, u64>>,
    rr: AtomicUsize,
}

/// Pure selection logic (unit-tested). `candidates` = (account, quota).
pub fn pick_account(
    policy: &UploadPolicy,
    candidates: &[(String, StorageQuota)],
    reserved: &HashMap<String, u64>,
    size: u64,
    rr_start: usize,
) -> Result<String, CloudError> {
    let free = |a: &str, q: &StorageQuota| q.free().saturating_sub(*reserved.get(a).unwrap_or(&0));
    let fits = |a: &str, q: &StorageQuota| q.limit == 0 || free(a, q) >= size;
    let best_free = candidates.iter().map(|(a, q)| free(a, q)).max().unwrap_or(0);
    let no_space = || CloudError::InsufficientSpace { needed: size, best_free };

    match policy {
        UploadPolicy::MostFreeSpace => candidates
            .iter()
            .filter(|(a, q)| fits(a, q))
            .max_by_key(|(a, q)| free(a, q))
            .map(|(a, _)| a.clone())
            .ok_or_else(no_space),
        UploadPolicy::RoundRobin => {
            let n = candidates.len();
            (0..n)
                .map(|i| &candidates[(rr_start + i) % n])
                .find(|(a, q)| fits(a, q))
                .map(|(a, _)| a.clone())
                .ok_or_else(no_space)
        }
        UploadPolicy::PinnedAccount(email) => match candidates.iter().find(|(a, _)| a == email) {
            Some((a, q)) if fits(a, q) => Ok(a.clone()),
            Some(_) => Err(no_space()),
            None => Err(CloudError::Config(format!("The pinned account {email} is not available"))),
        },
    }
}

impl UploadRouter {
    pub fn new(policy: UploadPolicy) -> Self {
        Self {
            policy: RwLock::new(policy),
            quotas: RwLock::new(HashMap::new()),
            reserved: Mutex::new(HashMap::new()),
            rr: AtomicUsize::new(0),
        }
    }

    pub fn set_policy(&self, policy: UploadPolicy) {
        *self.policy.write() = policy;
    }

    pub fn invalidate_quota_cache(&self) {
        self.quotas.write().clear();
    }

    /// Pick an account with room for `file_size` bytes and reserve that space.
    /// Call [`release`](Self::release) when the upload finishes or fails.
    pub async fn select_account(
        &self,
        accounts: &[String],
        file_size: u64,
        api: &DriveClient,
    ) -> Result<String, CloudError> {
        if accounts.is_empty() {
            return Err(CloudError::Config("No Google accounts are connected yet".into()));
        }
        // Quotas of all accounts in parallel; accounts that cannot be
        // queried (expired login, offline) are skipped instead of failing everything.
        let results = join_all(accounts.iter().map(|a| async move { (a.clone(), self.quota(a, api).await) })).await;
        let mut candidates = Vec::new();
        let mut last_err = None;
        for (a, r) in results {
            match r {
                Ok(q) => candidates.push((a, q)),
                Err(e) => last_err = Some(e),
            }
        }
        if candidates.is_empty() {
            return Err(last_err.unwrap_or(CloudError::Quota));
        }

        let policy = self.policy.read().clone();
        let rr_start = self.rr.fetch_add(1, Ordering::SeqCst);
        let mut reserved = self.reserved.lock();
        let chosen = pick_account(&policy, &candidates, &reserved, file_size, rr_start)?;
        *reserved.entry(chosen.clone()).or_insert(0) += file_size;
        Ok(chosen)
    }

    /// Mark `bytes` of `account` as spoken for (used when re-queuing an upload).
    pub fn reserve(&self, account: &str, bytes: u64) {
        *self.reserved.lock().entry(account.to_string()).or_insert(0) += bytes;
    }

    pub fn release(&self, account: &str, bytes: u64) {
        let mut r = self.reserved.lock();
        if let Some(v) = r.get_mut(account) {
            *v = v.saturating_sub(bytes);
        }
        // Quota usage has changed.
        self.quotas.write().remove(account);
    }

    pub async fn quota(&self, account: &str, api: &DriveClient) -> Result<StorageQuota, CloudError> {
        if let Some((q, t)) = self.quotas.read().get(account) {
            if t.elapsed() < QUOTA_TTL {
                return Ok(q.clone());
            }
        }
        let q = api.get_quota(account).await?;
        self.quotas.write().insert(account.to_string(), (q.clone(), Instant::now()));
        Ok(q)
    }

    pub fn forget_account(&self, account: &str) {
        self.quotas.write().remove(account);
        self.reserved.lock().remove(account);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;
    fn q(limit_gb: u64, used_gb: u64) -> StorageQuota {
        StorageQuota { limit: limit_gb * GB, usage: used_gb * GB, usage_in_drive: 0 }
    }
    fn cands() -> Vec<(String, StorageQuota)> {
        vec![("a".into(), q(15, 14)), ("b".into(), q(15, 3)), ("c".into(), q(15, 10))]
    }

    #[test]
    fn most_free_picks_emptiest_that_fits() {
        let r = HashMap::new();
        assert_eq!(pick_account(&UploadPolicy::MostFreeSpace, &cands(), &r, GB, 0).unwrap(), "b");
    }

    #[test]
    fn reservations_steer_a_batch_to_other_accounts() {
        let mut r = HashMap::new();
        r.insert("b".to_string(), 11 * GB); // b now effectively has 1 GB free
        assert_eq!(pick_account(&UploadPolicy::MostFreeSpace, &cands(), &r, 2 * GB, 0).unwrap(), "c");
    }

    #[test]
    fn too_big_for_any_single_account_reports_best_free() {
        let r = HashMap::new();
        match pick_account(&UploadPolicy::MostFreeSpace, &cands(), &r, 13 * GB, 0) {
            Err(CloudError::InsufficientSpace { needed, best_free }) => {
                assert_eq!(needed, 13 * GB);
                assert_eq!(best_free, 12 * GB);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn round_robin_rotates_and_skips_full_accounts() {
        let r = HashMap::new();
        let c = cands();
        assert_eq!(pick_account(&UploadPolicy::RoundRobin, &c, &r, 2 * GB, 0).unwrap(), "b"); // a is full
        assert_eq!(pick_account(&UploadPolicy::RoundRobin, &c, &r, 2 * GB, 1).unwrap(), "b");
        assert_eq!(pick_account(&UploadPolicy::RoundRobin, &c, &r, 2 * GB, 2).unwrap(), "c");
    }

    #[test]
    fn pinned_respects_space_and_availability() {
        let r = HashMap::new();
        let p = UploadPolicy::PinnedAccount("a".into());
        assert!(pick_account(&p, &cands(), &r, GB / 2, 0).is_ok());
        assert!(matches!(pick_account(&p, &cands(), &r, 5 * GB, 0), Err(CloudError::InsufficientSpace { .. })));
        let missing = UploadPolicy::PinnedAccount("zzz".into());
        assert!(matches!(pick_account(&missing, &cands(), &r, 1, 0), Err(CloudError::Config(_))));
    }

    #[test]
    fn unlimited_accounts_always_fit() {
        let c = vec![("w".to_string(), StorageQuota { limit: 0, usage: 999 * GB, usage_in_drive: 0 })];
        assert_eq!(pick_account(&UploadPolicy::MostFreeSpace, &c, &HashMap::new(), 500 * GB, 0).unwrap(), "w");
    }
}
