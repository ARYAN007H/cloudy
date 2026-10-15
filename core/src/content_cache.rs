use crate::error::CloudError;
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use std::fs;
use std::path::{Path, PathBuf};
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
    max_size: u64,
}

impl ContentCache {
    pub fn new<P: AsRef<Path>>(cache_dir: P, max_size: u64) -> Result<Self, CloudError> {
        let path = cache_dir.as_ref().to_path_buf();
        fs::create_dir_all(&path).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(Self { cache_dir: path, max_size })
    }

    fn file_path(&self, file_id: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(file_id.as_bytes());
        let hash = hex::encode(hasher.finalize());
        self.cache_dir.join(hash)
    }

    fn meta_path(&self, file_id: &str) -> PathBuf {
        let mut path = self.file_path(file_id);
        path.set_extension("meta");
        path
    }

    pub fn get(&self, file_id: &str, expected_md5: Option<&str>, expected_mtime: &str) -> Option<PathBuf> {
        let fpath = self.file_path(file_id);
        let mpath = self.meta_path(file_id);

        if !fpath.exists() || !mpath.exists() {
            return None;
        }

        if let Ok(meta_json) = fs::read_to_string(&mpath) {
            if let Ok(mut meta) = serde_json::from_str::<CacheMeta>(&meta_json) {
                if meta.mtime == expected_mtime && (expected_md5.is_none() || expected_md5.unwrap() == meta.md5) {
                    meta.last_access = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
                    let _ = fs::write(&mpath, serde_json::to_string(&meta).unwrap_or_default());
                    return Some(fpath);
                }
            }
        }
        None
    }

    pub fn put(&self, file_id: &str, data: &[u8], md5: &str, mtime: &str) -> Result<PathBuf, CloudError> {
        let size = data.len() as u64;
        self.evict_to_fit(size)?;

        let fpath = self.file_path(file_id);
        let mpath = self.meta_path(file_id);

        fs::write(&fpath, data).map_err(|e| CloudError::Io(e.to_string()))?;

        let meta = CacheMeta {
            md5: md5.to_string(),
            mtime: mtime.to_string(),
            size,
            last_access: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs(),
        };
        fs::write(&mpath, serde_json::to_string(&meta).unwrap_or_default()).map_err(|e| CloudError::Io(e.to_string()))?;

        Ok(fpath)
    }

    pub fn read_range(&self, file_id: &str, offset: u64, length: u64) -> Result<Option<Vec<u8>>, CloudError> {
        let fpath = self.file_path(file_id);
        if !fpath.exists() {
            return Ok(None);
        }

        use std::io::{Read, Seek, SeekFrom};
        let mut file = fs::File::open(&fpath).map_err(|e| CloudError::Io(e.to_string()))?;
        file.seek(SeekFrom::Start(offset)).map_err(|e| CloudError::Io(e.to_string()))?;
        
        let mut buf = vec![0; length as usize];
        let bytes_read = file.read(&mut buf).map_err(|e| CloudError::Io(e.to_string()))?;
        buf.truncate(bytes_read);
        
        Ok(Some(buf))
    }

    pub fn has_valid(&self, file_id: &str, md5: Option<&str>, mtime: &str) -> bool {
        self.get(file_id, md5, mtime).is_some()
    }

    pub fn total_size(&self) -> Result<u64, CloudError> {
        let mut total = 0;
        if let Ok(entries) = fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
        Ok(total)
    }

    pub fn evict_to_fit(&self, needed_bytes: u64) -> Result<(), CloudError> {
        let mut current_size = self.total_size()?;
        if current_size + needed_bytes <= self.max_size {
            return Ok(());
        }

        let mut metas = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("meta") {
                    if let Ok(meta_json) = fs::read_to_string(&path) {
                        if let Ok(meta) = serde_json::from_str::<CacheMeta>(&meta_json) {
                            metas.push((path, meta));
                        }
                    }
                }
            }
        }

        metas.sort_by_key(|(_, m)| m.last_access);

        for (mpath, meta) in metas {
            let mut fpath = mpath.clone();
            fpath.set_extension("");
            
            let _ = fs::remove_file(&fpath);
            let _ = fs::remove_file(&mpath);
            
            current_size = current_size.saturating_sub(meta.size);
            if current_size + needed_bytes <= self.max_size {
                break;
            }
        }

        Ok(())
    }

    pub fn clear(&self) -> Result<(), CloudError> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir).map_err(|e| CloudError::Io(e.to_string()))?;
            fs::create_dir_all(&self.cache_dir).map_err(|e| CloudError::Io(e.to_string()))?;
        }
        Ok(())
    }
}
