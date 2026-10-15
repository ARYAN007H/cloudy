use crate::error::CloudError;
use crate::types::DriveFile;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct PendingUpload {
    pub id: String,
    pub local_path: String,
    pub session_uri: String,
    pub total_size: u64,
    pub confirmed_bytes: u64,
    pub account_email: String,
    pub remote_parent_id: String,
    pub remote_name: String,
}

pub struct MetadataCache {
    conn: Mutex<Connection>,
}

impl MetadataCache {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, CloudError> {
        let conn = Connection::open(path).map_err(|e| CloudError::Cache(e.to_string()))?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| CloudError::Cache(e.to_string()))?;
        
        conn.execute(
            "CREATE TABLE IF NOT EXISTS files (
                drive_id TEXT,
                account_email TEXT,
                name TEXT NOT NULL,
                parent_id TEXT NOT NULL,
                size INTEGER,
                mime_type TEXT NOT NULL,
                modified_time TEXT NOT NULL,
                md5_checksum TEXT,
                trashed INTEGER NOT NULL,
                is_folder INTEGER NOT NULL,
                thumbnail_link TEXT,
                PRIMARY KEY (drive_id, account_email)
            )",
            [],
        ).map_err(|e| CloudError::Cache(e.to_string()))?;

        // Migration: add thumbnail_link if column doesn't exist yet
        let _ = conn.execute("ALTER TABLE files ADD COLUMN thumbnail_link TEXT", []);

        conn.execute(
            "CREATE TABLE IF NOT EXISTS sync_state (
                account_email TEXT PRIMARY KEY,
                change_page_token TEXT
            )",
            [],
        ).map_err(|e| CloudError::Cache(e.to_string()))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS uploads (
                id TEXT PRIMARY KEY,
                local_path TEXT NOT NULL,
                session_uri TEXT NOT NULL,
                total_size INTEGER NOT NULL,
                confirmed_bytes INTEGER NOT NULL,
                account_email TEXT NOT NULL,
                remote_parent_id TEXT NOT NULL,
                remote_name TEXT NOT NULL,
                created_at TEXT NOT NULL
            )",
            [],
        ).map_err(|e| CloudError::Cache(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert_file(&self, file: &DriveFile, account: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        let parent_id = file.parents.first().map(|s| s.as_str()).unwrap_or("root");
        
        conn.execute(
            "INSERT INTO files (drive_id, account_email, name, parent_id, size, mime_type, modified_time, md5_checksum, trashed, is_folder, thumbnail_link)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(drive_id, account_email) DO UPDATE SET
                name=excluded.name,
                parent_id=excluded.parent_id,
                size=excluded.size,
                mime_type=excluded.mime_type,
                modified_time=excluded.modified_time,
                md5_checksum=excluded.md5_checksum,
                trashed=excluded.trashed,
                is_folder=excluded.is_folder,
                thumbnail_link=excluded.thumbnail_link",
            params![
                file.id,
                account,
                file.name,
                parent_id,
                file.size.map(|s| s as i64),
                file.mime_type,
                file.modified_time,
                file.md5_checksum,
                file.trashed as i32,
                file.is_folder as i32,
                file.thumbnail_link,
            ],
        ).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(())
    }

    pub fn remove_file(&self, drive_id: &str, account: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM files WHERE drive_id = ?1 AND account_email = ?2",
            params![drive_id, account],
        ).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(())
    }

    pub fn get_children(&self, parent_id: &str, account: &str) -> Result<Vec<DriveFile>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT drive_id, name, mime_type, size, modified_time, md5_checksum, trashed, is_folder, parent_id, thumbnail_link FROM files WHERE parent_id = ?1 AND account_email = ?2 AND trashed = 0")
            .map_err(|e| CloudError::Cache(e.to_string()))?;
        
        let files = stmt.query_map(params![parent_id, account], |row| {
            Ok(DriveFile {
                id: row.get(0)?,
                name: row.get(1)?,
                mime_type: row.get(2)?,
                parents: vec![row.get(8)?],
                size: row.get::<_, Option<i64>>(3)?.map(|s| s as u64),
                modified_time: row.get(4)?,
                md5_checksum: row.get(5)?,
                trashed: row.get::<_, i32>(6)? != 0,
                is_folder: row.get::<_, i32>(7)? != 0,
                thumbnail_link: row.get(9)?,
            })
        }).map_err(|e| CloudError::Cache(e.to_string()))?;

        let mut result = Vec::new();
        for file in files {
            result.push(file.map_err(|e| CloudError::Cache(e.to_string()))?);
        }
        Ok(result)
    }

    pub fn get_all_children_pooled(&self, parent_id: &str) -> Result<Vec<(DriveFile, String)>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT drive_id, name, mime_type, size, modified_time, md5_checksum, trashed, is_folder, parent_id, account_email, thumbnail_link FROM files WHERE parent_id = ?1 AND trashed = 0")
            .map_err(|e| CloudError::Cache(e.to_string()))?;
        
        let files = stmt.query_map(params![parent_id], |row| {
            let file = DriveFile {
                id: row.get(0)?,
                name: row.get(1)?,
                mime_type: row.get(2)?,
                parents: vec![row.get(8)?],
                size: row.get::<_, Option<i64>>(3)?.map(|s| s as u64),
                modified_time: row.get(4)?,
                md5_checksum: row.get(5)?,
                trashed: row.get::<_, i32>(6)? != 0,
                is_folder: row.get::<_, i32>(7)? != 0,
                thumbnail_link: row.get(10)?,
            };
            let account: String = row.get(9)?;
            Ok((file, account))
        }).map_err(|e| CloudError::Cache(e.to_string()))?;

        let mut result = Vec::new();
        for file in files {
            result.push(file.map_err(|e| CloudError::Cache(e.to_string()))?);
        }
        Ok(result)
    }

    pub fn get_file(&self, drive_id: &str, account: &str) -> Result<Option<DriveFile>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT drive_id, name, mime_type, size, modified_time, md5_checksum, trashed, is_folder, parent_id, thumbnail_link FROM files WHERE drive_id = ?1 AND account_email = ?2")
            .map_err(|e| CloudError::Cache(e.to_string()))?;
        
        let file = stmt.query_row(params![drive_id, account], |row| {
            Ok(DriveFile {
                id: row.get(0)?,
                name: row.get(1)?,
                mime_type: row.get(2)?,
                parents: vec![row.get(8)?],
                size: row.get::<_, Option<i64>>(3)?.map(|s| s as u64),
                modified_time: row.get(4)?,
                md5_checksum: row.get(5)?,
                trashed: row.get::<_, i32>(6)? != 0,
                is_folder: row.get::<_, i32>(7)? != 0,
                thumbnail_link: row.get(9)?,
            })
        }).optional().map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(file)
    }

    pub fn search(&self, query: &str) -> Result<Vec<(DriveFile, String)>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT drive_id, name, mime_type, size, modified_time, md5_checksum, trashed, is_folder, parent_id, account_email, thumbnail_link FROM files WHERE name LIKE ?1 AND trashed = 0")
            .map_err(|e| CloudError::Cache(e.to_string()))?;
        
        let like_query = format!("%{}%", query);
        let files = stmt.query_map(params![like_query], |row| {
            let file = DriveFile {
                id: row.get(0)?,
                name: row.get(1)?,
                mime_type: row.get(2)?,
                parents: vec![row.get(8)?],
                size: row.get::<_, Option<i64>>(3)?.map(|s| s as u64),
                modified_time: row.get(4)?,
                md5_checksum: row.get(5)?,
                trashed: row.get::<_, i32>(6)? != 0,
                is_folder: row.get::<_, i32>(7)? != 0,
                thumbnail_link: row.get(10)?,
            };
            let account: String = row.get(9)?;
            Ok((file, account))
        }).map_err(|e| CloudError::Cache(e.to_string()))?;

        let mut result = Vec::new();
        for file in files {
            result.push(file.map_err(|e| CloudError::Cache(e.to_string()))?);
        }
        Ok(result)
    }

    pub fn get_all_media_files(&self) -> Result<Vec<(DriveFile, String)>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT drive_id, name, mime_type, size, modified_time, md5_checksum, trashed, is_folder, parent_id, account_email, thumbnail_link FROM files WHERE (mime_type LIKE 'image/%' OR mime_type LIKE 'video/%') AND trashed = 0 AND is_folder = 0 ORDER BY modified_time DESC")
            .map_err(|e| CloudError::Cache(e.to_string()))?;
        
        let files = stmt.query_map([], |row| {
            let file = DriveFile {
                id: row.get(0)?,
                name: row.get(1)?,
                mime_type: row.get(2)?,
                parents: vec![row.get(8)?],
                size: row.get::<_, Option<i64>>(3)?.map(|s| s as u64),
                modified_time: row.get(4)?,
                md5_checksum: row.get(5)?,
                trashed: row.get::<_, i32>(6)? != 0,
                is_folder: row.get::<_, i32>(7)? != 0,
                thumbnail_link: row.get(10)?,
            };
            let account: String = row.get(9)?;
            Ok((file, account))
        }).map_err(|e| CloudError::Cache(e.to_string()))?;

        let mut result = Vec::new();
        for file in files {
            result.push(file.map_err(|e| CloudError::Cache(e.to_string()))?);
        }
        Ok(result)
    }

    pub fn get_all_root_files(&self, account: &str) -> Result<Vec<DriveFile>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT drive_id, name, mime_type, size, modified_time, md5_checksum, trashed, is_folder, parent_id, thumbnail_link FROM files WHERE account_email = ?1 AND trashed = 0 AND parent_id NOT IN (SELECT drive_id FROM files WHERE account_email = ?1)")
            .map_err(|e| CloudError::Cache(e.to_string()))?;
        
        let files = stmt.query_map(params![account], |row| {
            Ok(DriveFile {
                id: row.get(0)?,
                name: row.get(1)?,
                mime_type: row.get(2)?,
                parents: vec![row.get(8)?],
                size: row.get::<_, Option<i64>>(3)?.map(|s| s as u64),
                modified_time: row.get(4)?,
                md5_checksum: row.get(5)?,
                trashed: row.get::<_, i32>(6)? != 0,
                is_folder: row.get::<_, i32>(7)? != 0,
                thumbnail_link: row.get(9)?,
            })
        }).map_err(|e| CloudError::Cache(e.to_string()))?;

        let mut result = Vec::new();
        for file in files {
            result.push(file.map_err(|e| CloudError::Cache(e.to_string()))?);
        }
        Ok(result)
    }
        
    pub fn get_file_count_for_account(&self, account: &str) -> Result<usize, CloudError> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files WHERE account_email = ?1 AND trashed = 0",
            params![account],
            |row| row.get(0),
        ).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(count as usize)
    }

    pub fn clear_account(&self, account: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM files WHERE account_email = ?1", params![account]).map_err(|e| CloudError::Cache(e.to_string()))?;
        conn.execute("DELETE FROM sync_state WHERE account_email = ?1", params![account]).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(())
    }

    pub fn save_change_token(&self, account: &str, token: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_state (account_email, change_page_token) VALUES (?1, ?2) ON CONFLICT(account_email) DO UPDATE SET change_page_token=excluded.change_page_token",
            params![account, token],
        ).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(())
    }

    pub fn get_change_token(&self, account: &str) -> Result<Option<String>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT change_page_token FROM sync_state WHERE account_email = ?1").map_err(|e| CloudError::Cache(e.to_string()))?;
        let token = stmt.query_row(params![account], |row| row.get(0)).optional().map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(token)
    }

    pub fn save_upload_session(&self, id: &str, local_path: &str, session_uri: &str, total_size: u64, account: &str, remote_parent_id: &str, remote_name: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO uploads (id, local_path, session_uri, total_size, confirmed_bytes, account_email, remote_parent_id, remote_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, local_path, session_uri, total_size, 0, account, remote_parent_id, remote_name, chrono::Utc::now().to_rfc3339()],
        ).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(())
    }

    pub fn get_pending_uploads(&self) -> Result<Vec<PendingUpload>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, local_path, session_uri, total_size, confirmed_bytes, account_email, remote_parent_id, remote_name FROM uploads").map_err(|e| CloudError::Cache(e.to_string()))?;
        let uploads = stmt.query_map([], |row| {
            Ok(PendingUpload {
                id: row.get(0)?,
                local_path: row.get(1)?,
                session_uri: row.get(2)?,
                total_size: row.get::<_, i64>(3)? as u64,
                confirmed_bytes: row.get::<_, i64>(4)? as u64,
                account_email: row.get(5)?,
                remote_parent_id: row.get(6)?,
                remote_name: row.get(7)?,
            })
        }).map_err(|e| CloudError::Cache(e.to_string()))?;

        let mut result = Vec::new();
        for upload in uploads {
            result.push(upload.map_err(|e| CloudError::Cache(e.to_string()))?);
        }
        Ok(result)
    }

    pub fn remove_upload_session(&self, id: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM uploads WHERE id = ?1", params![id]).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(())
    }

    pub fn update_upload_progress(&self, id: &str, confirmed_bytes: u64) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute("UPDATE uploads SET confirmed_bytes = ?1 WHERE id = ?2", params![confirmed_bytes as i64, id]).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(())
    }

    pub fn get_file_count(&self) -> Result<usize, CloudError> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0)).map_err(|e| CloudError::Cache(e.to_string()))?;
        Ok(count as usize)
    }
}
