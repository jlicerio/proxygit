//! Local NVMe Write-Ahead Log (WAL)
//!
//! Writes hit the WAL first (<0.5ms), then a background worker performs
//! atomic rotation, pending stage recovery, patch replay over base content,
//! FastCDC chunking, BLAKE3 hashing, and async QUIC flush to the server.
//!
//! WAL record format (CRC32C-framed):
//! [magic:4B][seq:8B][path_len:2B][path][offset:8B][data_len:4B][data][crc32c:4B][magic_end:4B]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{info, warn};

use proxygit_common::protocol::{
    encode_sparse_write, recv_frame, send_frame, SparseChunk, MAX_PAYLOAD_SIZE, MSG_ERROR,
    MSG_WRITE_ACK, MSG_WRITE_BLOCKS_SPARSE,
};
use proxygit_common::types::ProjectId;

/// Magic constants for CRC32C-framed WAL records
const MAGIC_START: u32 = u32::from_le_bytes(*b"PGWL");
const MAGIC_END: u32 = u32::from_le_bytes(*b"wEND");

/// Minimum frame size: magic(4) + seq(8) + path_len(2) + path(0) + offset(8)
/// + data_len(4) + data(0) + crc32c(4) + magic_end(4) = 34 bytes
const MIN_FRAME_SIZE: usize = 34;

#[derive(Debug, Clone)]
pub struct WalRecord {
    pub seq: u64,
    pub path: String,
    pub offset: u64,
    pub data: Vec<u8>,
}

/// A sequential Write-Ahead Log stored on local NVMe.
pub struct LocalWal {
    wal_dir: PathBuf,
    active_journal: Arc<Mutex<File>>,
    next_seq: AtomicU64,
}

impl LocalWal {
    pub fn new<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let wal_dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&wal_dir)?;

        let journal_path = wal_dir.join("current_journal.wal");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&journal_path)?;

        Ok(Self {
            wal_dir,
            active_journal: Arc::new(Mutex::new(file)),
            next_seq: AtomicU64::new(0),
        })
    }

    /// Append a write operation to the WAL using the CRC32C-framed format.
    pub async fn append_entry(&self, file_path: &str, offset: u64, data: &[u8]) -> Result<()> {
        let mut journal = self.active_journal.lock().await;
        let path_bytes = file_path.as_bytes();
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);

        // Compute CRC32C over payload: seq, path_len, path, offset, data_len, data
        let mut crc = crc32fast::Hasher::new();
        crc.update(&seq.to_le_bytes());
        crc.update(&(path_bytes.len() as u16).to_le_bytes());
        crc.update(path_bytes);
        crc.update(&offset.to_le_bytes());
        crc.update(&(data.len() as u32).to_le_bytes());
        crc.update(data);
        let crc_val = crc.finalize();

        journal.write_all(&MAGIC_START.to_le_bytes())?;
        journal.write_all(&seq.to_le_bytes())?;
        journal.write_all(&(path_bytes.len() as u16).to_le_bytes())?;
        journal.write_all(path_bytes)?;
        journal.write_all(&offset.to_le_bytes())?;
        journal.write_all(&(data.len() as u32).to_le_bytes())?;
        journal.write_all(data)?;
        journal.write_all(&crc_val.to_le_bytes())?;
        journal.write_all(&MAGIC_END.to_le_bytes())?;
        // user-space buffer flush; fsync deferred to rotate_for_flush
        journal.flush()?;
        Ok(())
    }

    /// Atomically rotate `current_journal.wal` to a stage file for flushing
    pub async fn rotate_for_flush(&self) -> Result<Option<PathBuf>> {
        let mut journal = self.active_journal.lock().await;
        let journal_path = self.wal_dir.join("current_journal.wal");

        let _metadata = match std::fs::metadata(&journal_path) {
            Ok(m) if m.len() > 0 => m,
            _ => return Ok(None),
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let stage_path = self.wal_dir.join(format!("stage_{now}.wal"));

        journal.flush()?;
        std::fs::rename(&journal_path, &stage_path)?;

        // Fsync the rotated stage file to make buffered data durable on disk
        {
            let stage_file =
                std::fs::File::open(&stage_path).context("failed to open rotated stage file")?;
            stage_file
                .sync_all()
                .context("failed to fsync rotated stage file")?;
        }
        // Fsync parent directory to ensure rename + stage data are durable
        if let Some(parent) = stage_path.parent() {
            let parent_f =
                std::fs::File::open(parent).context("failed to open WAL dir for fsync")?;
            parent_f
                .sync_all()
                .context("failed to fsync WAL parent directory")?;
        }

        *journal = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&journal_path)?;

        Ok(Some(stage_path))
    }

    /// Read records from a specific staged WAL file.
    ///
    /// Scans forward for valid CRC32C-framed records. On CRC mismatch or
    /// truncated tail, returns `Ok` with all valid leading records — the
    /// tail is silently dropped.
    pub fn read_staged_records(stage_path: &Path) -> Result<Vec<WalRecord>> {
        if !stage_path.exists() {
            return Ok(Vec::new());
        }

        let data = match std::fs::read(stage_path) {
            Ok(d) if !d.is_empty() => d,
            _ => return Ok(Vec::new()),
        };

        let mut records = Vec::new();
        let mut pos = 0;
        let magic_start_bytes = MAGIC_START.to_le_bytes();
        let len = data.len();

        while pos + MIN_FRAME_SIZE <= len {
            // Scan forward for MAGIC_START
            if data[pos..pos + 4] != magic_start_bytes {
                pos += 1;
                continue;
            }

            let frame_start = pos;
            pos += 4; // skip magic_start

            // seq: 8 bytes
            if pos + 8 > len {
                break;
            }
            let seq = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            pos += 8;

            // path_len: 2 bytes
            if pos + 2 > len {
                break;
            }
            let path_len = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;

            if path_len > 4096 {
                pos = frame_start + 1;
                continue;
            }

            // path: path_len bytes
            if pos + path_len > len {
                break;
            }
            let path_bytes = &data[pos..pos + path_len];
            pos += path_len;

            // offset: 8 bytes
            if pos + 8 > len {
                break;
            }
            let offset = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            pos += 8;

            // data_len: 4 bytes
            if pos + 4 > len {
                break;
            }
            let data_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;

            if data_len > MAX_PAYLOAD_SIZE {
                pos = frame_start + 1;
                continue;
            }

            // data: data_len bytes
            if pos + data_len > len {
                break;
            }
            let payload_data = &data[pos..pos + data_len];
            pos += data_len;

            // crc32c: 4 bytes
            if pos + 4 > len {
                break;
            }
            let stored_crc = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
            pos += 4;

            // magic_end: 4 bytes
            if pos + 4 > len {
                break;
            }
            let magic_end = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
            pos += 4;

            if magic_end != MAGIC_END {
                pos = frame_start + 1;
                continue;
            }

            // Verify CRC32C over payload fields
            let mut crc = crc32fast::Hasher::new();
            crc.update(&seq.to_le_bytes());
            crc.update(&(path_len as u16).to_le_bytes());
            crc.update(path_bytes);
            crc.update(&offset.to_le_bytes());
            crc.update(&(data_len as u32).to_le_bytes());
            crc.update(payload_data);
            let computed_crc = crc.finalize();

            if computed_crc != stored_crc {
                pos = frame_start + 1;
                continue;
            }

            let path = match String::from_utf8(path_bytes.to_vec()) {
                Ok(p) => p,
                Err(_) => {
                    pos = frame_start + 1;
                    continue;
                }
            };

            records.push(WalRecord {
                seq,
                path,
                offset,
                data: payload_data.to_vec(),
            });
        }

        Ok(records)
    }

    /// Atomically write a list of WalRecords back to a stage file
    /// using the CRC32C-framed format.
    pub fn write_staged_records(stage_path: &Path, records: &[WalRecord]) -> Result<()> {
        if records.is_empty() {
            let _ = std::fs::remove_file(stage_path);
            return Ok(());
        }
        let temp_path = stage_path.with_extension("tmp");
        let mut file = File::create(&temp_path)?;
        for r in records {
            let path_bytes = r.path.as_bytes();

            let mut crc = crc32fast::Hasher::new();
            crc.update(&r.seq.to_le_bytes());
            crc.update(&(path_bytes.len() as u16).to_le_bytes());
            crc.update(path_bytes);
            crc.update(&r.offset.to_le_bytes());
            crc.update(&(r.data.len() as u32).to_le_bytes());
            crc.update(&r.data);
            let crc_val = crc.finalize();

            file.write_all(&MAGIC_START.to_le_bytes())?;
            file.write_all(&r.seq.to_le_bytes())?;
            file.write_all(&(path_bytes.len() as u16).to_le_bytes())?;
            file.write_all(path_bytes)?;
            file.write_all(&r.offset.to_le_bytes())?;
            file.write_all(&(r.data.len() as u32).to_le_bytes())?;
            file.write_all(&r.data)?;
            file.write_all(&crc_val.to_le_bytes())?;
            file.write_all(&MAGIC_END.to_le_bytes())?;
        }
        file.flush()?;
        file.sync_data().context("failed to sync WAL")?;
        std::fs::rename(temp_path, stage_path)?;
        Ok(())
    }

    /// List all pending `stage_*.wal` files sorted chronologically
    pub fn get_pending_stages(&self) -> Vec<PathBuf> {
        let mut stages = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.wal_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("stage_") && name.ends_with(".wal") {
                        stages.push(path);
                    }
                }
            }
        }
        stages.sort();
        stages
    }
}

/// Split data into fixed-size blocks, compute BLAKE3 hashes,
/// and build SparseChunks — only include data for blocks whose
/// hash differs from the old version.
fn build_sparse_diff(old: &[u8], new: &[u8], block_size: usize) -> Vec<SparseChunk> {
    let old_hashes: Vec<[u8; 32]> = old
        .chunks(block_size)
        .map(|c| *blake3::hash(c).as_bytes())
        .collect();

    new.chunks(block_size)
        .enumerate()
        .map(|(i, data)| {
            let hash = *blake3::hash(data).as_bytes();
            let is_unchanged = i < old_hashes.len() && old_hashes[i] == hash;
            SparseChunk {
                hash,
                data: if is_unchanged {
                    Vec::new()
                } else {
                    data.to_vec()
                },
            }
        })
        .collect()
}

/// Background worker that periodically flushes WAL records over QUIC to the server.
pub fn start_wal_flush_worker(
    wal: Arc<LocalWal>,
    conn: quinn::Connection,
    project_id: ProjectId,
    cache_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let pool = crate::StreamPool::new(conn, 4);
        pool.prewarm().await;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
        loop {
            interval.tick().await;

            // 1. Rotate current journal if it contains writes
            let _ = wal.rotate_for_flush().await;

            // 2. Fetch all pending stage files (oldest first)
            let pending_stages = wal.get_pending_stages();
            if pending_stages.is_empty() {
                continue;
            }

            for stage_path in pending_stages {
                // Ensure all data is on disk before processing (covers crash-recovery scenarios)
                if let Ok(f) = std::fs::File::open(&stage_path) {
                    if let Err(e) = f.sync_all() {
                        tracing::error!("Failed to fsync WAL stage {}: {e}", stage_path.display());
                    }
                }

                let records = match LocalWal::read_staged_records(&stage_path) {
                    Ok(r) if r.is_empty() => {
                        let _ = std::fs::remove_file(&stage_path);
                        continue;
                    }
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("Failed to read WAL stage {}: {e}", stage_path.display());
                        let corrupt_path = stage_path.with_extension("corrupt");
                        let _ = std::fs::rename(&stage_path, &corrupt_path);
                        continue;
                    }
                };

                let mut path_map: std::collections::HashMap<String, Vec<WalRecord>> =
                    std::collections::HashMap::new();
                for r in &records {
                    path_map.entry(r.path.clone()).or_default().push(r.clone());
                }

                let mut remaining_records = Vec::new();

                for (path, patches) in path_map {
                    // Fetch base content from server (empty if new file)
                    let base = match crate::mcp_read_raw_bytes(&pool, project_id, &path).await {
                        Ok(b) => b,
                        Err(e) => {
                            let err_msg = e.to_string();
                            if err_msg.contains("File not found") || err_msg.contains("not found") {
                                // New file — proceed with empty base
                                Vec::new()
                            } else {
                                warn!(
                                    "WAL flush: failed to fetch base for {path}: {e} — deferring"
                                );
                                remaining_records.extend(patches);
                                continue;
                            }
                        }
                    };

                    let mut content = base.clone();
                    for patch in &patches {
                        let end = patch.offset as usize + patch.data.len();
                        if end > content.len() {
                            content.resize(end, 0);
                        }
                        content[patch.offset as usize..end].copy_from_slice(&patch.data);
                    }

                    // Use fixed-size block diff for sparse write.
                    // Instead of CDC (which shifts all chunk boundaries for a 1KB edit),
                    // use 64KB fixed blocks and only send changed blocks' data.
                    let sparse_chunks = build_sparse_diff(&base, &content, 65536);
                    let payload = encode_sparse_write(&path, &sparse_chunks);
                    let hash = blake3::hash(&payload).into();

                    let res = match pool.borrow().await {
                        Ok((mut send, mut recv)) => {
                            if let Err(e) = send_frame(
                                &mut send,
                                MSG_WRITE_BLOCKS_SPARSE,
                                project_id.as_u128(),
                                &hash,
                                &payload,
                            )
                            .await
                            {
                                serde_json::json!({ "error": format!("send error: {e}") })
                            } else {
                                match recv_frame(&mut recv).await {
                                    Ok(resp) if resp.msg_type == MSG_WRITE_ACK => {
                                        serde_json::json!({ "status": "ok", "tree_hash": String::from_utf8_lossy(&resp.payload).to_string() })
                                    }
                                    Ok(resp) if resp.msg_type == MSG_ERROR => {
                                        serde_json::json!({ "error": String::from_utf8_lossy(&resp.payload).to_string() })
                                    }
                                    Ok(_) => serde_json::json!({ "error": "unexpected response" }),
                                    Err(e) => {
                                        serde_json::json!({ "error": format!("recv error: {e}") })
                                    }
                                }
                            }
                        }
                        Err(e) => serde_json::json!({ "error": format!("pool error: {e}") }),
                    };
                    if res["status"] == "ok" {
                        // Invalidate local read cache on successful flush
                        let cache_path =
                            cache_dir.join(blake3::hash(path.as_bytes()).to_hex().as_str());
                        let _ = std::fs::remove_file(&cache_path);
                    } else {
                        warn!("WAL flush failed for path {path}: {:?}", res);
                        // Retain failed path patches for retry
                        remaining_records.extend(patches);
                    }
                }

                if remaining_records.is_empty() {
                    let _ = std::fs::remove_file(&stage_path);
                    info!(
                        "WAL stage file {} successfully flushed and deleted",
                        stage_path.display()
                    );
                } else {
                    if let Err(e) = LocalWal::write_staged_records(&stage_path, &remaining_records)
                    {
                        tracing::error!(
                            "Failed to rewrite WAL stage {}: {e}",
                            stage_path.display()
                        );
                    }
                    // Stop processing newer stages for this tick to preserve chronological ordering!
                    break;
                }
            }
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_sparse_diff_identical_content() {
        let data = vec![0u8; 65536 * 4];
        let chunks = build_sparse_diff(&data, &data, 65536);
        assert_eq!(chunks.len(), 4);
        for chunk in &chunks {
            assert!(chunk.data.is_empty());
        }
    }

    #[test]
    fn test_build_sparse_diff_small_edit() {
        let old = vec![b'A'; 65536 * 16];
        let mut new = old.clone();
        new[65536 * 8 + 1000..65536 * 8 + 2024].fill(b'Z');
        let chunks = build_sparse_diff(&old, &new, 65536);
        let changed: Vec<_> = chunks.iter().filter(|c| !c.data.is_empty()).collect();
        assert!(
            changed.len() <= 2,
            "at most 2 blocks, got {}",
            changed.len()
        );
    }

    #[test]
    fn test_build_sparse_diff_new_file() {
        let old = vec![];
        let new = vec![b'B'; 65536 * 2];
        let chunks = build_sparse_diff(&old, &new, 65536);
        assert_eq!(chunks.len(), 2);
        for chunk in &chunks {
            assert!(!chunk.data.is_empty());
        }
    }

    #[test]
    fn test_build_sparse_diff_tail_block() {
        let old = vec![b'C'; 70000];
        let mut new = old.clone();
        new[65500] = b'D';
        let chunks = build_sparse_diff(&old, &new, 65536);
        let changed: Vec<_> = chunks.iter().filter(|c| !c.data.is_empty()).collect();
        assert_eq!(changed.len(), 1);
    }

    #[test]
    fn test_read_staged_records_partial_tail_returns_ok() {
        let temp_dir = tempfile::tempdir().unwrap();
        let wal = LocalWal::new(temp_dir.path()).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            wal.append_entry("src/a.rs", 0, b"content").await.unwrap();
        });

        let rotated = rt.block_on(async { wal.rotate_for_flush().await.unwrap().unwrap() });

        // Append a 1-byte partial fragment at the end
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rotated)
            .unwrap();
        f.write_all(&[0x05]).unwrap();
        f.flush().unwrap();

        // Must return Ok with 1 valid leading record (truncated tail silently dropped)
        let result = LocalWal::read_staged_records(&rotated);
        assert!(result.is_ok());
        let records = result.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "src/a.rs");
        assert_eq!(records[0].offset, 0);
        assert_eq!(records[0].data, b"content");
    }

    #[test]
    fn test_read_staged_records_truncated_payload_returns_ok_empty() {
        let temp_dir = tempfile::tempdir().unwrap();
        let stage_path = temp_dir.path().join("stage_truncated_payload.wal");

        use std::io::Write;
        let mut f = std::fs::File::create(&stage_path).unwrap();
        // Write a fragment that doesn't form a valid frame (no MAGIC_START)
        f.write_all(&[0x00; 10]).unwrap();
        f.flush().unwrap();

        // Must return Ok with empty vec (no valid leading records)
        let result = LocalWal::read_staged_records(&stage_path);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_read_staged_records_crc_mismatch_skips_corrupt_record() {
        let temp_dir = tempfile::tempdir().unwrap();
        let wal = LocalWal::new(temp_dir.path()).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            wal.append_entry("good.rs", 10, b"hello").await.unwrap();
        });

        let rotated = rt.block_on(async { wal.rotate_for_flush().await.unwrap().unwrap() });

        // Read the file, corrupt one byte in the middle
        let mut data = std::fs::read(&rotated).unwrap();
        if data.len() > 20 {
            data[15] ^= 0xFF; // flip bits in payload area
        }
        std::fs::write(&rotated, &data).unwrap();

        // CRC mismatch should cause the record to be skipped
        let result = LocalWal::read_staged_records(&rotated);
        assert!(result.is_ok());
        // Data corrupted so no valid record found
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_append_and_read_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let wal = LocalWal::new(temp_dir.path()).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            wal.append_entry("a.rs", 0, b"alpha").await.unwrap();
            wal.append_entry("b.rs", 100, b"beta").await.unwrap();
        });

        let rotated = rt.block_on(async { wal.rotate_for_flush().await.unwrap().unwrap() });
        let records = LocalWal::read_staged_records(&rotated).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 0);
        assert_eq!(records[0].path, "a.rs");
        assert_eq!(records[0].offset, 0);
        assert_eq!(records[0].data, b"alpha");
        assert_eq!(records[1].seq, 1);
        assert_eq!(records[1].path, "b.rs");
        assert_eq!(records[1].offset, 100);
        assert_eq!(records[1].data, b"beta");
    }

    #[test]
    fn test_write_staged_records_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let stage_path = temp_dir.path().join("stage_roundtrip.wal");

        let records_in = vec![
            WalRecord {
                seq: 42,
                path: "x.rs".into(),
                offset: 0,
                data: b"hello world".to_vec(),
            },
            WalRecord {
                seq: 99,
                path: "y.rs".into(),
                offset: 50,
                data: b"goodbye".to_vec(),
            },
        ];

        LocalWal::write_staged_records(&stage_path, &records_in).unwrap();
        assert!(stage_path.exists());

        let records_out = LocalWal::read_staged_records(&stage_path).unwrap();
        assert_eq!(records_out.len(), 2);
        assert_eq!(records_out[0].seq, 42);
        assert_eq!(records_out[0].path, "x.rs");
        assert_eq!(records_out[0].data, b"hello world");
        assert_eq!(records_out[1].seq, 99);
        assert_eq!(records_out[1].path, "y.rs");
        assert_eq!(records_out[1].data, b"goodbye");
    }
}
