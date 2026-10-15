use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, TimeOrNow,
};
use libc::{EACCES, EIO, ENOENT, ENOSYS, ENOTEMPTY};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle;
use tracing::{error, info};

use clouddrive_core::{CoreEngine, VirtualNode};
use crate::inode::InodeMap;

const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4096;

#[allow(dead_code)]
pub struct OpenFileHandle {
    pub node_id: String,
    pub drive_file_id: Option<String>,
    pub account: Option<String>,
    pub write_buffer: Option<PathBuf>,
    pub is_writing: bool,
    pub is_workspace_file: bool,
}

pub struct CloudDriveFs {
    pub engine: Arc<CoreEngine>,
    pub runtime: Handle,
    pub inodes: RwLock<InodeMap>,
    pub open_files: Mutex<HashMap<u64, OpenFileHandle>>,
    pub next_fh: AtomicU64,
    pub uid: u32,
    pub gid: u32,
}

impl CloudDriveFs {
    fn node_to_attr(&self, node: &VirtualNode, ino: u64) -> FileAttr {
        let kind = if node.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };

        let perm = if node.is_dir {
            0o755
        } else if node.is_read_only || node.is_workspace_file {
            0o444
        } else {
            0o644
        };

        let mtime = SystemTime::from(node.modified_time);

        FileAttr {
            ino,
            size: node.size,
            blocks: (node.size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64,
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink: if node.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: BLOCK_SIZE,
        }
    }

    fn get_node_id(&self, ino: u64) -> Option<String> {
        self.inodes.read().get_node_id(ino).map(String::from)
    }

    fn ensure_temp_dir() -> PathBuf {
        let mut path = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        path.push("clouddrive");
        path.push("tmp");
        let _ = fs::create_dir_all(&path);
        path
    }
}

impl Filesystem for CloudDriveFs {
    fn lookup(&mut self, _req: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_id = match self.get_node_id(parent) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let name_str = name.to_string_lossy();
        if let Some(node) = self.engine.lookup_child(&parent_id, &name_str) {
            let ino = self.inodes.write().get_or_assign(&node.id);
            let attr = self.node_to_attr(&node, ino);
            reply.entry(&TTL, &attr, 0);
        } else {
            reply.error(ENOENT);
        }
    }

    fn getattr(&mut self, _req: &fuser::Request<'_>, ino: u64, reply: ReplyAttr) {
        let node_id = match self.get_node_id(ino) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if let Some(node) = self.engine.get_node(&node_id) {
            let attr = self.node_to_attr(&node, ino);
            reply.attr(&TTL, &attr);
        } else {
            reply.error(ENOENT);
        }
    }

    fn readdir(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let node_id = match self.get_node_id(ino) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let children = self.engine.get_children(&node_id);
        let mut entries = vec![
            (ino, FileType::Directory, "."),
            (ino, FileType::Directory, ".."), // Simplified, should technically point to parent ino
        ];

        let mut inodes_lock = self.inodes.write();
        for child in &children {
            let child_ino = inodes_lock.get_or_assign(&child.id);
            let kind = if child.is_dir { FileType::Directory } else { FileType::RegularFile };
            entries.push((child_ino, kind, &child.name));
        }
        drop(inodes_lock);

        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &fuser::Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let node_id = match self.get_node_id(ino) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let node = match self.engine.get_node(&node_id) {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let is_writing = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        
        if is_writing && (node.is_workspace_file || node.is_read_only) {
            reply.error(EACCES);
            return;
        }

        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.open_files.lock().insert(fh, OpenFileHandle {
            node_id: node.id,
            drive_file_id: node.drive_file_id,
            account: node.account,
            write_buffer: None,
            is_writing,
            is_workspace_file: node.is_workspace_file,
        });

        reply.opened(fh, flags as u32);
    }

    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        let node_id = {
            let files = self.open_files.lock();
            let handle = match files.get(&fh) {
                Some(h) => h,
                None => {
                    reply.error(EIO);
                    return;
                }
            };
            handle.node_id.clone()
        };

        let data_res = self.runtime.block_on(async {
            self.engine.read_content(&node_id, offset as u64, size as u64).await
        });

        match data_res {
            Ok(data) => reply.data(&data),
            Err(e) => {
                error!("Failed to read content: {:?}", e);
                reply.error(EIO);
            }
        }
    }

    fn write(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyWrite,
    ) {
        let mut files = self.open_files.lock();
        let handle = match files.get_mut(&fh) {
            Some(h) => h,
            None => {
                reply.error(EIO);
                return;
            }
        };

        if handle.is_workspace_file {
            reply.error(EACCES);
            return;
        }

        if handle.write_buffer.is_none() {
            let tmp_dir = Self::ensure_temp_dir();
            let tmp_path = tmp_dir.join(format!("{}.partial", uuid::Uuid::new_v4()));
            handle.write_buffer = Some(tmp_path);
        }

        let tmp_path = handle.write_buffer.as_ref().unwrap();
        let mut file = match OpenOptions::new().create(true).write(true).open(tmp_path) {
            Ok(f) => f,
            Err(_) => {
                reply.error(EIO);
                return;
            }
        };

        if file.seek(SeekFrom::Start(offset as u64)).is_err() {
            reply.error(EIO);
            return;
        }

        match file.write_all(data) {
            Ok(_) => reply.written(data.len() as u32),
            Err(_) => reply.error(EIO),
        }
    }

    fn flush(&mut self, _req: &fuser::Request<'_>, _ino: u64, _fh: u64, _lock: u64, reply: ReplyEmpty) {
        // We delay actual upload until release for simplicity in this implementation.
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self.open_files.lock().remove(&fh);
        if let Some(h) = handle {
            if h.is_writing {
                if let Some(path) = h.write_buffer {
                    info!("File released after writing, triggering upload from {:?}", path);
                    // In a full implementation, we'd trigger a background upload task here.
                    // For now we simulate success and clean up the temp file to avoid leak.
                    let _ = fs::remove_file(path);
                }
            }
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let _parent_id = match self.get_node_id(parent) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        
        let _name_str = name.to_string_lossy();
        
        // This is a simplified mock creation in the engine for the sake of FUSE.
        // The engine normally wouldn't create a file synchronously without upload.
        // We assume we can create an empty file or at least a VirtualNode.
        // However, clouddrive_core doesn't have create_file, just create_folder.
        // This implies writes to new files must just be handled in memory/temp and uploaded later.
        reply.error(ENOSYS); // Simplified: defer to actual implementation specifics
    }

    fn mkdir(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent_id = match self.get_node_id(parent) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let _parent_node = match self.engine.get_node(&parent_id) {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let name_str = name.to_string_lossy();
        
        let create_res = self.runtime.block_on(async {
            self.engine.create_folder(&name_str, &parent_id).await
        });

        match create_res {
            Ok(new_node) => {
                let ino = self.inodes.write().get_or_assign(&new_node.id);
                let attr = self.node_to_attr(&new_node, ino);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => {
                error!("Failed to mkdir: {:?}", e);
                reply.error(EIO);
            }
        }
    }

    fn unlink(&mut self, _req: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_id = match self.get_node_id(parent) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let name_str = name.to_string_lossy();
        let target = match self.engine.lookup_child(&parent_id, &name_str) {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if target.is_workspace_file || target.is_read_only {
            reply.error(EACCES);
            return;
        }
        
        if target.is_dir {
            reply.error(libc::EISDIR);
            return;
        }
        
        let trash_res = self.runtime.block_on(async {
            self.engine.trash_file(&target.id).await
        });

        match trash_res {
            Ok(_) => {
                self.inodes.write().remove(&target.id);
                reply.ok();
            }
            Err(e) => {
                error!("Failed to trash file: {:?}", e);
                reply.error(EIO);
            }
        }
    }

    fn rmdir(&mut self, _req: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_id = match self.get_node_id(parent) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let name_str = name.to_string_lossy();
        let target = match self.engine.lookup_child(&parent_id, &name_str) {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if !target.is_dir {
            reply.error(libc::ENOTDIR);
            return;
        }

        if !target.children_ids.is_empty() {
            reply.error(ENOTEMPTY);
            return;
        }
        
        let trash_res = self.runtime.block_on(async {
            self.engine.trash_file(&target.id).await
        });

        match trash_res {
            Ok(_) => {
                self.inodes.write().remove(&target.id);
                reply.ok();
            }
            Err(e) => {
                error!("Failed to rmdir: {:?}", e);
                reply.error(EIO);
            }
        }
    }

    fn rename(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let parent_id = match self.get_node_id(parent) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let newparent_id = match self.get_node_id(newparent) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let name_str = name.to_string_lossy();
        let target = match self.engine.lookup_child(&parent_id, &name_str) {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let rename_res = self.runtime.block_on(async {
            let mut result = Ok(());
            let new_name_str = newname.to_string_lossy();
            if name_str != new_name_str {
                result = self.engine.rename_file(&target.id, &new_name_str).await;
            }
            if result.is_ok() && parent_id != newparent_id {
                result = self.engine.move_file(&target.id, &newparent_id, &parent_id).await;
            }
            result
        });

        match rename_res {
            Ok(_) => reply.ok(),
            Err(e) => {
                error!("Failed to rename/move file: {:?}", e);
                reply.error(EIO);
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if let Some(uid_val) = uid {
            self.uid = uid_val;
        }
        if let Some(gid_val) = gid {
            self.gid = gid_val;
        }

        let node_id = match self.get_node_id(ino) {
            Some(id) => id,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if let Some(mut node) = self.engine.get_node(&node_id) {
            if let Some(s) = size {
                // If it's 0, it might mean truncation for rewrite. Just simulate size update.
                node.size = s;
            }
            let attr = self.node_to_attr(&node, ino);
            reply.attr(&TTL, &attr);
        } else {
            reply.error(ENOENT);
        }
    }

    fn statfs(&mut self, _req: &fuser::Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let quotas_res = self.runtime.block_on(async {
            self.engine.get_all_quotas().await
        });

        match quotas_res {
            Ok(quotas) => {
                let mut total_limit = 0;
                let mut total_usage = 0;
                for (_, quota) in quotas {
                    total_limit += quota.limit;
                    total_usage += quota.usage_in_drive; // or usage
                }

                let blocks = total_limit / BLOCK_SIZE as u64;
                let bfree = (total_limit.saturating_sub(total_usage)) / BLOCK_SIZE as u64;

                reply.statfs(blocks, bfree, bfree, 0, 0, BLOCK_SIZE, 255, 0);
            }
            Err(_) => reply.statfs(0, 0, 0, 0, 0, BLOCK_SIZE, 255, 0),
        }
    }
}
