//! Content-addressed block storage on local filesystem.
//!
//! Blocks are stored at `{blocks_dir}/{hash_prefix}/{full_hash}`.
//! For MVP this uses local FS; in production this delegates to Garage S3.

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
        let path = blocks_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)?;
        Ok(Self { blocks_dir: path })
    }

    /// Hash-to-path: first 2 hex chars = prefix dir for sharding
    fn block_path(&self, hash: &[u8; 32]) -> PathBuf {
        let hex = hex::encode(hash);
        let prefix = &hex[0..2];
        self.blocks_dir.join(prefix).join(&hex)
    }

    fn hex_to_block_path(&self, hex_hash: &str) -> PathBuf {
        let prefix = if hex_hash.len() >= 2 {
            &hex_hash[0..2]
        } else {
            "00"
        };
        self.blocks_dir.join(prefix).join(hex_hash)
    }

    /// Store a raw block by its content hash
    pub fn store_block(&self, hash_bytes: &[u8; 32], data: &[u8]) -> Result<()> {
        let dest = self.block_path(hash_bytes);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp).context("Failed to create block temp file")?;
            f.write_all(data)
                .context("Failed to write block temp file")?;
            f.sync_all().context("Failed to fsync block temp file")?;
        }
        std::fs::rename(&tmp, &dest).context("Failed to rename block file")?;
        // fsync parent directory to ensure rename is durable
        if let Some(parent) = dest.parent() {
            if let Ok(parent_f) = std::fs::File::open(parent) {
                parent_f
                    .sync_all()
                    .context("failed to sync parent dir after rename")?;
            }
        }
        debug!("Stored block {}", hex::encode(hash_bytes));
        Ok(())
    }

    /// Store a batch of blocks using staged writes:
    /// 1. Write all blocks to a unique staging/ subdirectory (no per-block fsync)
    /// 2. Single `sync_all()` on the staging directory
    /// 3. Atomic renames into `blocks/{prefix}/`
    /// 4. Single `sync_all()` on the blocks parent directory
    ///
    /// Expected: 2 fsyncs per batch (vs 2 fsyncs per block previously).
    pub fn store_file(
        &self,
        _project_id: &ProjectId,
        _path: &str,
        chunks: &[ChunkResult],
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }

        // 1. Unique staging directory for this batch
        let staging_id = Uuid::new_v4().to_string();
        let staging_dir = self.blocks_dir.join("staging").join(&staging_id);
        std::fs::create_dir_all(&staging_dir)
            .context("Failed to create block staging directory")?;

        // 2. Write all blocks to staging (no per-block fsync)
        let mut targets: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let hash_bytes: [u8; 32] = chunk.hash.into();
            let hex_hash = hex::encode(hash_bytes);
            let staging_path = staging_dir.join(&hex_hash);
            {
                let mut f = std::fs::File::create(&staging_path)
                    .context("Failed to create staging block file")?;
                f.write_all(&chunk.data)
                    .context("Failed to write staging block file")?;
            }
            let prefix = &hex_hash[0..2];
            let target = self.blocks_dir.join(prefix).join(&hex_hash);
            targets.push((staging_path, target));
        }

        // 3. Single sync of staging directory (all block data now durable)
        {
            let staging_f = std::fs::File::open(&staging_dir)
                .context("Failed to open staging dir for fsync")?;
            staging_f
                .sync_all()
                .context("Failed to sync block staging dir")?;
        }

        // 4. Atomic renames into blocks/{prefix}/
        for (staging_path, target) in &targets {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(staging_path, target)
                .with_context(|| format!("Failed to rename block to {}", target.display()))?;
        }

        // 5. Single sync of blocks parent directory (renames now durable)
        {
            let parent_f = std::fs::File::open(&self.blocks_dir)
                .context("Failed to open blocks dir for fsync")?;
            parent_f
                .sync_all()
                .context("Failed to sync blocks parent dir")?;
        }

        // Clean up empty staging directory
        let _ = std::fs::remove_dir_all(&staging_dir);

        debug!(
            "Stored {} blocks via staged batch ({})",
            chunks.len(),
            staging_id
        );
        Ok(())
    }

    /// Read a file by assembling its blocks given an ordered list of block hex hashes
    pub fn read_blocks(&self, block_hex_hashes: &[String]) -> Result<Vec<u8>> {
        let mut file_data = Vec::new();
        for hex_hash in block_hex_hashes {
            let path = self.hex_to_block_path(hex_hash);
            let block =
                std::fs::read(&path).with_context(|| format!("Block not found: {hex_hash}"))?;
            file_data.extend_from_slice(&block);
        }
        Ok(file_data)
    }

    /// Get a block by its BLAKE3 hash
    pub fn get_block(&self, hash: &[u8; 32]) -> Option<Vec<u8>> {
        let path = self.block_path(hash);
        std::fs::read(&path).ok()
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

        if let Ok(entries) = std::fs::read_dir(&self.blocks_dir) {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                // Only look at 2-hex-char prefix directories (skip staging/, etc.)
                if !entry_path.is_dir() {
                    continue;
                }
                let dir_name = match entry_path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                // Prefix dirs are always 2 hex characters
                if dir_name.len() != 2 || !dir_name.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }

                if let Ok(sub_entries) = std::fs::read_dir(&entry_path) {
                    for sub in sub_entries.flatten() {
                        let block_path = sub.path();
                        if !block_path.is_file() {
                            continue;
                        }
                        if let Some(name) = block_path.file_name().and_then(|n| n.to_str()) {
                            if !referenced_hashes.contains(name) {
                                std::fs::remove_file(&block_path).with_context(|| {
                                    format!("Failed to remove {}", block_path.display())
                                })?;
                                deleted += 1;
                            }
                        }
                    }
                }
            }
        }

        Ok(deleted)
    }
}
