//! Upload / download queue with progress, cancellation, retries and resume.

use crate::api::{DriveClient, UploadStatus};
use crate::cache::{MetadataCache, PendingUpload};
use crate::error::CloudError;
use crate::router::UploadRouter;
use crate::types::{DriveFile, EngineEvent, TransferDirection, TransferInfo, TransferStatus};
use md5::{Digest, Md5};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio::sync::{broadcast, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Drive requires chunks to be a multiple of 256 KiB (except the last one).
const CHUNK: usize = 16 * 256 * 1024; // 4 MiB
const MAX_RETRIES: u32 = 6;
const MAX_PARALLEL_UPLOADS: usize = 3;

pub type EventBus = broadcast::Sender<EngineEvent>;

/// Called once an upload finished so the engine can index the new file.
pub type OnUploaded = Arc<dyn Fn(&str, &DriveFile) + Send + Sync>;

#[derive(Clone)]
pub struct UploadJob {
    pub id: String,
    pub local_path: PathBuf,
    pub remote_parent_id: String,
    pub remote_name: String,
    pub account: String,
    pub total_size: u64,
    /// Existing resumable session (after an app restart).
    pub session_uri: Option<String>,
    pub confirmed: u64,
}

struct Entry {
    info: TransferInfo,
    seq: u64,
    last_emit: Instant,
}

#[derive(Clone)]
pub struct UploadCtx {
    pub api: Arc<DriveClient>,
    pub cache: Arc<MetadataCache>,
    pub router: Arc<UploadRouter>,
    pub on_uploaded: OnUploaded,
}

pub struct TransferManager {
    transfers: Arc<RwLock<HashMap<String, Entry>>>,
    cancels: Arc<Mutex<HashMap<String, CancellationToken>>>,
    jobs: Arc<Mutex<HashMap<String, UploadJob>>>,
    sem: Arc<Semaphore>,
    seq: Arc<AtomicU64>,
    events: EventBus,
}

/// Read until `cap` bytes or EOF. A plain `read()` may return short, which
/// would produce illegal (non-256KiB-aligned) intermediate chunks.
pub async fn fill_chunk<R: AsyncRead + Unpin>(r: &mut R, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; cap];
    let mut n = 0;
    while n < cap {
        let k = r.read(&mut buf[n..]).await?;
        if k == 0 {
            break;
        }
        n += k;
    }
    buf.truncate(n);
    Ok(buf)
}

async fn md5_of(path: PathBuf) -> std::io::Result<String> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut h = Md5::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(format!("{:x}", h.finalize()))
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}

impl TransferManager {
    pub fn new(events: EventBus) -> Self {
        Self {
            transfers: Arc::new(RwLock::new(HashMap::new())),
            cancels: Arc::new(Mutex::new(HashMap::new())),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            sem: Arc::new(Semaphore::new(MAX_PARALLEL_UPLOADS)),
            seq: Arc::new(AtomicU64::new(0)),
            events,
        }
    }

    // ------------------------------------------------------------ bookkeeping

    pub fn get_transfers(&self) -> Vec<TransferInfo> {
        let g = self.transfers.read();
        let mut v: Vec<&Entry> = g.values().collect();
        v.sort_by_key(|e| e.seq);
        v.into_iter().map(|e| e.info.clone()).collect()
    }

    pub fn get_transfer(&self, id: &str) -> Option<TransferInfo> {
        self.transfers.read().get(id).map(|e| e.info.clone())
    }

    /// Register a transfer (upload or download); returns its cancellation token.
    pub fn register(&self, info: TransferInfo) -> CancellationToken {
        let token = CancellationToken::new();
        self.cancels.lock().insert(info.id.clone(), token.clone());
        let _ = self.events.send(EngineEvent::Transfer(info.clone()));
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.transfers.write().insert(info.id.clone(), Entry { info, seq, last_emit: Instant::now() });
        token
    }

    pub fn set_progress(&self, id: &str, bytes: u64) {
        let mut g = self.transfers.write();
        if let Some(e) = g.get_mut(id) {
            e.info.progress_bytes = bytes;
            if e.last_emit.elapsed() > Duration::from_millis(200) {
                e.last_emit = Instant::now();
                let _ = self.events.send(EngineEvent::Transfer(e.info.clone()));
            }
        }
    }

    pub fn set_status(&self, id: &str, status: TransferStatus) {
        let mut g = self.transfers.write();
        if let Some(e) = g.get_mut(id) {
            // A cancelled transfer stays cancelled.
            if e.info.status == TransferStatus::Cancelled {
                return;
            }
            if status == TransferStatus::Completed {
                e.info.progress_bytes = e.info.total_bytes;
            }
            e.info.status = status;
            let _ = self.events.send(EngineEvent::Transfer(e.info.clone()));
        }
    }

    pub fn cancel(&self, id: &str) {
        if let Some(t) = self.cancels.lock().get(id) {
            t.cancel();
        }
        let mut g = self.transfers.write();
        if let Some(e) = g.get_mut(id) {
            if !e.info.status.is_finished() {
                e.info.status = TransferStatus::Cancelled;
                let _ = self.events.send(EngineEvent::Transfer(e.info.clone()));
            }
        }
    }

    pub fn cancel_account(&self, account: &str) {
        let ids: Vec<String> =
            self.transfers.read().values().filter(|e| e.info.account == account).map(|e| e.info.id.clone()).collect();
        for id in ids {
            self.cancel(&id);
        }
    }

    pub fn clear_finished(&self) {
        let mut g = self.transfers.write();
        let done: Vec<String> = g.values().filter(|e| e.info.status.is_finished()).map(|e| e.info.id.clone()).collect();
        for id in &done {
            g.remove(id);
            self.cancels.lock().remove(id);
            self.jobs.lock().remove(id);
        }
    }

    // ---------------------------------------------------------------- uploads

    /// Queue an upload. Returns immediately; work happens in the background.
    pub fn enqueue(&self, job: UploadJob, ctx: UploadCtx) -> String {
        let id = job.id.clone();
        let info = TransferInfo {
            id: id.clone(),
            file_name: job.remote_name.clone(),
            account: job.account.clone(),
            progress_bytes: job.confirmed,
            total_bytes: job.total_size,
            status: TransferStatus::Pending,
            direction: TransferDirection::Upload,
        };
        let token = self.register(info);
        self.jobs.lock().insert(id.clone(), job.clone());
        self.spawn(job, ctx, token);
        id
    }

    pub fn new_job(
        local_path: PathBuf,
        remote_parent_id: &str,
        remote_name: &str,
        account: &str,
        total_size: u64,
    ) -> UploadJob {
        UploadJob {
            id: Uuid::new_v4().to_string(),
            local_path,
            remote_parent_id: remote_parent_id.to_string(),
            remote_name: remote_name.to_string(),
            account: account.to_string(),
            total_size,
            session_uri: None,
            confirmed: 0,
        }
    }

    /// Retry a failed upload from where it stopped.
    pub fn retry(&self, id: &str, ctx: UploadCtx) -> bool {
        let Some(mut job) = self.jobs.lock().get(id).cloned() else { return false };
        match self.get_transfer(id) {
            Some(t) if matches!(t.status, TransferStatus::Failed(_) | TransferStatus::Cancelled) => {
                job.confirmed = 0;
                job.session_uri = None;
                let token = CancellationToken::new();
                self.cancels.lock().insert(id.to_string(), token.clone());
                if let Some(e) = self.transfers.write().get_mut(id) {
                    e.info.status = TransferStatus::Pending;
                    e.info.progress_bytes = 0;
                    let _ = self.events.send(EngineEvent::Transfer(e.info.clone()));
                }
                // Space was released when the previous attempt ended.
                ctx.router.reserve(&job.account, job.total_size);
                self.spawn(job, ctx, token);
                true
            }
            _ => false,
        }
    }

    /// Continue uploads that were interrupted by an app restart.
    pub async fn resume_pending(&self, ctx: UploadCtx) -> Result<Vec<String>, CloudError> {
        let mut resumed = Vec::new();
        for p in ctx.cache.get_pending_uploads()? {
            if !PathBuf::from(&p.local_path).exists() {
                ctx.cache.remove_upload_session(&p.id)?;
                continue;
            }
            let PendingUpload { id, local_path, session_uri, total_size, account_email, remote_parent_id, remote_name, .. } = p;
            let confirmed = match ctx.api.check_upload_status(&session_uri, total_size).await {
                Ok(UploadStatus::Incomplete(c)) => c,
                Ok(UploadStatus::Complete(f)) => {
                    (ctx.on_uploaded)(&account_email, &f);
                    ctx.cache.remove_upload_session(&id)?;
                    continue;
                }
                Ok(UploadStatus::SessionExpired) => {
                    ctx.cache.remove_upload_session(&id)?;
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Could not check upload {id}: {e}; will retry on next start");
                    continue;
                }
            };
            ctx.router.reserve(&account_email, total_size.saturating_sub(confirmed));
            let job = UploadJob {
                id: id.clone(),
                local_path: PathBuf::from(local_path),
                remote_parent_id,
                remote_name,
                account: account_email,
                total_size,
                session_uri: Some(session_uri),
                confirmed,
            };
            self.enqueue(job, ctx.clone());
            resumed.push(id);
        }
        Ok(resumed)
    }

    fn spawn(&self, job: UploadJob, ctx: UploadCtx, token: CancellationToken) {
        let me = TransferManager {
            transfers: self.transfers.clone(),
            cancels: self.cancels.clone(),
            jobs: self.jobs.clone(),
            sem: self.sem.clone(),
            seq: self.seq.clone(),
            events: self.events.clone(),
        };
        tokio::spawn(async move {
            // Wait for a free upload slot (but stay cancellable while queued).
            let permit = tokio::select! {
                p = me.sem.clone().acquire_owned() => p.ok(),
                _ = token.cancelled() => None,
            };
            let outcome = match permit {
                Some(_p) => {
                    me.set_status(&job.id, TransferStatus::InProgress);
                    me.run_upload(&job, &ctx, &token).await
                }
                None => Err(CloudError::Cancelled),
            };
            ctx.router.release(&job.account, job.total_size.saturating_sub(job.confirmed));
            match outcome {
                Ok(file) => {
                    let _ = ctx.cache.remove_upload_session(&job.id);
                    (ctx.on_uploaded)(&job.account, &file);
                    me.set_status(&job.id, TransferStatus::Completed);
                }
                Err(CloudError::Cancelled) => {
                    let _ = ctx.cache.remove_upload_session(&job.id);
                    me.set_status(&job.id, TransferStatus::Cancelled);
                }
                Err(e) => {
                    tracing::warn!("Upload '{}' failed: {e}", job.remote_name);
                    // Keep the session only if retrying later could succeed.
                    if !e.is_transient() {
                        let _ = ctx.cache.remove_upload_session(&job.id);
                    }
                    me.set_status(&job.id, TransferStatus::Failed(e.to_string()));
                }
            }
        });
    }

    async fn run_upload(&self, job: &UploadJob, ctx: &UploadCtx, token: &CancellationToken) -> Result<DriveFile, CloudError> {
        let mime = mime_guess::from_path(&job.local_path).first_or_octet_stream().to_string();
        let mut session = match &job.session_uri {
            Some(s) => s.clone(),
            None => {
                let s = ctx
                    .api
                    .initiate_resumable_upload(&job.account, &job.remote_name, &job.remote_parent_id, &mime, job.total_size)
                    .await?;
                ctx.cache.save_upload_session(
                    &job.id,
                    &job.local_path.to_string_lossy(),
                    &s,
                    job.total_size,
                    &job.account,
                    &job.remote_parent_id,
                    &job.remote_name,
                )?;
                s
            }
        };

        // Empty files: a single empty PUT completes the session.
        if job.total_size == 0 {
            return match ctx.api.upload_chunk(&session, vec![], 0, 0).await? {
                UploadStatus::Complete(f) => Ok(f),
                _ => Err(CloudError::Integrity("Drive did not finish the empty upload".into())),
            };
        }

        let mut file = tokio::fs::File::open(&job.local_path).await?;
        let mut offset = job.confirmed;
        let mut restarted = false;
        let mut failures = 0u32;
        let mut file_len_checked = false;

        loop {
            if token.is_cancelled() {
                return Err(CloudError::Cancelled);
            }
            if !file_len_checked {
                let len = file.metadata().await?.len();
                if len != job.total_size {
                    return Err(CloudError::Integrity("The file changed on disk while it was being uploaded".into()));
                }
                file_len_checked = true;
            }
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let data = fill_chunk(&mut file, CHUNK.min((job.total_size - offset) as usize)).await?;
            if data.is_empty() {
                return Err(CloudError::Integrity("Unexpected end of file during upload".into()));
            }

            let res = tokio::select! {
                _ = token.cancelled() => return Err(CloudError::Cancelled),
                r = ctx.api.upload_chunk(&session, data, offset, job.total_size) => r,
            };
            match res {
                Ok(UploadStatus::Complete(remote)) => {
                    if let Some(remote_md5) = &remote.md5_checksum {
                        if let Ok(local) = md5_of(job.local_path.clone()).await {
                            if &local != remote_md5 {
                                let _ = ctx.api.delete_permanently(&job.account, &remote.id).await;
                                return Err(CloudError::Integrity("Checksum mismatch after upload".into()));
                            }
                        }
                    }
                    return Ok(remote);
                }
                Ok(UploadStatus::Incomplete(next)) => {
                    failures = 0;
                    offset = next.min(job.total_size);
                    self.set_progress(&job.id, offset);
                    let _ = ctx.cache.update_upload_progress(&job.id, offset);
                }
                Ok(UploadStatus::SessionExpired) => {
                    if restarted {
                        return Err(CloudError::Integrity("Upload session expired repeatedly".into()));
                    }
                    restarted = true;
                    session = ctx
                        .api
                        .initiate_resumable_upload(&job.account, &job.remote_name, &job.remote_parent_id, &mime, job.total_size)
                        .await?;
                    let _ = ctx.cache.save_upload_session(
                        &job.id,
                        &job.local_path.to_string_lossy(),
                        &session,
                        job.total_size,
                        &job.account,
                        &job.remote_parent_id,
                        &job.remote_name,
                    );
                    offset = 0;
                }
                Err(e) if e.is_transient() && failures < MAX_RETRIES => {
                    failures += 1;
                    tracing::warn!("Upload chunk failed ({e}); retry {failures}/{MAX_RETRIES}");
                    tokio::select! {
                        _ = token.cancelled() => return Err(CloudError::Cancelled),
                        _ = tokio::time::sleep(Duration::from_secs(1 << failures.min(5))) => {}
                    }
                    // Ask Drive how much it really received.
                    match ctx.api.check_upload_status(&session, job.total_size).await {
                        Ok(UploadStatus::Incomplete(n)) => offset = n.min(job.total_size),
                        Ok(UploadStatus::Complete(f)) => return Ok(f),
                        _ => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    /// Returns at most 7 bytes per read – like a slow disk or pipe.
    struct Trickle(Vec<u8>, usize);
    impl AsyncRead for Trickle {
        fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            let n = 7.min(buf.remaining()).min(self.0.len() - self.1);
            let start = self.1;
            buf.put_slice(&self.0[start..start + n]);
            self.1 += n;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn fill_chunk_returns_full_chunks_despite_short_reads() {
        let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let mut r = Trickle(data.clone(), 0);
        let a = fill_chunk(&mut r, 256).await.unwrap();
        assert_eq!(a.len(), 256, "a short read must not yield a short chunk");
        let rest = fill_chunk(&mut r, 4096).await.unwrap();
        assert_eq!(rest.len(), 744);
        assert_eq!([a, rest].concat(), data);
    }

    #[test]
    fn chunk_size_is_256kib_aligned() {
        assert_eq!(CHUNK % (256 * 1024), 0);
    }

    #[test]
    fn manager_tracks_progress_status_and_cancel() {
        let (tx, mut rx) = broadcast::channel(64);
        let m = TransferManager::new(tx);
        let tok = m.register(TransferInfo {
            id: "1".into(),
            file_name: "f".into(),
            account: "a".into(),
            progress_bytes: 0,
            total_bytes: 10,
            status: TransferStatus::Pending,
            direction: TransferDirection::Download,
        });
        m.set_status("1", TransferStatus::InProgress);
        m.set_progress("1", 5);
        assert_eq!(m.get_transfer("1").unwrap().progress_bytes, 5);
        m.cancel("1");
        assert!(tok.is_cancelled());
        m.set_status("1", TransferStatus::Completed); // must not resurrect a cancelled transfer
        assert_eq!(m.get_transfer("1").unwrap().status, TransferStatus::Cancelled);
        assert!(rx.try_recv().is_ok(), "events are published");
        m.clear_finished();
        assert!(m.get_transfers().is_empty());
    }
}
