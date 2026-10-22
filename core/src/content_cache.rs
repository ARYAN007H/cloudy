//! On-disk LRU cache for downloaded content and thumbnails.

use crate::error::CloudError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Debug)]
pub struct CacheMeta {
    pub md5: String,
    pub mtime: String,
    pub size: u64,
    pub last_access: u64,
}

pub struct ContentCache {
    cache_dir: PathBuf,
    max_size: AtomicU64,
    /// Tracked incrementally so `put` does not re-scan the directory.
    current: AtomicU64,
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn io<E: std::fmt::Display>(e: E) -> CloudError {
    CloudError::Io(e.to_string())
}

impl ContentCache {
    pub fn new<P: AsRef<Path>>(cache_dir: P, max_size: u64) -> Result<Self, CloudError> {
        let path = cache_dir.as_ref().to_path_buf();
        fs::create_dir_all(&path).map_err(io)?;
        let c = Self { cache_dir: path, max_size: AtomicU64::new(max_size), current: AtomicU64::new(0) };
        c.current.store(c.scan_size(), Ordering::SeqCst);
        Ok(c)
    }

    pub fn set_max_size(&self, bytes: u64) {
        self.max_size.store(bytes, Ordering::SeqCst);
    }

    fn scan_size(&self) -> u64 {
        fs::read_dir(&self.cache_dir)
            .map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0)
    }

    fn file_path(&self, key: &str) -> PathBuf {
        self.cache_dir.join(hex::encode(Sha256::digest(key.as_bytes())))
    }

    fn meta_path(&self, key: &str) -> PathBuf {
        self.file_path(key).with_extension("meta")
    }

    pub fn get(&self, key: &str, expected_md5: Option<&str>, expected_mtime: &str) -> Option<PathBuf> {
        let (fpath, mpath) = (self.file_path(key), self.meta_path(key));
        if !fpath.exists() {
            return None;
        }
        let mut meta: CacheMeta = serde_json::from_str(&fs::read_to_string(&mpath).ok()?).ok()?;
        let md5_ok = expected_md5.map_or(true, |m| m == meta.md5);
        if meta.mtime != expected_mtime || !md5_ok {
            return None;
        }
        meta.last_access = now_millis();
        let _ = fs::write(&mpath, serde_json::to_string(&meta).unwrap_or_default());
        Some(fpath)
    }

    /// Convenience: cached bytes for `key` if present and fresh.
    pub fn get_bytes(&self, key: &str, expected_mtime: &str) -> Option<Vec<u8>> {
        fs::read(self.get(key, None, expected_mtime)?).ok()
    }

    pub fn put(&self, key: &str, data: &[u8], md5: &str, mtime: &str) -> Result<PathBuf, CloudError> {
        let size = data.len() as u64;
        let meta = CacheMeta { md5: md5.into(), mtime: mtime.into(), size, last_access: now_millis() };
        let meta_json = serde_json::to_string(&meta).unwrap_or_default();
        let added = size + meta_json.len() as u64;
        let (fpath, mpath) = (self.file_path(key), self.meta_path(key));
        let on_disk = |p: &Path| fs::metadata(p).map(|m| m.len()).unwrap_or(0);

        // Replacing an entry frees its old data file *and* its old sidecar, so
        // only the difference needs room. (The sidecar counts toward the budget too.)
        self.evict_to_fit(added.saturating_sub(on_disk(&fpath) + on_disk(&mpath)))?;
        // Re-measure: eviction may just have removed this very entry.
        let old = on_disk(&fpath) + on_disk(&mpath);

        // Write to a temp file first so a crash never leaves a truncated entry.
        let tmp = fpath.with_extension("part");
        fs::write(&tmp, data).map_err(io)?;
        fs::rename(&tmp, &fpath).map_err(io)?;
        fs::write(&mpath, &meta_json).map_err(io)?;

        if added >= old {
            self.current.fetch_add(added - old, Ordering::SeqCst);
        } else {
            self.current.fetch_sub(old - added, Ordering::SeqCst);
        }
        Ok(fpath)
    }

    pub fn read_range(&self, key: &str, offset: u64, length: u64) -> Result<Option<Vec<u8>>, CloudError> {
        use std::io::{Read, Seek, SeekFrom};
        let fpath = self.file_path(key);
        if !fpath.exists() {
            return Ok(None);
        }
        let mut file = fs::File::open(&fpath).map_err(io)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut buf = Vec::new();
        file.take(length).read_to_end(&mut buf).map_err(io)?;
        Ok(Some(buf))
    }

    pub fn has_valid(&self, key: &str, md5: Option<&str>, mtime: &str) -> bool {
        self.get(key, md5, mtime).is_some()
    }

    pub fn total_size(&self) -> Result<u64, CloudError> {
        Ok(self.current.load(Ordering::SeqCst))
    }

    pub fn evict_to_fit(&self, needed_bytes: u64) -> Result<(), CloudError> {
        let max = self.max_size.load(Ordering::SeqCst);
        if self.current.load(Ordering::SeqCst) + needed_bytes <= max {
            return Ok(());
        }
        // Looks full: re-measure the directory first so a drifted counter can
        // never cause needless eviction.
        self.current.store(self.scan_size(), Ordering::SeqCst);
        if self.current.load(Ordering::SeqCst) + needed_bytes <= max {
            return Ok(());
        }
        let mut metas = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.cache_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("meta") {
                    if let Some(m) = fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<CacheMeta>(&s).ok()) {
                        metas.push((p, m));
                    }
                }
            }
        }
        metas.sort_by_key(|(_, m)| m.last_access);
        for (mpath, meta) in metas {
            let fpath = mpath.with_extension("");
            let freed = fs::metadata(&fpath).map(|m| m.len()).unwrap_or(0)
                + fs::metadata(&mpath).map(|m| m.len()).unwrap_or(0);
            let _ = fs::remove_file(&fpath);
            let _ = fs::remove_file(&mpath);
            let _ = meta;
            let cur = self.current.load(Ordering::SeqCst);
            self.current.store(cur.saturating_sub(freed), Ordering::SeqCst);
            if self.current.load(Ordering::SeqCst) + needed_bytes <= max {
                break;
            }
        }
        Ok(())
    }

    pub fn clear(&self) -> Result<(), CloudError> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir).map_err(io)?;
        }
        fs::create_dir_all(&self.cache_dir).map_err(io)?;
        self.current.store(0, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_range_and_staleness() {
        let d = tempfile::tempdir().unwrap();
        let c = ContentCache::new(d.path(), 1 << 20).unwrap();
        c.put("k", b"hello world", "md5", "t1").unwrap();
        assert_eq!(c.get_bytes("k", "t1").unwrap(), b"hello world");
        assert!(c.get_bytes("k", "t2").is_none(), "changed mtime invalidates");
        assert_eq!(c.read_range("k", 6, 100).unwrap().unwrap(), b"world");
        assert!(c.read_range("missing", 0, 1).unwrap().is_none());
    }

    #[test]
    fn size_counter_matches_disk_when_entries_are_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let c = ContentCache::new(d.path(), 1 << 20).unwrap();
        let on_disk = || -> u64 {
            fs::read_dir(d.path()).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum()
        };
        for i in 0..60 {
            // same key, changing sizes (grow and shrink) plus a few other keys
            c.put("same", &vec![1u8; 50 + (i * 37) % 400], "md5", &format!("t{i}")).unwrap();
            if i % 7 == 0 {
                c.put(&format!("other{i}"), &[2u8; 30], "", "t").unwrap();
            }
            assert_eq!(c.total_size().unwrap(), on_disk(), "counter drifted after overwrite #{i}");
        }
    }

    #[test]
    fn overwriting_does_not_trigger_needless_eviction() {
        let d = tempfile::tempdir().unwrap();
        // Budget fits exactly one ~300-byte entry (data + sidecar), not two.
        let c = ContentCache::new(d.path(), 400).unwrap();
        c.put("a", &[0u8; 250], "", "t0").unwrap();
        for i in 1..30 {
            c.put("a", &[0u8; 250], "", &format!("t{i}")).unwrap();
        }
        assert!(c.get_bytes("a", "t29").is_some(), "rewriting the only entry must not evict it");
    }

    #[test]
    fn lru_eviction_keeps_size_bounded() {
        let d = tempfile::tempdir().unwrap();
        let c = ContentCache::new(d.path(), 1000).unwrap();
        for i in 0..20 {
            c.put(&format!("k{i}"), &[0u8; 200], "", "t").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(c.total_size().unwrap() <= 1000, "size {}", c.total_size().unwrap());
        assert!(c.get_bytes("k19", "t").is_some(), "newest entry survives");
        assert!(c.get_bytes("k0", "t").is_none(), "oldest entry evicted");
    }
}
