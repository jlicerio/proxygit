use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

/// Unique project identifier — UUID v4
pub type ProjectId = uuid::Uuid;

/// BLAKE3 hash as a 32-byte array
pub type BlockHash = [u8; 32];

/// File metadata record returned by the server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub mode: u32,
    pub mtime: u64,
    pub tree_hash: String,
}

/// Directory listing response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub project: ProjectId,
    pub path: String,
    pub entries: Vec<FileEntry>,
}

/// A single content-addressed block
#[derive(Debug, Clone)]
pub struct Block {
    pub hash: BlockHash,
    pub offset: u64,
    pub data: Vec<u8>,
}

/// LRU cache entry for local block cache
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub hash: BlockHash,
    pub local_path: PathBuf,
    pub last_access: SystemTime,
    pub refcount: u32,
}

/// Project configuration (local)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub id: ProjectId,
    pub name: String,
    pub mount_point: PathBuf,
    pub server_addr: String,
}

/// Server connection info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub addr: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1".into(),
            port: 8080,
        }
    }
}

/// Client daemon configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub server: ServerConfig,
    pub mount_point: PathBuf,
    pub cache_dir: PathBuf,
    pub wal_dir: PathBuf,
    pub build_cache_dir: PathBuf,
    pub server_cert: Option<PathBuf>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            mount_point: PathBuf::from("/tmp/proxygit/mount"),
            cache_dir: PathBuf::from("/tmp/proxygit/cache"),
            wal_dir: PathBuf::from("/tmp/proxygit/wal"),
            build_cache_dir: PathBuf::from("/tmp/proxygit/build_cache"),
            server_cert: None,
        }
    }
}

/// Build directory intercept rule
#[derive(Debug, Clone)]
pub struct BuildInterceptRule {
    pub pattern: &'static str,
    pub virtual_path: &'static str,
    pub description: &'static str,
}
impl BuildInterceptRule {
    pub const fn new(
        pattern: &'static str,
        virtual_path: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            pattern: pattern,
            virtual_path,
            description,
        }
    }
}
/// Known build intercept rules
pub fn default_build_intercepts() -> Vec<BuildInterceptRule> {
    vec![
        BuildInterceptRule::new("**/target/*", "target", "Rust/Cargo build artifacts"),
        BuildInterceptRule::new("**/node_modules/*", "node_modules", "Node.js packages"),
        BuildInterceptRule::new("**/.next/*", ".next", "Next.js build cache"),
    ]
}
