//! Per-project SQLite metadata index.
//!
//! Each project gets one `{project_id}.sqlite` database with:
//! - `files` table: metadata (path, size, mode, mtime, tree_hash)
//! - `file_blocks` table: file-to-block mapping (path, offset, hash)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tracing::debug;

use proxygit_common::cdc::ChunkResult;
use proxygit_common::types::{FileEntry, ProjectId};

pub struct ProjectIndexer {
    index_dir: PathBuf,
}

impl ProjectIndexer {
    pub fn new<P: AsRef<Path>>(index_dir: P) -> Result<Self> {
        let path = index_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)?;
        Ok(Self { index_dir: path })
    }

    /// Get or create a connection to a project's SQLite database
    fn get_project_conn(&self, project_id: &ProjectId) -> Result<Connection> {
        let db_path = self.index_dir.join(format!("{project_id}.sqlite"));
        debug!("Opening index: {}", db_path.display());
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                mode INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                tree_hash TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS file_blocks (
                path TEXT NOT NULL,
                block_offset INTEGER NOT NULL,
                block_size INTEGER NOT NULL,
                block_hash TEXT NOT NULL,
                PRIMARY KEY (path, block_offset)
            );",
        )?;

        Ok(conn)
    }

    /// List all files in a project
    pub fn list_files(&self, project_id: &ProjectId) -> Result<Vec<FileEntry>> {
        let conn = self.get_project_conn(project_id)?;
        let mut stmt =
            conn.prepare("SELECT path, size, mode, mtime, tree_hash FROM files ORDER BY path")?;

        let entries = stmt
            .query_map([], |row| {
                Ok(FileEntry {
                    path: row.get(0)?,
                    size: row.get::<_, i64>(1)? as u64,
                    mode: row.get::<_, i32>(2)? as u32,
                    mtime: row.get::<_, i64>(3)? as u64,
                    tree_hash: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Insert or update a file record and its block references inside an atomic transaction
    pub fn ingest_chunks(
        &self,
        project_id: &ProjectId,
        path: &str,
        chunks: &[ChunkResult],
    ) -> Result<()> {
        let mut conn = self.get_project_conn(project_id)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let total_size: i64 = chunks.iter().map(|c| c.length as i64).sum();

        // Content blake3 over concatenated chunk data (matches write-ack / read verify).
        let mut hasher = blake3::Hasher::new();
        for chunk in chunks {
            hasher.update(&chunk.data);
        }
        let tree_hash = hasher.finalize().to_hex().to_string();

        let tx = conn.transaction()?;

        // Upsert file metadata
        tx.execute(
            "INSERT INTO files (path, size, mode, mtime, tree_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
             size=excluded.size, mode=excluded.mode, mtime=excluded.mtime, tree_hash=excluded.tree_hash",
            params![path, total_size, 0o644, now, tree_hash],
        )
        .context("Failed to upsert file record")?;

        // Replace block mappings
        tx.execute("DELETE FROM file_blocks WHERE path = ?1", params![path])?;

        for chunk in chunks {
            tx.execute(
                "INSERT INTO file_blocks (path, block_offset, block_size, block_hash)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    path,
                    chunk.offset as i64,
                    chunk.length as i64,
                    chunk.hash.to_hex().to_string(),
                ],
            )
            .context("Failed to insert block mapping")?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Get block hashes for a file, ordered by offset
    pub fn get_file_blocks(&self, project_id: &ProjectId, path: &str) -> Result<Vec<String>> {
        let conn = self.get_project_conn(project_id)?;
        let mut stmt = conn.prepare(
            "SELECT block_hash FROM file_blocks WHERE path = ?1 ORDER BY block_offset ASC",
        )?;

        let blocks = stmt
            .query_map(params![path], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;

        Ok(blocks)
    }

    /// Get all distinct block hashes referenced by any file in the project
    pub fn get_all_block_hashes(&self, project_id: &ProjectId) -> Result<Vec<String>> {
        let conn = self.get_project_conn(project_id)?;
        let mut stmt = conn.prepare("SELECT DISTINCT block_hash FROM file_blocks")?;

        let hashes = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;

        Ok(hashes)
    }

    /// Get metadata for a single file
    pub fn stat_file(&self, project_id: &ProjectId, path: &str) -> Result<Option<FileEntry>> {
        let conn = self.get_project_conn(project_id)?;
        let mut stmt =
            conn.prepare("SELECT path, size, mode, mtime, tree_hash FROM files WHERE path = ?1")?;

        let mut rows = stmt.query_map(params![path], |row| {
            Ok(FileEntry {
                path: row.get(0)?,
                size: row.get::<_, i64>(1)? as u64,
                mode: row.get::<_, i32>(2)? as u32,
                mtime: row.get::<_, i64>(3)? as u64,
                tree_hash: row.get(4)?,
            })
        })?;

        match rows.next() {
            Some(Ok(entry)) => Ok(Some(entry)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Delete a file from the index
    pub fn delete_file(&self, project_id: &ProjectId, path: &str) -> Result<()> {
        let mut conn = self.get_project_conn(project_id)?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
        tx.execute("DELETE FROM file_blocks WHERE path = ?1", params![path])?;
        tx.commit()?;
        Ok(())
    }
}
