//! FUSE filesystem implementation for ProxyGit.
//!
//! Behind `#[cfg(feature = "fuse")]` — only compiles when the `fuse` Cargo feature
//! is explicitly enabled and the system library (macFUSE / libfuse3) is present.
//!
//! Presents a virtual directory backed by the ProxyGit server with a hierarchical tree model.
//! File reads hydrate on-demand from the server's block store and local cache.
//! File writes go through the local WAL for NVMe speed, then async-flush.
//! Build directories (target/, node_modules/) are intercepted and redirected to local NVMe.

use std::path::{Path, PathBuf};

use proxygit_common::types::ClientConfig;

// ── Build Intercept Utilities (always available) ───────────────────

/// Check if a path matches a known build artifact pattern.
/// These directories should be redirected to local NVMe.
pub fn is_build_path(path: &str) -> bool {
    path.contains("/target/")
        || path.contains("/node_modules/")
        || path.contains("/.next/")
        || path.contains("/DerivedData/")
        || path == "/target"
        || path == "/node_modules"
        || path == "/.next"
        || path == "/DerivedData"
}

/// Map a VFS build path to a local NVMe cache path.
pub fn build_cache_path(project_id: &uuid::Uuid, vfs_path: &str, cache_dir: &Path) -> PathBuf {
    let cache_root = cache_dir.join(project_id.to_string());
    let clean_path = vfs_path.trim_start_matches('/');
    cache_root.join(clean_path)
}

// ── FUSE Filesystem (only with `fuse` feature) ─────────────────────

#[cfg(feature = "fuse")]
mod fuse_impl {
    use std::collections::{BTreeMap, HashMap};
    use std::ffi::OsStr;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, SystemTime};

    use fuser::{
        FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
        ReplyEntry, ReplyOpen, ReplyWrite, Request,
    };
    use tracing::{debug, warn};

    use super::{build_cache_path, is_build_path};
    use crate::wal::LocalWal;
    use proxygit_common::protocol::*;
    use proxygit_common::types::{ClientConfig, FileEntry};

    /// FUSE filesystem that proxies to the ProxyGit server.
    pub struct FuseFilesystem {
        conn: quinn::Connection,
        project_id: uuid::Uuid,
        config: ClientConfig,
        wal: Arc<LocalWal>,
        ttl: Duration,
        ino_map: Arc<RwLock<HashMap<u64, String>>>,
    }

    impl FuseFilesystem {
        pub fn new(
            conn: quinn::Connection,
            project_id: uuid::Uuid,
            config: ClientConfig,
            wal: Arc<LocalWal>,
        ) -> Self {
            let mut map = HashMap::new();
            map.insert(1, "/".to_string());
            Self {
                conn,
                project_id,
                config,
                wal,
                ttl: Duration::from_secs(1),
                ino_map: Arc::new(RwLock::new(map)),
            }
        }

        fn make_attr(&self, ino: u64, size: u64, kind: FileType, perm: u16) -> FileAttr {
            let now = SystemTime::now();
            FileAttr {
                ino,
                size,
                blocks: (size + 511) / 512,
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                kind,
                perm,
                nlink: if kind == FileType::Directory { 2 } else { 1 },
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                rdev: 0,
                blksize: 4096,
                flags: 0,
            }
        }

        fn path_to_ino(path: &str) -> u64 {
            let h = blake3::hash(path.as_bytes());
            u64::from_le_bytes(h.as_bytes()[0..8].try_into().unwrap())
        }

        fn record_path(&self, ino: u64, path: String) {
            if let Ok(mut map) = self.ino_map.write() {
                map.insert(ino, path);
            }
        }

        fn get_path(&self, ino: u64) -> Option<String> {
            self.ino_map.read().ok()?.get(&ino).cloned()
        }

        fn fetch_file_list(&self) -> Vec<FileEntry> {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let (mut send, mut recv) = self.conn.open_bi().await.ok()?;
                let hash = [0u8; 32];
                send_frame(
                    &mut send,
                    MSG_LIST_PROJECT,
                    self.project_id.as_u128(),
                    &hash,
                    &[],
                )
                .await
                .ok()?;

                let resp = recv_frame(&mut recv).await.ok()?;
                if resp.msg_type == MSG_LIST_PROJECT_RESP {
                    serde_json::from_slice(&resp.payload).ok()?
                } else {
                    None
                }
            })
            .unwrap_or_default()
        }
    }

    impl Filesystem for FuseFilesystem {
        fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let parent_path = self.get_path(parent).unwrap_or_else(|| "/".to_string());
            let name_str = name.to_string_lossy();
            let path = if parent_path == "/" {
                format!("/{name_str}")
            } else {
                format!("{parent_path}/{name_str}")
            };
            let rel_path = path.trim_start_matches('/');

            let ino = Self::path_to_ino(&path);
            self.record_path(ino, path.clone());

            // Build directory interception: check local NVMe build cache path
            if is_build_path(&path) {
                let local_path =
                    build_cache_path(&self.project_id, &path, &self.config.build_cache_dir);
                let (kind, size) = if local_path.is_dir() {
                    (FileType::Directory, 4096)
                } else if let Ok(meta) = std::fs::metadata(&local_path) {
                    (FileType::RegularFile, meta.len())
                } else {
                    (FileType::Directory, 4096)
                };
                let attr = self.make_attr(ino, size, kind, 0o755);
                reply.entry(&self.ttl, &attr, 0);
                return;
            }

            let files = self.fetch_file_list();

            // Check if exact file match
            if let Some(file) = files
                .iter()
                .find(|f| f.path.trim_start_matches('/') == rel_path)
            {
                let attr = self.make_attr(ino, file.size, FileType::RegularFile, 0o644);
                reply.entry(&self.ttl, &attr, 0);
                return;
            }

            // Check if directory prefix match
            let prefix = format!("{rel_path}/");
            if files
                .iter()
                .any(|f| f.path.trim_start_matches('/').starts_with(&prefix))
            {
                let attr = self.make_attr(ino, 4096, FileType::Directory, 0o755);
                reply.entry(&self.ttl, &attr, 0);
                return;
            }

            reply.error(libc::ENOENT);
        }

        fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
            let path = self.get_path(ino).unwrap_or_else(|| "/".to_string());
            if path == "/" {
                let attr = self.make_attr(1, 4096, FileType::Directory, 0o755);
                reply.attr(&self.ttl, &attr);
                return;
            }

            let rel_path = path.trim_start_matches('/');

            if is_build_path(&path) {
                let local_path =
                    build_cache_path(&self.project_id, &path, &self.config.build_cache_dir);
                let size = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
                let kind = if local_path.is_dir() {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                let attr = self.make_attr(ino, size, kind, 0o755);
                reply.attr(&self.ttl, &attr);
                return;
            }

            let files = self.fetch_file_list();

            if let Some(file) = files
                .iter()
                .find(|f| f.path.trim_start_matches('/') == rel_path)
            {
                let attr = self.make_attr(ino, file.size, FileType::RegularFile, 0o644);
                reply.attr(&self.ttl, &attr);
                return;
            }

            let prefix = format!("{rel_path}/");
            if files
                .iter()
                .any(|f| f.path.trim_start_matches('/').starts_with(&prefix))
            {
                let attr = self.make_attr(ino, 4096, FileType::Directory, 0o755);
                reply.attr(&self.ttl, &attr);
                return;
            }

            reply.error(libc::ENOENT);
        }

        fn readdir(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            if offset == 0 {
                reply.add(1, 0, FileType::Directory, ".");
                reply.add(1, 0, FileType::Directory, "..");
            }

            let dir_path = self.get_path(ino).unwrap_or_else(|| "/".to_string());

            // Build directory interception: read directly from local NVMe build cache
            if is_build_path(&dir_path) {
                let local_path =
                    build_cache_path(&self.project_id, &dir_path, &self.config.build_cache_dir);
                if let Ok(entries) = std::fs::read_dir(&local_path) {
                    let mut idx = 3i64;
                    for entry in entries.flatten() {
                        if idx > offset {
                            let file_name = entry.file_name();
                            let name_str = file_name.to_string_lossy();
                            let child_path = format!("{dir_path}/{name_str}");
                            let child_ino = Self::path_to_ino(&child_path);
                            self.record_path(child_ino, child_path);

                            let kind = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                FileType::Directory
                            } else {
                                FileType::RegularFile
                            };
                            reply.add(child_ino, idx, kind, &*name_str);
                        }
                        idx += 1;
                    }
                }
                reply.ok();
                return;
            }

            let rel_dir = dir_path.trim_start_matches('/').to_string();
            let files = self.fetch_file_list();

            // Extract immediate children under rel_dir for hierarchical navigation
            let mut children: BTreeMap<String, (FileType, u64)> = BTreeMap::new();

            for file in &files {
                let path = file.path.trim_start_matches('/');
                if rel_dir.is_empty() {
                    let mut parts = path.split('/');
                    if let Some(first) = parts.next() {
                        if parts.next().is_none() {
                            children.insert(first.to_string(), (FileType::RegularFile, file.size));
                        } else {
                            children
                                .entry(first.to_string())
                                .or_insert((FileType::Directory, 4096));
                        }
                    }
                } else if path.starts_with(&format!("{rel_dir}/")) {
                    let remainder = &path[rel_dir.len() + 1..];
                    let mut parts = remainder.split('/');
                    if let Some(first) = parts.next() {
                        if parts.next().is_none() {
                            children.insert(first.to_string(), (FileType::RegularFile, file.size));
                        } else {
                            children
                                .entry(first.to_string())
                                .or_insert((FileType::Directory, 4096));
                        }
                    }
                }
            }

            let mut idx = 3i64;
            for (name, (kind, _size)) in children {
                if idx > offset {
                    let child_path = if rel_dir.is_empty() {
                        format!("/{name}")
                    } else {
                        format!("/{rel_dir}/{name}")
                    };
                    let child_ino = Self::path_to_ino(&child_path);
                    self.record_path(child_ino, child_path);
                    reply.add::<&OsStr>(child_ino, idx, kind, name.as_ref());
                }
                idx += 1;
            }

            reply.ok();
        }

        fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
            reply.opened(1, 0);
        }

        fn read(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyData,
        ) {
            let path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.data(&[]);
                    return;
                }
            };

            // Build directory interception: read directly from local NVMe build cache
            if is_build_path(&path) {
                let local_path =
                    build_cache_path(&self.project_id, &path, &self.config.build_cache_dir);
                if let Ok(data) = std::fs::read(&local_path) {
                    let offset = offset as usize;
                    let end = (offset + size as usize).min(data.len());
                    if offset < data.len() {
                        reply.data(&data[offset..end]);
                    } else {
                        reply.data(&[]);
                    }
                } else {
                    reply.data(&[]);
                }
                return;
            }

            // Check local block cache with 2-second TTL for cross-agent remote invalidation
            let cache_path = self
                .config
                .cache_dir
                .join(blake3::hash(path.as_bytes()).to_hex().as_str());
            let is_cache_valid = if let Ok(meta) = std::fs::metadata(&cache_path) {
                if let Ok(mtime) = meta.modified() {
                    mtime
                        .elapsed()
                        .map(|d| d < Duration::from_secs(2))
                        .unwrap_or(false)
                } else {
                    false
                }
            } else {
                false
            };

            let data = if is_cache_valid {
                std::fs::read(&cache_path).unwrap_or_default()
            } else {
                // Fetch from server over QUIC
                let rt = tokio::runtime::Handle::current();
                let fetched = rt
                    .block_on(async {
                        let (mut send, mut recv) = self.conn.open_bi().await.ok()?;
                        let path_bytes = path.trim_start_matches('/').as_bytes();
                        let hash = blake3::hash(path_bytes).into();

                        send_frame(
                            &mut send,
                            MSG_READ_FILE,
                            self.project_id.as_u128(),
                            &hash,
                            path_bytes,
                        )
                        .await
                        .ok()?;

                        let resp = recv_frame(&mut recv).await.ok()?;
                        if resp.msg_type == MSG_READ_FILE_RESP {
                            Some(resp.payload)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                // Save to local block cache
                if !fetched.is_empty() {
                    let _ = std::fs::create_dir_all(&self.config.cache_dir);
                    let _ = std::fs::write(&cache_path, &fetched);
                }
                fetched
            };

            let offset = offset as usize;
            let end = (offset + size as usize).min(data.len());
            if offset < data.len() {
                reply.data(&data[offset..end]);
            } else {
                reply.data(&[]);
            }
        }

        fn write(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            data: &[u8],
            _write_flags: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyWrite,
        ) {
            let path = self.get_path(ino).unwrap_or_else(|| format!("/ino-{ino}"));
            let rel_path = path.trim_start_matches('/').to_string();

            // Build directory interception: write directly to local NVMe build cache
            if is_build_path(&path) {
                let local_path =
                    build_cache_path(&self.project_id, &path, &self.config.build_cache_dir);
                if let Some(parent) = local_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                use std::io::{Seek, SeekFrom, Write as _};
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(&local_path)
                {
                    let _ = file.seek(SeekFrom::Start(offset as u64));
                    let _ = file.write_all(data);
                }
                reply.written(data.len() as u32);
                return;
            }
            // Invalidate local read cache on write
            let cache_path = self
                .config
                .cache_dir
                .join(blake3::hash(path.as_bytes()).to_hex().as_str());
            let _ = std::fs::remove_file(&cache_path);

            // Source write: append entry to local WAL log with actual file path
            let rt = tokio::runtime::Handle::current();
            let wal_ok = rt.block_on(async {
                match self.wal.append_entry(&rel_path, offset as u64, data).await {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("WAL write failed for {rel_path}: {e}");
                        false
                    }
                }
            });
            if wal_ok {
                reply.written(data.len() as u32);
            } else {
                reply.error(libc::EIO);
            }
        }

        fn create(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            _flags: i32,
            reply: ReplyCreate,
        ) {
            let parent_path = self.get_path(parent).unwrap_or_else(|| "/".to_string());
            let name_str = name.to_string_lossy();
            let path = if parent_path == "/" {
                format!("/{name_str}")
            } else {
                format!("{parent_path}/{name_str}")
            };

            let ino = Self::path_to_ino(&path);
            self.record_path(ino, path);
            let attr = self.make_attr(ino, 0, FileType::RegularFile, 0o644);
            reply.created(&self.ttl, &attr, 0, 1, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_cache_path_no_collision() {
        let project_id = uuid::Uuid::nil();
        let cache_dir = Path::new("/tmp/proxygit/build_cache");

        let target_path = build_cache_path(&project_id, "/target/debug/app", cache_dir);
        let node_path = build_cache_path(&project_id, "/node_modules/debug/app", cache_dir);

        assert_ne!(target_path, node_path);
        assert!(target_path.ends_with("target/debug/app"));
        assert!(node_path.ends_with("node_modules/debug/app"));
    }

    #[test]
    fn test_is_build_path() {
        assert!(is_build_path("/target/debug/app"));
        assert!(is_build_path("/node_modules/express/index.js"));
        assert!(is_build_path("/src-tauri/target/release/app"));
        assert!(!is_build_path("/src/main.rs"));
    }
}
#[cfg(feature = "fuse")]
pub use fuse_impl::FuseFilesystem;
