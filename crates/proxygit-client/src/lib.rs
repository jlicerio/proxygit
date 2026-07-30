pub mod fuse_mount;
pub mod wal;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::Endpoint;
use tracing::{info, warn};

use proxygit_common::protocol::*;
use proxygit_common::types::FileEntry;

fn parse_hex_tree_hash(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn auto_base_hash_enabled() -> bool {
    matches!(
        std::env::var("PROXYGIT_WRITE_CONFLICT")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "reject_stale" | "reject" | "strict"
    )
}

/// Resolve the server's current content hash for optimistic concurrency.
/// Missing file → all-zero hash (server treats as "create").
async fn fetch_base_tree_hash(pool: &StreamPool, project_id: uuid::Uuid, path: &str) -> [u8; 32] {
    let st = mcp_stat(pool, project_id, path).await;
    if let Some(h) = st
        .get("tree_hash")
        .and_then(|v| v.as_str())
        .and_then(parse_hex_tree_hash)
    {
        return h;
    }
    [0u8; 32]
}

/// A bounded pool of pre-opened bidirectional QUIC streams.
/// Avoids the overhead of calling `conn.open_bi()` for every MCP tool invocation.
pub struct StreamPool {
    conn: quinn::Connection,
    pool: tokio::sync::Mutex<Vec<(quinn::SendStream, quinn::RecvStream)>>,
    max_size: usize,
}

impl StreamPool {
    pub fn new(conn: quinn::Connection, max_size: usize) -> Self {
        Self {
            conn,
            pool: tokio::sync::Mutex::new(Vec::with_capacity(max_size)),
            max_size,
        }
    }

    /// Fill the pool with pre-opened streams.
    pub async fn prewarm(&self) {
        let mut pool = self.pool.lock().await;
        while pool.len() < self.max_size {
            match self.conn.open_bi().await {
                Ok(pair) => pool.push(pair),
                Err(_) => break,
            }
        }
    }

    /// Borrow a stream from the pool, or open a new one if empty.
    pub async fn borrow(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        let mut pool = self.pool.lock().await;
        if let Some(pair) = pool.pop() {
            Ok(pair)
        } else {
            self.conn
                .open_bi()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
    }

    /// Return a stream to the pool for reuse.
    /// Note: QUIC streams are single-request — we drop them instead of caching.
    /// The pool's primary value is pre-warming connections via `borrow()`.
    pub async fn return_stream(&self, _send: quinn::SendStream, _recv: quinn::RecvStream) {
        // Drop streams — QUIC streams are half-closed after one request-response.
    }
}

/// Connect to the ProxyGit server over QUIC (with automatic server cert discovery)
pub async fn connect_to_server(server_addr: SocketAddr) -> Result<quinn::Connection> {
    connect_to_server_with_cert(server_addr, None).await
}

/// Connect to the ProxyGit server over QUIC with explicit server certificate path
pub async fn connect_to_server_with_cert(
    server_addr: SocketAddr,
    explicit_cert_path: Option<PathBuf>,
) -> Result<quinn::Connection> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();

    let mut cert_paths = Vec::new();
    if let Some(path) = explicit_cert_path {
        cert_paths.push(path);
    }
    if let Ok(env_cert) = std::env::var("PROXYGIT_SERVER_CERT") {
        cert_paths.push(PathBuf::from(env_cert));
    }
    cert_paths.push(PathBuf::from("/tmp/proxygit-server/data/server_cert.der"));
    cert_paths.push(PathBuf::from("data/server_cert.der"));
    if let Ok(home) = std::env::var("HOME") {
        cert_paths.push(PathBuf::from(home).join(".config/proxygit/server_cert.der"));
    }

    let mut loaded = false;
    for path in cert_paths {
        if path.exists() {
            if let Ok(cert_bytes) = std::fs::read(&path) {
                info!("Loaded server certificate from {}", path.display());
                let cert_der: rustls::pki_types::CertificateDer = cert_bytes.into();
                roots.add(cert_der).ok();
                loaded = true;
                break;
            }
        }
    }

    if !loaded {
        warn!("No pinned server_cert.der found; TLS verification requires pinned cert or CA");
    }

    let client_cert = std::env::var("PROXYGIT_CLIENT_CERT")
        .ok()
        .filter(|s| !s.is_empty());
    let client_key = std::env::var("PROXYGIT_CLIENT_KEY")
        .ok()
        .filter(|s| !s.is_empty());

    let mut tls_config = match (&client_cert, &client_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert_bytes = std::fs::read(cert_path)
                .with_context(|| format!("read PROXYGIT_CLIENT_CERT at {cert_path}"))?;
            let key_bytes = std::fs::read(key_path)
                .with_context(|| format!("read PROXYGIT_CLIENT_KEY at {key_path}"))?;
            let cert_der: rustls::pki_types::CertificateDer = cert_bytes.into();
            let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_bytes),
            );
            info!(
                "mTLS client cert enabled (cert={}, key={})",
                cert_path, key_path
            );
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_client_auth_cert(vec![cert_der], key_der)
                .context("build TLS client config with client cert")?
        }
        (None, None) => rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
        _ => anyhow::bail!(
            "Set both PROXYGIT_CLIENT_CERT and PROXYGIT_CLIENT_KEY for mTLS, or neither"
        ),
    };
    tls_config.alpn_protocols = vec![b"proxygit-1".to_vec()];

    let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)?;

    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(Duration::from_secs(5)));

    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_config));
    client_config.transport_config(Arc::new(transport_config));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint.connect(server_addr, "proxygit-server")?.await?;

    // Optional bearer auth (must be first bi-stream when server has PROXYGIT_TOKEN set).
    if let Some(token) = proxygit_common::auth::load_token_from_env()? {
        let (mut send, mut recv) = conn.open_bi().await?;
        let payload = token.as_bytes();
        let hash = blake3::hash(payload).into();
        send_frame(&mut send, MSG_AUTH, 0, &hash, payload).await?;
        let resp = tokio::time::timeout(Duration::from_secs(5), recv_frame(&mut recv))
            .await
            .map_err(|_| anyhow::anyhow!("auth handshake timed out"))??;
        match resp.msg_type {
            MSG_AUTH_OK => {
                info!("Authenticated to server");
            }
            MSG_AUTH_FAIL => {
                anyhow::bail!("Server rejected auth token (MSG_AUTH_FAIL)");
            }
            MSG_ERROR => {
                anyhow::bail!("Auth error: {}", String::from_utf8_lossy(&resp.payload));
            }
            other => anyhow::bail!("Unexpected auth response type: 0x{other:02x}"),
        }
    }

    Ok(conn)
}

/// List files in a project from the server
pub async fn list_project(
    conn: &quinn::Connection,
    project_id: uuid::Uuid,
) -> Result<Vec<FileEntry>> {
    let (mut send, mut recv) = conn.open_bi().await?;

    let hash = [0u8; 32];
    send_frame(
        &mut send,
        MSG_LIST_PROJECT,
        project_id.as_u128(),
        &hash,
        &[],
    )
    .await?;
    let resp = recv_frame(&mut recv).await?;

    if resp.msg_type == MSG_LIST_PROJECT_RESP {
        let entries: Vec<FileEntry> = serde_json::from_slice(&resp.payload)?;
        Ok(entries)
    } else if resp.msg_type == MSG_ERROR {
        let err = String::from_utf8_lossy(&resp.payload);
        anyhow::bail!("Server error: {err}");
    } else {
        anyhow::bail!("Unexpected response type: 0x{:02x}", resp.msg_type);
    }
}

pub async fn mcp_read_file(
    pool: &StreamPool,
    project_id: uuid::Uuid,
    path: &str,
) -> serde_json::Value {
    let path_bytes = path.as_bytes();
    let hash = blake3::hash(path_bytes).into();

    match pool.borrow().await {
        Ok((mut send, mut recv)) => {
            if let Err(e) = send_frame(
                &mut send,
                MSG_READ_FILE,
                project_id.as_u128(),
                &hash,
                path_bytes,
            )
            .await
            {
                return serde_json::json!({ "error": format!("send error: {e}") });
            }
            match recv_frame(&mut recv).await {
                Ok(resp) if resp.msg_type == MSG_READ_FILE_RESP => {
                    pool.return_stream(send, recv).await;
                    let content = String::from_utf8_lossy(&resp.payload).to_string();
                    serde_json::json!({ "content": content })
                }
                Ok(resp) if resp.msg_type == MSG_ERROR => {
                    pool.return_stream(send, recv).await;
                    serde_json::json!({ "error": String::from_utf8_lossy(&resp.payload).to_string() })
                }
                Ok(_) => {
                    pool.return_stream(send, recv).await;
                    serde_json::json!({ "error": "unexpected response" })
                }
                Err(e) => serde_json::json!({ "error": format!("recv error: {e}") }),
            }
        }
        Err(e) => serde_json::json!({ "error": format!("pool error: {e}") }),
    }
}

pub async fn mcp_read_raw_bytes(
    pool: &StreamPool,
    project_id: uuid::Uuid,
    path: &str,
) -> Result<Vec<u8>> {
    let path_bytes = path.as_bytes();
    let hash = blake3::hash(path_bytes).into();
    let (mut send, mut recv) = pool.borrow().await?;
    send_frame(
        &mut send,
        MSG_READ_FILE,
        project_id.as_u128(),
        &hash,
        path_bytes,
    )
    .await?;
    let resp = recv_frame(&mut recv).await?;
    if resp.msg_type == MSG_READ_FILE_RESP {
        pool.return_stream(send, recv).await;
        Ok(resp.payload)
    } else if resp.msg_type == MSG_ERROR {
        pool.return_stream(send, recv).await;
        anyhow::bail!("Server error: {}", String::from_utf8_lossy(&resp.payload));
    } else {
        pool.return_stream(send, recv).await;
        anyhow::bail!("Unexpected response type: 0x{:02x}", resp.msg_type);
    }
}

pub async fn mcp_write_file(
    pool: &StreamPool,
    project_id: uuid::Uuid,
    path: &str,
    content: &[u8],
) -> serde_json::Value {
    mcp_write_file_opts(pool, project_id, path, content, None).await
}

/// Write with optional optimistic-concurrency base hash (32-byte BLAKE3 of prior content).
///
/// When `expected_tree_hash` is `None` and `PROXYGIT_WRITE_CONFLICT` is
/// `reject_stale`/`reject`/`strict`, the client stats the path and sends the
/// current `tree_hash` automatically (new files use the all-zero hash).
pub async fn mcp_write_file_opts(
    pool: &StreamPool,
    project_id: uuid::Uuid,
    path: &str,
    content: &[u8],
    expected_tree_hash: Option<[u8; 32]>,
) -> serde_json::Value {
    let expected_tree_hash = match expected_tree_hash {
        Some(h) => Some(h),
        None if auto_base_hash_enabled() => {
            Some(fetch_base_tree_hash(pool, project_id, path).await)
        }
        None => None,
    };

    const BLOCK_SIZE: usize = 65536; // 64KB fixed-size blocks

    let mut sparse_chunks: Vec<SparseChunk> = Vec::new();
    let mut all_hashes: Vec<[u8; 32]> = Vec::new();
    for data in content.chunks(BLOCK_SIZE) {
        let hash = *blake3::hash(data).as_bytes();
        all_hashes.push(hash);
        sparse_chunks.push(SparseChunk {
            hash,
            data: data.to_vec(),
        });
    }

    let known_hashes: Vec<[u8; 32]> = match pool.borrow().await {
        Ok((mut send, mut recv)) => {
            let query_payload = encode_hash_list(&all_hashes);
            let hash = blake3::hash(&query_payload).into();
            if let Err(e) = send_frame(
                &mut send,
                MSG_HAS_BLOCKS,
                project_id.as_u128(),
                &hash,
                &query_payload,
            )
            .await
            {
                warn!("HAS_BLOCKS send failed: {e}; falling back to full send");
                Vec::new()
            } else {
                match recv_frame(&mut recv).await {
                    Ok(resp) if resp.msg_type == MSG_HAS_BLOCKS_RESP => {
                        match decode_hash_list(&resp.payload) {
                            Ok((_, hashes)) => hashes,
                            Err(e) => {
                                warn!("HAS_BLOCKS_RESP decode: {e}; full send");
                                Vec::new()
                            }
                        }
                    }
                    Ok(resp) if resp.msg_type == MSG_ERROR => {
                        warn!(
                            "HAS_BLOCKS server error: {}; full send",
                            String::from_utf8_lossy(&resp.payload)
                        );
                        Vec::new()
                    }
                    Ok(_) => {
                        warn!("HAS_BLOCKS unexpected response; full send");
                        Vec::new()
                    }
                    Err(e) => {
                        warn!("HAS_BLOCKS recv error: {e}; full send");
                        Vec::new()
                    }
                }
            }
        }
        Err(e) => {
            warn!("HAS_BLOCKS pool error: {e}; falling back to full send");
            Vec::new()
        }
    };

    for chunk in &mut sparse_chunks {
        if known_hashes.contains(&chunk.hash) {
            chunk.data.clear();
        }
    }

    let payload = encode_sparse_write(path, &sparse_chunks, expected_tree_hash.as_ref());
    let hash = blake3::hash(&payload).into();

    match pool.borrow().await {
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
                return serde_json::json!({ "error": format!("send error: {e}") });
            }
            match recv_frame(&mut recv).await {
                Ok(resp) if resp.msg_type == MSG_WRITE_ACK => {
                    pool.return_stream(send, recv).await;
                    if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&resp.payload) {
                        if v.get("status").is_none() {
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert("status".into(), serde_json::json!("ok"));
                            }
                        }
                        return v;
                    }
                    serde_json::json!({
                        "status": "ok",
                        "tree_hash": String::from_utf8_lossy(&resp.payload).to_string()
                    })
                }
                Ok(resp) if resp.msg_type == MSG_ERROR => {
                    pool.return_stream(send, recv).await;
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp.payload) {
                        return v;
                    }
                    serde_json::json!({ "error": String::from_utf8_lossy(&resp.payload).to_string() })
                }
                Ok(_) => {
                    pool.return_stream(send, recv).await;
                    serde_json::json!({ "error": "unexpected response" })
                }
                Err(e) => serde_json::json!({ "error": format!("recv error: {e}") }),
            }
        }
        Err(e) => serde_json::json!({ "error": format!("pool error: {e}") }),
    }
}

pub async fn mcp_list_directory(
    pool: &StreamPool,
    project_id: uuid::Uuid,
    _path: &str,
) -> serde_json::Value {
    match pool.borrow().await {
        Ok((mut send, mut recv)) => {
            if let Err(e) = send_frame(
                &mut send,
                MSG_LIST_PROJECT,
                project_id.as_u128(),
                &[0u8; 32],
                &[],
            )
            .await
            {
                return serde_json::json!({ "error": format!("send error: {e}") });
            }
            match recv_frame(&mut recv).await {
                Ok(resp) if resp.msg_type == MSG_LIST_PROJECT_RESP => {
                    pool.return_stream(send, recv).await;
                    let entries: Vec<FileEntry> =
                        serde_json::from_slice(&resp.payload).unwrap_or_default();
                    let files: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
                    serde_json::json!({ "files": files })
                }
                Ok(resp) if resp.msg_type == MSG_ERROR => {
                    pool.return_stream(send, recv).await;
                    serde_json::json!({ "error": String::from_utf8_lossy(&resp.payload).to_string() })
                }
                Ok(_) => {
                    pool.return_stream(send, recv).await;
                    serde_json::json!({ "error": "unexpected response" })
                }
                Err(e) => serde_json::json!({ "error": format!("recv error: {e}") }),
            }
        }
        Err(e) => serde_json::json!({ "error": format!("pool error: {e}") }),
    }
}

pub async fn mcp_stat(pool: &StreamPool, project_id: uuid::Uuid, path: &str) -> serde_json::Value {
    let path_bytes = path.as_bytes();
    let hash = blake3::hash(path_bytes).into();

    match pool.borrow().await {
        Ok((mut send, mut recv)) => {
            if let Err(e) = send_frame(
                &mut send,
                MSG_STAT_FILE,
                project_id.as_u128(),
                &hash,
                path_bytes,
            )
            .await
            {
                return serde_json::json!({ "error": format!("send error: {e}") });
            }
            match recv_frame(&mut recv).await {
                Ok(resp) if resp.msg_type == MSG_STAT_FILE_RESP => {
                    pool.return_stream(send, recv).await;
                    if let Ok(entry) = serde_json::from_slice::<FileEntry>(&resp.payload) {
                        serde_json::json!({
                            "path": entry.path,
                            "size": entry.size,
                            "mode": entry.mode,
                            "mtime": entry.mtime,
                            "tree_hash": entry.tree_hash
                        })
                    } else {
                        serde_json::json!({ "error": "deserialization error" })
                    }
                }
                Ok(resp) if resp.msg_type == MSG_ERROR => {
                    pool.return_stream(send, recv).await;
                    serde_json::json!({ "error": String::from_utf8_lossy(&resp.payload).to_string() })
                }
                Ok(_) => {
                    pool.return_stream(send, recv).await;
                    serde_json::json!({ "error": "unexpected response" })
                }
                Err(e) => serde_json::json!({ "error": format!("recv error: {e}") }),
            }
        }
        Err(e) => serde_json::json!({ "error": format!("pool error: {e}") }),
    }
}

/// Content search over project files (feature-hash embeddings by default).
/// Prefer [`mcp_content_search`]. Kept as alias for older callers.
pub async fn mcp_semantic_search(
    pool: &StreamPool,
    project_id: uuid::Uuid,
    query: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let query_bytes = query.as_bytes();
    let limit_bytes = limit.to_le_bytes();
    let mut payload = Vec::with_capacity(query_bytes.len() + 1 + 8);
    payload.extend_from_slice(query_bytes);
    payload.push(b'\0');
    payload.extend_from_slice(&limit_bytes);

    let (mut send, mut recv) = pool.borrow().await?;
    send_frame(
        &mut send,
        MSG_SEMANTIC_SEARCH,
        project_id.as_u128(),
        &[0u8; 32],
        &payload,
    )
    .await?;
    let resp = recv_frame(&mut recv).await?;
    if resp.msg_type == MSG_SEMANTIC_SEARCH_RESP {
        pool.return_stream(send, recv).await;
        let paths: Vec<String> = serde_json::from_slice(&resp.payload)?;
        Ok(paths)
    } else if resp.msg_type == MSG_ERROR {
        pool.return_stream(send, recv).await;
        anyhow::bail!("Server error: {}", String::from_utf8_lossy(&resp.payload));
    } else {
        pool.return_stream(send, recv).await;
        anyhow::bail!("Unexpected response type: 0x{:02x}", resp.msg_type);
    }
}

/// Honest name for the hash-embedding search stub.
pub async fn mcp_content_search(
    pool: &StreamPool,
    project_id: uuid::Uuid,
    query: &str,
    limit: usize,
) -> Result<Vec<String>> {
    mcp_semantic_search(pool, project_id, query, limit).await
}

/// Run standard Anthropic MCP stdio JSON-RPC 2.0 protocol loop over any reader/writer pair
pub async fn run_mcp_stdio_stream<R, W>(
    reader: R,
    mut writer: W,
    conn: quinn::Connection,
    project_id: uuid::Uuid,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let pool = Arc::new(StreamPool::new(conn, 8));
    pool.prewarm().await;

    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let resp = handle_mcp_jsonrpc_request(&line, &pool, project_id).await;
        if !resp.is_null() {
            if let Ok(json) = serde_json::to_string(&resp) {
                writer.write_all(json.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
        }
    }
    Ok(())
}

/// Handle a single official Anthropic MCP JSON-RPC 2.0 request frame
pub async fn handle_mcp_jsonrpc_request(
    line: &str,
    pool: &StreamPool,
    project_id: uuid::Uuid,
) -> serde_json::Value {
    let req: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "error": { "code": -32700, "message": format!("Parse error: {e}") },
                "id": null
            });
        }
    };

    let method = req["method"].as_str().unwrap_or("");
    let params = &req["params"];
    let id = &req["id"];

    if method.starts_with("notifications/") {
        return serde_json::Value::Null;
    }

    let result = match method {
        "initialize" => {
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "proxygit-mcp",
                    "version": "0.1.0"
                }
            })
        }
        "tools/list" | "list_tools" => {
            serde_json::json!({
                "tools": [
                    {
                        "name": "read_file",
                        "description": "Read a file from the mounted workspace",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" }
                            },
                            "required": ["path"]
                        }
                    },
                    {
                        "name": "write_file",
                        "description": "Write content to a file. Provide either `content` (UTF-8 string) or `base64_content` (base64-encoded binary). Optional `expected_tree_hash` (64 hex) for optimistic concurrency when server has PROXYGIT_WRITE_CONFLICT=reject_stale.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "content": { "type": "string" },
                                "base64_content": { "type": "string" },
                                "expected_tree_hash": { "type": "string" }
                            },
                            "required": ["path"]
                        }
                    },
                    {
                        "name": "list_directory",
                        "description": "List files in a directory",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" }
                            },
                            "required": ["path"]
                        }
                    },
                    {
                        "name": "stat",
                        "description": "Get file metadata",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" }
                            },
                            "required": ["path"]
                        }
                    },
                    {
                        "name": "get_project_map",
                        "description": "Get a complete hierarchical map of the project tree with file sizes. One call replaces hundreds of recursive directory listings.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {},
                            "required": []
                        }
                    },
                    {
                        "name": "content_search",
                        "description": "Search project files by content-hash embedding similarity (MVP stub: deterministic BLAKE3 mock vectors, not a language model).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "description": "The search query" },
                                "limit": { "type": "integer", "description": "Maximum results to return", "default": 10 }
                            },
                            "required": ["query"]
                        }
                    },
                    {
                        "name": "semantic_search",
                        "description": "Search project files by content-hash embedding similarity (MVP stub: deterministic BLAKE3 mock vectors, not a language model). Deprecated alias for content_search.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "description": "The search query" },
                                "limit": { "type": "integer", "description": "Maximum results to return", "default": 10 }
                            },
                            "required": ["query"]
                        }
                    }
                ]
            })
        }
        "tools/call" | "execute_tool" => {
            let tool_name = params["name"]
                .as_str()
                .or_else(|| params["tool"].as_str())
                .unwrap_or("");
            let tool_params = &params["arguments"];
            let path = tool_params["path"].as_str().unwrap_or("/");

            let res = match tool_name {
                "read_file" => mcp_read_file(pool, project_id, path).await,
                "write_file" => {
                    let content: Vec<u8> = if let Some(b64) = tool_params["base64_content"].as_str()
                    {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .unwrap_or_else(|e| format!("base64 decode error: {e}").into_bytes())
                    } else {
                        tool_params["content"]
                            .as_str()
                            .unwrap_or("")
                            .as_bytes()
                            .to_vec()
                    };
                    let expected = tool_params["expected_tree_hash"]
                        .as_str()
                        .and_then(parse_hex_tree_hash);
                    mcp_write_file_opts(pool, project_id, path, &content, expected).await
                }
                "list_directory" => mcp_list_directory(pool, project_id, path).await,
                "stat" => mcp_stat(pool, project_id, path).await,
                "get_project_map" => generate_project_map(pool, project_id).await,
                "content_search" | "semantic_search" => {
                    let query = tool_params["query"].as_str().unwrap_or("");
                    let limit = tool_params["limit"].as_u64().unwrap_or(10) as usize;
                    match mcp_content_search(pool, project_id, query, limit).await {
                        Ok(paths) => serde_json::json!({
                            "paths": paths,
                            "note": "hash-embedding stub, not ML semantic search"
                        }),
                        Err(e) => serde_json::json!({ "error": format!("{e}") }),
                    }
                }
                _ => serde_json::json!({ "error": format!("Unknown tool: {tool_name}") }),
            };

            serde_json::json!({
                "content": [
                    {
                        "type": "text",
                        "text": serde_json::to_string_pretty(&res).unwrap_or_default()
                    }
                ]
            })
        }
        _ => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "error": { "code": -32601, "message": format!("Method not found: {method}") },
                "id": id
            });
        }
    };

    serde_json::json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id
    })
}

/// Build a complete hierarchical project map with file sizes.
/// One MCP call replaces hundreds of recursive directory listings.
/// First tries the new GET_PROJECT_MAP message (single RTT), falling back
/// to the old N+1 approach if the server doesn't support it.
pub async fn generate_project_map(pool: &StreamPool, project_id: uuid::Uuid) -> serde_json::Value {
    // ── Try the new GET_PROJECT_MAP message (single round-trip) ──────────
    {
        match pool.borrow().await {
            Ok((mut send, mut recv)) => {
                let send_result = send_frame(
                    &mut send,
                    MSG_GET_PROJECT_MAP,
                    project_id.as_u128(),
                    &[0u8; 32],
                    &[],
                )
                .await;
                match send_result {
                    Ok(()) => match recv_frame(&mut recv).await {
                        Ok(resp) if resp.msg_type == MSG_GET_PROJECT_MAP_RESP => {
                            pool.return_stream(send, recv).await;
                            if let Ok(entries) =
                                serde_json::from_slice::<Vec<FileEntry>>(&resp.payload)
                            {
                                let tree = build_project_tree_from_entries(&entries);
                                return serde_json::json!(tree);
                            }
                        }
                        Ok(resp) if resp.msg_type == MSG_ERROR => {
                            pool.return_stream(send, recv).await;
                            // Fall through to fallback
                        }
                        Ok(_) => {
                            pool.return_stream(send, recv).await;
                            // Fall through to fallback
                        }
                        Err(_) => {
                            // Stream broken, drop it — fall through
                        }
                    },
                    Err(_) => {
                        // Stream broken, drop it — fall through
                    }
                }
            }
            Err(_) => {
                // Pool error, fall through
            }
        }
    }

    // ── Fallback: old N+1 approach ──────────────────────────────────────
    let entries = mcp_list_directory(pool, project_id, "/").await;
    let files = match entries["files"].as_array() {
        Some(f) => f,
        None => return serde_json::json!({ "error": "failed to list project files" }),
    };

    let mut tree = serde_json::Map::new();
    tree.insert("name".into(), serde_json::Value::String(".".into()));
    tree.insert("type".into(), serde_json::Value::String("directory".into()));
    tree.insert(
        "dirs".into(),
        serde_json::Value::Object(serde_json::Map::new()),
    );
    tree.insert("files".into(), serde_json::Value::Array(Vec::new()));

    for file_val in files {
        let path = match file_val.as_str() {
            Some(p) => p,
            None => continue,
        };

        // Stat to get size
        let stat = mcp_stat(pool, project_id, path).await;
        let size = stat["size"].as_u64().unwrap_or(0);

        let segments: Vec<&str> = path.split('/').collect();
        if let Some(file_name) = segments.last() {
            let parent_segments = &segments[..segments.len() - 1];
            insert_path(&mut tree, parent_segments, file_name, size);
        }
    }

    serde_json::json!(tree)
}

/// Build a hierarchical project tree from a list of FileEntry items.
fn build_project_tree_from_entries(
    entries: &[FileEntry],
) -> serde_json::Map<String, serde_json::Value> {
    let mut tree = serde_json::Map::new();
    tree.insert("name".into(), serde_json::Value::String(".".into()));
    tree.insert("type".into(), serde_json::Value::String("directory".into()));
    tree.insert(
        "dirs".into(),
        serde_json::Value::Object(serde_json::Map::new()),
    );
    tree.insert("files".into(), serde_json::Value::Array(Vec::new()));

    for entry in entries {
        let segments: Vec<&str> = entry.path.split('/').collect();
        if let Some(file_name) = segments.last() {
            let parent_segments = &segments[..segments.len() - 1];
            insert_path(&mut tree, parent_segments, file_name, entry.size);
        }
    }

    tree
}

/// Recursively insert a path segment into the project tree.
fn insert_path<'a>(
    tree: &mut serde_json::Map<String, serde_json::Value>,
    segments: &[&'a str],
    file_name: &'a str,
    file_size: u64,
) {
    if segments.is_empty() {
        // Leaf file
        let entry = serde_json::json!({
            "name": file_name,
            "type": "file",
            "size": file_size
        });
        tree.entry("files")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap()
            .push(entry);
        return;
    }

    let dir = segments[0].to_string();
    let dirs = tree
        .entry("dirs")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .unwrap();

    let child = dirs
        .entry(dir.clone())
        .or_insert_with(|| {
            serde_json::json!({
                "name": dir,
                "type": "directory",
                "dirs": {},
                "files": []
            })
        })
        .as_object_mut()
        .unwrap();

    insert_path(child, &segments[1..], file_name, file_size);
}
