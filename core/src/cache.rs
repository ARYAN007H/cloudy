//! SQLite metadata cache (file index, change tokens, pending uploads).

use crate::error::CloudError;
use crate::types::{Change, DriveFile};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use std::path::Path;

fn db<E: std::fmt::Display>(e: E) -> CloudError {
    CloudError::Cache(e.to_string())
}

#[derive(Debug, Clone)]
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

const COLS: &str =
    "drive_id, name, mime_type, size, modified_time, md5_checksum, trashed, is_folder, parent_id, thumbnail_link";
const COLS_ACCT: &str =
    "drive_id, name, mime_type, size, modified_time, md5_checksum, trashed, is_folder, parent_id, thumbnail_link, account_email";

fn row_to_file(row: &Row<'_>) -> rusqlite::Result<DriveFile> {
    Ok(DriveFile {
        id: row.get(0)?,
        name: row.get(1)?,
        mime_type: row.get(2)?,
        size: row.get::<_, Option<i64>>(3)?.map(|s| s.max(0) as u64),
        modified_time: row.get(4)?,
        md5_checksum: row.get(5)?,
        trashed: row.get::<_, i32>(6)? != 0,
        is_folder: row.get::<_, i32>(7)? != 0,
        parents: vec![row.get(8)?],
        thumbnail_link: row.get(9)?,
        owned_by_me: true,
    })
}

fn row_to_file_acct(row: &Row<'_>) -> rusqlite::Result<(DriveFile, String)> {
    Ok((row_to_file(row)?, row.get(10)?))
}

fn upsert_in(tx: &Transaction<'_>, file: &DriveFile, account: &str) -> rusqlite::Result<()> {
    let parent_id = file.parents.first().map(|s| s.as_str()).unwrap_or("root");
    tx.prepare_cached(
        "INSERT INTO files (drive_id, account_email, name, parent_id, size, mime_type, modified_time,
                            md5_checksum, trashed, is_folder, thumbnail_link)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(drive_id, account_email) DO UPDATE SET
            name=excluded.name, parent_id=excluded.parent_id, size=excluded.size,
            mime_type=excluded.mime_type, modified_time=excluded.modified_time,
            md5_checksum=excluded.md5_checksum, trashed=excluded.trashed,
            is_folder=excluded.is_folder, thumbnail_link=excluded.thumbnail_link",
    )?
    .execute(params![
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
    ])?;
    Ok(())
}

fn like_pattern(q: &str) -> String {
    let esc = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{esc}%")
}

impl MetadataCache {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, CloudError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(db)?;
        }
        let conn = Connection::open(path).map_err(db)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(db)?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(db)?;
        conn.pragma_update(None, "temp_store", "MEMORY").map_err(db)?;
        conn.busy_timeout(std::time::Duration::from_secs(10)).map_err(db)?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                drive_id TEXT NOT NULL,
                account_email TEXT NOT NULL,
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
             );
             CREATE INDEX IF NOT EXISTS idx_files_parent ON files(account_email, parent_id);
             CREATE INDEX IF NOT EXISTS idx_files_mime ON files(mime_type);
             CREATE TABLE IF NOT EXISTS sync_state (
                account_email TEXT PRIMARY KEY,
                change_page_token TEXT
             );
             CREATE TABLE IF NOT EXISTS uploads (
                id TEXT PRIMARY KEY,
                local_path TEXT NOT NULL,
                session_uri TEXT NOT NULL,
                total_size INTEGER NOT NULL,
                confirmed_bytes INTEGER NOT NULL,
                account_email TEXT NOT NULL,
                remote_parent_id TEXT NOT NULL,
                remote_name TEXT NOT NULL,
                created_at TEXT NOT NULL
             );",
        )
        .map_err(db)?;
        // Older databases predate this column.
        let _ = conn.execute("ALTER TABLE files ADD COLUMN thumbnail_link TEXT", []);

        Ok(Self { conn: Mutex::new(conn) })
    }

    // ------------------------------------------------------------- writes

    pub fn upsert_file(&self, file: &DriveFile, account: &str) -> Result<(), CloudError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(db)?;
        upsert_in(&tx, file, account).map_err(db)?;
        tx.commit().map_err(db)
    }

    /// Insert many files in ONE transaction (a commit per file is ~100x slower).
    pub fn upsert_files(&self, account: &str, files: &[DriveFile]) -> Result<(), CloudError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(db)?;
        for f in files {
            upsert_in(&tx, f, account).map_err(db)?;
        }
        tx.commit().map_err(db)
    }

    /// Atomically replace everything known about `account`, and record the
    /// change token the snapshot corresponds to. A failure leaves the old
    /// data untouched.
    pub fn replace_account_files(
        &self,
        account: &str,
        files: &[DriveFile],
        change_token: &str,
    ) -> Result<(), CloudError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("DELETE FROM files WHERE account_email = ?1", params![account]).map_err(db)?;
        for f in files {
            upsert_in(&tx, f, account).map_err(db)?;
        }
        tx.execute(
            "INSERT INTO sync_state (account_email, change_page_token) VALUES (?1, ?2)
             ON CONFLICT(account_email) DO UPDATE SET change_page_token=excluded.change_page_token",
            params![account, change_token],
        )
        .map_err(db)?;
        tx.commit().map_err(db)
    }

    /// Apply a batch of Drive changes and store the new token in one
    /// transaction. Returns how many rows were touched.
    pub fn apply_changes(&self, account: &str, changes: &[Change], new_token: &str) -> Result<usize, CloudError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(db)?;
        let mut touched = 0;
        for c in changes {
            match &c.file {
                // Gone, trashed, or not ours (shared with us) → not part of the pool.
                Some(f) if !c.removed && !f.trashed && f.owned_by_me => {
                    upsert_in(&tx, f, account).map_err(db)?;
                    touched += 1;
                }
                _ => {
                    touched += tx
                        .execute(
                            "DELETE FROM files WHERE drive_id = ?1 AND account_email = ?2",
                            params![c.file_id, account],
                        )
                        .map_err(db)?;
                }
            }
        }
        tx.execute(
            "INSERT INTO sync_state (account_email, change_page_token) VALUES (?1, ?2)
             ON CONFLICT(account_email) DO UPDATE SET change_page_token=excluded.change_page_token",
            params![account, new_token],
        )
        .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(touched)
    }

    pub fn remove_file(&self, drive_id: &str, account: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM files WHERE drive_id = ?1 AND account_email = ?2", params![drive_id, account])
            .map_err(db)?;
        Ok(())
    }

    pub fn clear_account(&self, account: &str) -> Result<(), CloudError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("DELETE FROM files WHERE account_email = ?1", params![account]).map_err(db)?;
        tx.execute("DELETE FROM sync_state WHERE account_email = ?1", params![account]).map_err(db)?;
        tx.execute("DELETE FROM uploads WHERE account_email = ?1", params![account]).map_err(db)?;
        tx.commit().map_err(db)
    }

    // -------------------------------------------------------------- reads

    pub fn get_file(&self, drive_id: &str, account: &str) -> Result<Option<DriveFile>, CloudError> {
        let conn = self.conn.lock();
        conn.query_row(
            &format!("SELECT {COLS} FROM files WHERE drive_id = ?1 AND account_email = ?2"),
            params![drive_id, account],
            row_to_file,
        )
        .optional()
        .map_err(db)
    }

    pub fn get_children(&self, parent_id: &str, account: &str) -> Result<Vec<DriveFile>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {COLS} FROM files WHERE parent_id = ?1 AND account_email = ?2 AND trashed = 0"
            ))
            .map_err(db)?;
        let rows = stmt.query_map(params![parent_id, account], row_to_file).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    /// Every non-trashed file of an account in one query (used to build the tree).
    pub fn all_files_for_account(&self, account: &str) -> Result<Vec<DriveFile>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare_cached(&format!("SELECT {COLS} FROM files WHERE account_email = ?1 AND trashed = 0"))
            .map_err(db)?;
        let rows = stmt.query_map(params![account], row_to_file).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    /// Files whose parent is not itself in the cache → the top level of the account's Drive.
    pub fn get_all_root_files(&self, account: &str) -> Result<Vec<DriveFile>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS} FROM files f WHERE account_email = ?1 AND trashed = 0
                 AND NOT EXISTS (SELECT 1 FROM files p WHERE p.account_email = f.account_email AND p.drive_id = f.parent_id)"
            ))
            .map_err(db)?;
        let rows = stmt.query_map(params![account], row_to_file).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    /// Case-insensitive substring search over all accounts.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(DriveFile, String)>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS_ACCT} FROM files WHERE name LIKE ?1 ESCAPE '\\' AND trashed = 0
                 ORDER BY is_folder DESC, modified_time DESC LIMIT ?2"
            ))
            .map_err(db)?;
        let rows = stmt.query_map(params![like_pattern(query), limit as i64], row_to_file_acct).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    /// Photos & videos, newest first.
    pub fn get_media_files(&self, limit: i64, offset: i64) -> Result<Vec<(DriveFile, String)>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS_ACCT} FROM files
                 WHERE (mime_type LIKE 'image/%' OR mime_type LIKE 'video/%') AND trashed = 0
                 ORDER BY modified_time DESC, drive_id LIMIT ?1 OFFSET ?2"
            ))
            .map_err(db)?;
        let rows = stmt.query_map(params![limit, offset], row_to_file_acct).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    pub fn get_all_media_files(&self) -> Result<Vec<(DriveFile, String)>, CloudError> {
        self.get_media_files(i64::MAX, 0)
    }

    pub fn get_file_count_for_account(&self, account: &str) -> Result<usize, CloudError> {
        let conn = self.conn.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE account_email = ?1 AND trashed = 0",
                params![account],
                |r| r.get(0),
            )
            .map_err(db)?;
        Ok(n as usize)
    }

    pub fn get_file_count(&self) -> Result<usize, CloudError> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).map_err(db)?;
        Ok(n as usize)
    }

    // ------------------------------------------------------- sync tokens

    pub fn save_change_token(&self, account: &str, token: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sync_state (account_email, change_page_token) VALUES (?1, ?2)
             ON CONFLICT(account_email) DO UPDATE SET change_page_token=excluded.change_page_token",
            params![account, token],
        )
        .map_err(db)?;
        Ok(())
    }

    pub fn get_change_token(&self, account: &str) -> Result<Option<String>, CloudError> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT change_page_token FROM sync_state WHERE account_email = ?1",
            params![account],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map(|o| o.flatten())
        .map_err(db)
    }

    pub fn clear_change_token(&self, account: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM sync_state WHERE account_email = ?1", params![account]).map_err(db)?;
        Ok(())
    }

    // ----------------------------------------------------------- uploads

    #[allow(clippy::too_many_arguments)]
    pub fn save_upload_session(
        &self,
        id: &str,
        local_path: &str,
        session_uri: &str,
        total_size: u64,
        account: &str,
        remote_parent_id: &str,
        remote_name: &str,
    ) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO uploads (id, local_path, session_uri, total_size, confirmed_bytes,
                 account_email, remote_parent_id, remote_name, created_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8)",
            params![
                id,
                local_path,
                session_uri,
                total_size as i64,
                account,
                remote_parent_id,
                remote_name,
                chrono::Utc::now().to_rfc3339()
            ],
        )
        .map_err(db)?;
        Ok(())
    }

    pub fn get_pending_uploads(&self) -> Result<Vec<PendingUpload>, CloudError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, local_path, session_uri, total_size, confirmed_bytes, account_email,
                        remote_parent_id, remote_name FROM uploads ORDER BY created_at",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PendingUpload {
                    id: r.get(0)?,
                    local_path: r.get(1)?,
                    session_uri: r.get(2)?,
                    total_size: r.get::<_, i64>(3)? as u64,
                    confirmed_bytes: r.get::<_, i64>(4)? as u64,
                    account_email: r.get(5)?,
                    remote_parent_id: r.get(6)?,
                    remote_name: r.get(7)?,
                })
            })
            .map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    pub fn remove_upload_session(&self, id: &str) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM uploads WHERE id = ?1", params![id]).map_err(db)?;
        Ok(())
    }

    pub fn update_upload_progress(&self, id: &str, confirmed_bytes: u64) -> Result<(), CloudError> {
        let conn = self.conn.lock();
        conn.execute("UPDATE uploads SET confirmed_bytes = ?1 WHERE id = ?2", params![confirmed_bytes as i64, id])
            .map_err(db)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, name: &str, parent: &str, folder: bool) -> DriveFile {
        DriveFile {
            id: id.into(),
            name: name.into(),
            mime_type: if folder { crate::types::FOLDER_MIME.into() } else { "image/jpeg".into() },
            parents: vec![parent.into()],
            size: Some(10),
            modified_time: "2024-01-01T00:00:00Z".into(),
            md5_checksum: None,
            trashed: false,
            is_folder: folder,
            thumbnail_link: None,
            owned_by_me: true,
        }
    }

    fn cache() -> (tempfile::TempDir, MetadataCache) {
        let d = tempfile::tempdir().unwrap();
        let c = MetadataCache::new(d.path().join("m.db")).unwrap();
        (d, c)
    }

    #[test]
    fn replace_is_atomic_and_stores_token() {
        let (_d, c) = cache();
        c.replace_account_files("a@x.com", &[file("1", "one", "ROOT", false)], "T1").unwrap();
        c.replace_account_files("a@x.com", &[file("2", "two", "ROOT", false)], "T2").unwrap();
        assert_eq!(c.get_file_count_for_account("a@x.com").unwrap(), 1);
        assert!(c.get_file("1", "a@x.com").unwrap().is_none());
        assert_eq!(c.get_change_token("a@x.com").unwrap().as_deref(), Some("T2"));
    }

    #[test]
    fn apply_changes_upserts_removes_and_skips_foreign_files() {
        let (_d, c) = cache();
        c.replace_account_files("a@x.com", &[file("1", "one", "ROOT", false), file("2", "two", "ROOT", false)], "T1")
            .unwrap();
        let mut shared = file("9", "shared", "ROOT", false);
        shared.owned_by_me = false;
        let mut trashed = file("2", "two", "ROOT", false);
        trashed.trashed = true;
        let changes = vec![
            Change { file_id: "1".into(), removed: true, file: None },
            Change { file_id: "2".into(), removed: false, file: Some(trashed) },
            Change { file_id: "3".into(), removed: false, file: Some(file("3", "three", "ROOT", false)) },
            Change { file_id: "9".into(), removed: false, file: Some(shared) },
        ];
        c.apply_changes("a@x.com", &changes, "T2").unwrap();
        let ids: Vec<String> = c.all_files_for_account("a@x.com").unwrap().into_iter().map(|f| f.id).collect();
        assert_eq!(ids, vec!["3"]);
        assert_eq!(c.get_change_token("a@x.com").unwrap().as_deref(), Some("T2"));
    }

    #[test]
    fn root_detection_and_search_escaping() {
        let (_d, c) = cache();
        let files = vec![
            file("f", "Holiday_100%", "ROOT", true),
            file("p", "a.jpg", "f", false),
            file("q", "b.jpg", "ROOT", false),
        ];
        c.upsert_files("a@x.com", &files).unwrap();
        let mut roots: Vec<String> = c.get_all_root_files("a@x.com").unwrap().into_iter().map(|f| f.id).collect();
        roots.sort();
        assert_eq!(roots, vec!["f", "q"]);
        // `%` and `_` are literals, not wildcards
        assert_eq!(c.search("100%", 10).unwrap().len(), 1);
        assert_eq!(c.search("_", 10).unwrap().len(), 1);
        assert_eq!(c.search("%", 10).unwrap().len(), 1);
    }

    #[test]
    fn bulk_insert_is_fast() {
        let (_d, c) = cache();
        let files: Vec<DriveFile> = (0..20_000).map(|i| file(&format!("id{i}"), &format!("f{i}"), "ROOT", false)).collect();
        let t = std::time::Instant::now();
        c.upsert_files("a@x.com", &files).unwrap();
        assert!(t.elapsed().as_secs() < 5, "20k rows took {:?}", t.elapsed());
        assert_eq!(c.get_file_count().unwrap(), 20_000);
    }
}
