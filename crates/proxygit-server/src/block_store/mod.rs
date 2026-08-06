//! Content-addressed block storage on local filesystem.
//!
//! Blocks are stored at `{blocks_dir}/{hash_prefix}/{full_hash}`.
//! For MVP this uses local FS; in production this delegates to Garage S3.
//!
//! Durability contract:
//! - Data is `fsync`'d before a block is considered stored.
//! - Parent directories are `fsync`'d after renames so the name is durable.
//! - Batch APIs amortize parent-dir fsyncs across many blocks (F1).

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::debug;
use uuid::Uuid;

use proxygit_common::cdc::ChunkResult;
use proxygit_common::types::ProjectId;

pub struct BlockStore {
    blocks_dir: PathBuf,
}

impl BlockStore {
    pub fn new<P: AsRef<Path>>(blocks_dir: P) -> Result<Self> {
        let blocks_dir = blocks_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&blocks_dir)
            .with_context(|| format!("create blocks dir {}", blocks_dir.display()))?;
        std::fs::create_dir_all(blocks_dir.join("staging")).context("create blocks staging dir")?;
        Ok(Self { blocks_dir })
    }

    /// Hash-to-path: first 2 hex chars = prefix dir for sharding
    fn block_path(&self, hash: &[u8; 32]) -> PathBuf {
        let hex_hash = hex::encode(hash);
        let prefix = &hex_hash[0..2];
        self.blocks_dir.join(prefix).join(hex_hash)
    }

    fn hex_to_block_path(&self, hex_hash: &str) -> PathBuf {
        let prefix = if hex_hash.len() >= 2 {
            &hex_hash[0..2]
        } else {
            "00"
        };
        self.blocks_dir.join(prefix).join(hex_hash)
    }

    /// Store a single block (thin wrapper over [`store_blocks`]).
    pub fn store_block(&self, hash_bytes: &[u8; 32], data: &[u8]) -> Result<()> {
        self.store_blocks(&[(*hash_bytes, data)])
    }

    /// Batch-store content-addressed blocks with amortized fsync.
    ///
    /// 1. Skip hashes already present.
    /// 2. Write new blocks into a unique `staging/<uuid>/` dir.
    /// 3. `fsync` each staging file (data durable).
    /// 4. Atomic rename into `blocks/{prefix}/{hash}`.
    /// 5. `fsync` each unique parent dir touched by renames (names durable).
    ///
    /// Empty `blocks` is a no-op. Fsync errors are fatal.
    pub fn store_blocks(&self, blocks: &[([u8; 32], &[u8])]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }

        let mut pending: Vec<([u8; 32], &[u8])> = Vec::with_capacity(blocks.len());
        for (hash, data) in blocks {
            if !self.has_block(hash) {
                pending.push((*hash, *data));
            }
        }
        if pending.is_empty() {
            return Ok(());
        }

        let staging_id = Uuid::new_v4().to_string();
        let staging_dir = self.blocks_dir.join("staging").join(&staging_id);
        std::fs::create_dir_all(&staging_dir)
            .context("Failed to create block staging directory")?;

        let mut targets: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(pending.len());
        for (hash_bytes, data) in &pending {
            let hex_hash = hex::encode(hash_bytes);
            let staging_path = staging_dir.join(&hex_hash);
            {
                let mut f = std::fs::File::create(&staging_path)
                    .context("Failed to create staging block file")?;
                f.write_all(data)
                    .context("Failed to write staging block file")?;
                // File data must be durable before rename — directory fsync alone is not enough.
                f.sync_all().context("Failed to fsync staging block file")?;
            }
            let target = self.block_path(hash_bytes);
            targets.push((staging_path, target));
        }

        let mut parents: HashSet<PathBuf> = HashSet::new();
        for (staging_path, target) in &targets {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create block prefix dir {}", parent.display()))?;
                parents.insert(parent.to_path_buf());
            }
            std::fs::rename(staging_path, target)
                .with_context(|| format!("Failed to rename block to {}", target.display()))?;
        }

        // Parent dir fsync makes the new directory entries durable.
        for parent in &parents {
            let parent_f = std::fs::File::open(parent)
                .with_context(|| format!("open block parent {} for fsync", parent.display()))?;
            parent_f
                .sync_all()
                .with_context(|| format!("fsync block parent {}", parent.display()))?;
        }

        let _ = std::fs::remove_dir_all(&staging_dir);

        debug!(
            "Stored {} blocks via staged batch ({})",
            pending.len(),
            staging_id
        );
        Ok(())
    }

    /// Store a batch of CDC chunks (legacy full-file ingest path).
    pub fn store_file(
        &self,
        _project_id: &ProjectId,
        _path: &str,
        chunks: &[ChunkResult],
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let owned: Vec<([u8; 32], Vec<u8>)> = chunks
            .iter()
            .map(|c| {
                let h: [u8; 32] = c.hash.into();
                (h, c.data.clone())
            })
            .collect();
        let refs: Vec<([u8; 32], &[u8])> = owned.iter().map(|(h, d)| (*h, d.as_slice())).collect();
        self.store_blocks(&refs)
    }

    /// Read a file by assembling its blocks given an ordered list of block hex hashes
    pub fn read_blocks(&self, block_hex_hashes: &[String]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for h in block_hex_hashes {
            let path = self.hex_to_block_path(h);
            let data =
                std::fs::read(&path).with_context(|| format!("read block {}", path.display()))?;
            out.extend_from_slice(&data);
        }
        Ok(out)
    }

    /// Get a block by its BLAKE3 hash
    pub fn get_block(&self, hash: &[u8; 32]) -> Option<Vec<u8>> {
        std::fs::read(self.block_path(hash)).ok()
    }

    /// Check if a block exists locally
    pub fn has_block(&self, hash: &[u8; 32]) -> bool {
        self.block_path(hash).exists()
    }

    /// Garbage-collect orphaned blocks.
    ///
    /// Lists all physical block files, compares against the set of hashes
    /// referenced by any indexed file, and deletes unreferenced ones.
    /// Returns the number of blocks deleted.
    pub fn gc_orphans(&self, referenced_hashes: &std::collections::HashSet<String>) -> Result<u64> {
        let mut deleted = 0u64;
        let entries = match std::fs::read_dir(&self.blocks_dir) {
            Ok(e) => e,
            Err(_) => return Ok(0),
        };
        for ent in entries.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            // Only look at 2-hex-char prefix directories (skip staging/, etc.)
            if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let prefix_dir = ent.path();
            let ok = match std::fs::read_dir(&prefix_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for file in ok.flatten() {
                let fname = file.file_name();
                let hex_hash = fname.to_string_lossy().to_string();
                if hex_hash.len() != 64 {
                    continue;
                }
                if !referenced_hashes.contains(&hex_hash) {
                    if std::fs::remove_file(file.path()).is_ok() {
                        deleted += 1;
                    }
                }
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_blocks_batch_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let bs = BlockStore::new(dir.path()).unwrap();
        let h1 = *blake3::hash(b"alpha").as_bytes();
        let h2 = *blake3::hash(b"beta").as_bytes();
        bs.store_blocks(&[(h1, b"alpha" as &[u8]), (h2, b"beta" as &[u8])])
            .unwrap();
        assert_eq!(bs.get_block(&h1).unwrap(), b"alpha");
        assert_eq!(bs.get_block(&h2).unwrap(), b"beta");
        // Idempotent re-store
        bs.store_blocks(&[(h1, b"alpha" as &[u8])]).unwrap();
        assert!(bs.has_block(&h1));
    }

    #[test]
    fn store_file_uses_batch_path() {
        let dir = tempfile::tempdir().unwrap();
        let bs = BlockStore::new(dir.path()).unwrap();
        let h = blake3::hash(b"chunk");
        let chunks = vec![ChunkResult {
            offset: 0,
            length: 5,
            hash: h,
            data: b"chunk".to_vec(),
        }];
        let pid = uuid::Uuid::from_u128(1);
        bs.store_file(&pid, "a.txt", &chunks).unwrap();
        assert!(bs.has_block(h.as_bytes()));
    }
}
