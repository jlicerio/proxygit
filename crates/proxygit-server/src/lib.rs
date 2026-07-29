pub mod block_store;
pub mod embeddings;
pub mod indexer;
pub mod webdav;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use quinn::Endpoint;
use tar::Archive as TarArchive;
use tar::Builder as TarBuilder;
use tracing::{info, warn};

use proxygit_common::protocol::*;
use proxygit_common::types::ProjectId;

pub struct AppState {
    pub data_dir: PathBuf,
    pub indexer: indexer::ProjectIndexer,
    pub block_store: block_store::BlockStore,
    pub embeddings: Arc<Mutex<embeddings::EmbeddingIndex>>,
}

pub fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = bytes[i + 1];
            let h2 = bytes[i + 2];
            let d1 = hex_nibble(h1);
            let d2 = hex_nibble(h2);
            if let (Some(a), Some(b)) = (d1, d2) {
                out.push((a << 4) | b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).context("Path is not valid UTF-8 after percent-decoding")
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn sanitize_path(raw: &str) -> Result<String> {
    // Decode percent-encoding first so '%2e%2e' cannot bypass the '..' check.
    let decoded = percent_decode(raw)?;
    let clean = decoded.trim_start_matches('/');
    if clean.contains("..") {
        bail!("Path traversal attempt rejected: {raw}");
    }
    Ok(clean.to_string())
}

pub async fn run_server(listen_addr: SocketAddr, data_dir: PathBuf) -> Result<()> {
    let index_dir = data_dir.join("indexes");
    let blocks_dir = data_dir.join("blocks");

    info!("ProxyGit Server");
    info!("Index directory: {}", index_dir.display());
    info!("Block store:     {}", blocks_dir.display());
    info!("Listening on:    {}", listen_addr);

    let mut embeddings_idx = embeddings::EmbeddingIndex::new(&data_dir);
    embeddings_idx.load()?;

    let state = Arc::new(AppState {
        data_dir: data_dir.clone(),
        indexer: indexer::ProjectIndexer::new(&index_dir)?,
        block_store: block_store::BlockStore::new(&blocks_dir)?,
        embeddings: Arc::new(Mutex::new(embeddings_idx)),
    });

    let webdav_addr: SocketAddr = std::env::var("PROXYGIT_WEBDAV_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:3900".into())
        .parse()
        .context("Invalid PROXYGIT_WEBDAV_LISTEN address")?;

    let webdav_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = webdav::start_webdav_server(webdav_addr, webdav_state).await {
            warn!("WebDAV server error: {e:#}");
        }
    });

    let server_config = make_server_config(&data_dir)?;
    let endpoint = Endpoint::server(server_config, listen_addr)?;

    info!("QUIC endpoint ready on {listen_addr}");
    info!("WebDAV HTTP server ready on http://{webdav_addr}");
    while let Some(conn) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(conn, state).await {
                warn!("Connection error: {e:#}");
            }
        });
    }

    Ok(())
}

pub fn make_server_config(data_dir: &Path) -> Result<quinn::ServerConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert_path = data_dir.join("server_cert.der");
    let key_path = data_dir.join("server_key.der");

    let (cert_der, key): (
        rustls::pki_types::CertificateDer,
        rustls::pki_types::PrivateKeyDer,
    ) = if cert_path.exists() && key_path.exists() {
        info!(
            "Loading existing TLS certificate from {}",
            cert_path.display()
        );
        let cert_bytes = std::fs::read(&cert_path)?;
        let key_bytes = std::fs::read(&key_path)?;
        (
            cert_bytes.into(),
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                key_bytes,
            )),
        )
    } else {
        info!("Generating new self-signed TLS certificate");
        let key_pair = rcgen::KeyPair::generate()?;
        let mut cert_params = rcgen::CertificateParams::default();
        cert_params.subject_alt_names = vec![
            rcgen::SanType::DnsName("proxygit-server".try_into()?),
            rcgen::SanType::DnsName("localhost".try_into()?),
            rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
        ];
        let cert = cert_params.self_signed(&key_pair)?;
        let cert_bytes = cert.der().to_vec();
        let key_bytes = key_pair.serialize_der();

        std::fs::create_dir_all(data_dir)?;
        std::fs::write(&cert_path, &cert_bytes)?;
        std::fs::write(&key_path, &key_bytes)?;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;

        (
            cert_bytes.into(),
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                key_bytes,
            )),
        )
    };

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key)?;

    tls_config.max_early_data_size = 0;
    tls_config.alpn_protocols = vec![b"proxygit-1".to_vec()];

    let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic_config)))
}

pub async fn handle_connection(incoming: quinn::Incoming, state: Arc<AppState>) -> Result<()> {
    let conn = incoming.await?;
    let remote = conn.remote_address();
    info!("New connection from {remote}");

    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_bi_stream(send, recv, state).await {
                        warn!("Stream error from {remote}: {e:#}");
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                info!("Connection from {remote} closed");
                break;
            }
            Err(e) => {
                warn!("Connection from {remote} error: {e}");
                break;
            }
        }
    }

    Ok(())
}

pub async fn handle_bi_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    state: Arc<AppState>,
) -> Result<()> {
    let frame = recv_frame(&mut recv).await?;

    match frame.msg_type {
        MSG_LIST_PROJECT => {
            handle_list_project(frame, &mut send, &state).await?;
        }
        MSG_READ_FILE => {
            handle_read_file(frame, &mut send, &state).await?;
        }
        MSG_WRITE_BLOCKS => {
            handle_write_blocks(frame, &mut send, &state).await?;
        }
        MSG_STAT_FILE => {
            handle_stat_file(frame, &mut send, &state).await?;
        }
        MSG_BLOCK_REQUEST => {
            handle_block_request(frame, &mut send, &state).await?;
        }
        MSG_GET_PROJECT_MAP => {
            handle_get_project_map(frame, &mut send, &state).await?;
        }
        MSG_SEMANTIC_SEARCH => {
            handle_semantic_search(frame, &mut send, &state).await?;
        }
        MSG_CREATE_BACKUP => {
            handle_create_backup(frame, &mut send, &state).await?;
        }
        MSG_LIST_BACKUPS => {
            handle_list_backups(frame, &mut send, &state).await?;
        }
        other => {
            warn!("Unknown message type: 0x{other:02x}");
            send_frame(
                &mut send,
                MSG_ERROR,
                0,
                &[0u8; 32],
                format!("Unknown message type: 0x{other:02x}").as_bytes(),
            )
            .await?;
        }
    }

    Ok(())
}

pub async fn handle_list_project(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let project_id = ProjectId::from_u128(frame.project_id);
    let list = state.indexer.list_files(&project_id).unwrap_or_default();
    let json = serde_json::to_vec(&list)?;
    let hash = blake3::hash(&json).into();
    send_frame(send, MSG_LIST_PROJECT_RESP, frame.project_id, &hash, &json).await
}

pub async fn handle_read_file(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let raw_path = String::from_utf8_lossy(&frame.payload).to_string();
    let path = match sanitize_path(&raw_path) {
        Ok(p) => p,
        Err(e) => {
            return send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                e.to_string().as_bytes(),
            )
            .await;
        }
    };
    let project_id = ProjectId::from_u128(frame.project_id);

    // First check if file exists in SQLite index
    let entry = match state.indexer.stat_file(&project_id, &path) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                format!("File not found: {path}").as_bytes(),
            )
            .await;
        }
        Err(e) => {
            return send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                format!("Database error: {e}").as_bytes(),
            )
            .await;
        }
    };

    // If zero-byte file, return empty payload
    let data = if entry.size == 0 {
        Vec::new()
    } else {
        match state.indexer.get_file_blocks(&project_id, &path) {
            Ok(blocks) if !blocks.is_empty() => match state.block_store.read_blocks(&blocks) {
                Ok(content) => content,
                Err(e) => {
                    return send_frame(
                        send,
                        MSG_ERROR,
                        frame.project_id,
                        &[0u8; 32],
                        format!("Block read error: {e}").as_bytes(),
                    )
                    .await;
                }
            },
            Ok(_) => {
                return send_frame(
                    send,
                    MSG_ERROR,
                    frame.project_id,
                    &[0u8; 32],
                    b"Storage corruption: non-empty file has no blocks",
                )
                .await;
            }
            Err(e) => {
                return send_frame(
                    send,
                    MSG_ERROR,
                    frame.project_id,
                    &[0u8; 32],
                    format!("Index lookup error: {e}").as_bytes(),
                )
                .await;
            }
        }
    };

    // Verify assembled content against the index tree_hash (content blake3).
    let hash = blake3::hash(&data);
    let actual_hash = hash.to_hex().to_string();
    if actual_hash != entry.tree_hash {
        return send_frame(
            send,
            MSG_ERROR,
            frame.project_id,
            &[0u8; 32],
            b"Block content corruption detected",
        )
        .await;
    }

    let hash_bytes: [u8; 32] = hash.into();
    send_frame(
        send,
        MSG_READ_FILE_RESP,
        frame.project_id,
        &hash_bytes,
        &data,
    )
    .await
}

pub async fn handle_write_blocks(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let (raw_path, content) = match decode_write_payload(&frame.payload) {
        Ok((p, c)) => (p, c),
        Err(e) => {
            return send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                format!("Invalid payload: {e}").as_bytes(),
            )
            .await;
        }
    };

    let path = match sanitize_path(&raw_path) {
        Ok(p) => p,
        Err(e) => {
            return send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                e.to_string().as_bytes(),
            )
            .await;
        }
    };

    let project_id = ProjectId::from_u128(frame.project_id);

    // Chunk once, store blocks (content-addressed, idempotent), then index.
    let chunker = proxygit_common::cdc::CdcChunker::default_config();
    let chunks = chunker.process_buffer(content)?;
    state.block_store.store_file(&project_id, &path, &chunks)?;
    state.indexer.ingest_chunks(&project_id, &path, &chunks)?;

    // Update embedding index for semantic search.
    let embedding = embeddings::compute_embedding(content);
    {
        let mut idx = state.embeddings.lock().unwrap();
        idx.set(&path, embedding);
        if let Err(e) = idx.save() {
            warn!("Failed to save embeddings: {e}");
        }
    }

    let tree_hash = blake3::hash(content);
    let ack = serde_json::json!({ "path": path, "tree_hash": tree_hash.to_hex().to_string() });
    let payload = serde_json::to_vec(&ack)?;
    let hash = blake3::hash(&payload).into();
    send_frame(send, MSG_WRITE_ACK, frame.project_id, &hash, &payload).await
}

pub async fn handle_stat_file(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let raw_path = String::from_utf8_lossy(&frame.payload).to_string();
    let path = match sanitize_path(&raw_path) {
        Ok(p) => p,
        Err(e) => {
            return send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                e.to_string().as_bytes(),
            )
            .await;
        }
    };

    let project_id = ProjectId::from_u128(frame.project_id);

    match state.indexer.stat_file(&project_id, &path) {
        Ok(Some(entry)) => {
            let json = serde_json::to_vec(&entry)?;
            let hash = blake3::hash(&json).into();
            send_frame(send, MSG_STAT_FILE_RESP, frame.project_id, &hash, &json).await
        }
        Ok(None) => {
            send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                format!("File not found: {path}").as_bytes(),
            )
            .await
        }
        Err(e) => {
            send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                format!("Server error: {e}").as_bytes(),
            )
            .await
        }
    }
}

pub async fn handle_block_request(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let block_hash: [u8; 32] = frame.hash;
    match state.block_store.get_block(&block_hash) {
        Some(data) => {
            let hash = blake3::hash(&data).into();
            send_frame(send, MSG_BLOCK_RESP, frame.project_id, &hash, &data).await
        }
        None => {
            send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                b"block not found",
            )
            .await
        }
    }
}

/// Handle GET_PROJECT_MAP — returns the full file listing with sizes in a single response.
pub async fn handle_get_project_map(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let project_id = ProjectId::from_u128(frame.project_id);
    let entries = state.indexer.list_files(&project_id).unwrap_or_default();
    let json = serde_json::to_vec(&entries)?;
    let hash = blake3::hash(&json).into();
    send_frame(
        send,
        MSG_GET_PROJECT_MAP_RESP,
        frame.project_id,
        &hash,
        &json,
    )
    .await
}

/// Handle SEMANTIC_SEARCH — find files whose content is similar to the query.
///
/// The payload is the query text. Returns a JSON array of `{"path", "score"}`
/// objects sorted by descending similarity (cosine).
pub async fn handle_semantic_search(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let query = String::from_utf8_lossy(&frame.payload).to_string();
    let query_embedding = embeddings::compute_embedding(query.as_bytes());

    let results = {
        let idx = state.embeddings.lock().unwrap();
        idx.search(&query_embedding, 20)
    };

    let json_results: Vec<String> = results.into_iter().map(|(path, _score)| path).collect();
    let payload = serde_json::to_vec(&json_results)?;
    let hash = blake3::hash(&payload).into();

    send_frame(
        send,
        MSG_SEMANTIC_SEARCH_RESP,
        frame.project_id,
        &hash,
        &payload,
    )
    .await
}

// ── Backup Handlers ─────────────────────────────────────────────────

/// Core backup creation logic (shared by QUIC and WebDAV handlers).
/// Creates a tar.gz of every project file, stored at
/// `data_dir/backups/<project_uuid>/<timestamp>.tar.gz`.
pub fn create_project_backup(
    data_dir: &Path,
    project_id: &ProjectId,
    indexer: &indexer::ProjectIndexer,
    block_store: &block_store::BlockStore,
) -> Result<(String, u64)> {
    let backups_dir = data_dir.join("backups").join(project_id.to_string());
    std::fs::create_dir_all(&backups_dir)?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let backup_name = format!("{timestamp}.tar.gz");

    // Write to a temp directory first, then atomically move
    let tmp_dir = data_dir.join("backups").join(".tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_path = tmp_dir.join(&backup_name);

    // Collect all files from the indexer
    let files = indexer.list_files(project_id)?;

    // Build the tar.gz
    let tmp_file = std::fs::File::create(&tmp_path)?;
    let gz_enc = GzEncoder::new(tmp_file, Compression::default());
    let mut tar = TarBuilder::new(gz_enc);

    for entry in &files {
        let blocks = indexer.get_file_blocks(project_id, &entry.path)?;
        let content: Vec<u8> = if blocks.is_empty() {
            Vec::new()
        } else {
            block_store.read_blocks(&blocks)?
        };

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(entry.mode);
        header.set_mtime(entry.mtime);
        header.set_size(content.len() as u64);
        // Use the relative path directly (already a cleaned project-relative path)
        tar.append_data(&mut header, &entry.path, &content[..])
            .with_context(|| format!("Failed to append {} to backup archive", entry.path))?;
    }

    // Finish writing
    let gz_enc = tar.into_inner()?;
    let tmp_file = gz_enc.finish()?;
    tmp_file.sync_all()?;

    // Atomically move to final location
    let final_path = backups_dir.join(&backup_name);
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("Failed to rename backup to {}", final_path.display()))?;

    // fsync parent directory
    if let Some(parent) = final_path.parent() {
        if let Ok(parent_f) = std::fs::File::open(parent) {
            parent_f.sync_all()?;
        }
    }

    let size = std::fs::metadata(&final_path)?.len();
    Ok((backup_name, size))
}

/// QUIC handler: create a backup for a project.
pub async fn handle_create_backup(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let project_id = ProjectId::from_u128(frame.project_id);
    match create_project_backup(
        &state.data_dir,
        &project_id,
        &state.indexer,
        &state.block_store,
    ) {
        Ok((name, size)) => {
            let json = serde_json::json!({ "name": name, "size": size });
            let payload = serde_json::to_vec(&json)?;
            let hash = blake3::hash(&payload).into();
            send_frame(send, MSG_CREATE_BACKUP, frame.project_id, &hash, &payload).await
        }
        Err(e) => {
            send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                format!("Backup failed: {e}").as_bytes(),
            )
            .await
        }
    }
}

/// List available backups for a project.
pub fn list_project_backups(
    data_dir: &Path,
    project_id: &ProjectId,
) -> Result<Vec<serde_json::Value>> {
    let backups_dir = data_dir.join("backups").join(project_id.to_string());
    if !backups_dir.exists() {
        return Ok(Vec::new());
    }

    let mut backups: Vec<serde_json::Value> = Vec::new();
    for entry in std::fs::read_dir(&backups_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("gz")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .map_or(false, |s| s.ends_with(".tar"))
        {
            let name = entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".tar.gz")
                .to_string();
            let size = entry.metadata()?.len();
            backups.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "timestamp": name,
                "size": size,
            }));
        }
    }
    // Sort by timestamp ascending
    backups.sort_by(|a, b| {
        a["timestamp"]
            .as_str()
            .unwrap_or("")
            .cmp(b["timestamp"].as_str().unwrap_or(""))
    });

    Ok(backups)
}

/// QUIC handler: list backups for a project.
pub async fn handle_list_backups(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let project_id = ProjectId::from_u128(frame.project_id);
    match list_project_backups(&state.data_dir, &project_id) {
        Ok(backups) => {
            let payload = serde_json::to_vec(&backups)?;
            let hash = blake3::hash(&payload).into();
            send_frame(send, MSG_LIST_BACKUPS, frame.project_id, &hash, &payload).await
        }
        Err(e) => {
            send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                format!("List backups failed: {e}").as_bytes(),
            )
            .await
        }
    }
}

/// Core backup restore logic — extracts a tar.gz backup and rewrites the project.
pub fn restore_project_backup(
    data_dir: &Path,
    project_id: &ProjectId,
    backup_name: &str,
    indexer: &indexer::ProjectIndexer,
    block_store: &block_store::BlockStore,
) -> Result<()> {
    let backup_path = data_dir
        .join("backups")
        .join(project_id.to_string())
        .join(backup_name);
    if !backup_path.exists() {
        anyhow::bail!("Backup not found: {}", backup_path.display());
    }

    // Extract to a temp restore directory
    let tmp_restore = data_dir
        .join("backups")
        .join(".tmp_restore")
        .join(project_id.to_string());
    if tmp_restore.exists() {
        std::fs::remove_dir_all(&tmp_restore)?;
    }
    std::fs::create_dir_all(&tmp_restore)?;

    let backup_file = std::fs::File::open(&backup_path)?;
    let gz_dec = GzDecoder::new(backup_file);
    let mut archive = TarArchive::new(gz_dec);
    archive.unpack(&tmp_restore)?;

    let chunker = proxygit_common::cdc::CdcChunker::default_config();

    // Walk extracted files, chunk and re-index each one
    let walker = walkdir::WalkDir::new(&tmp_restore).min_depth(1);
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let rel_path = entry
            .path()
            .strip_prefix(&tmp_restore)
            .with_context(|| "Failed to compute relative path")?;
        let path_str = rel_path.to_string_lossy().to_string();

        let content = std::fs::read(entry.path())?;

        let chunks = chunker.process_buffer(&content)?;
        block_store.store_file(project_id, &path_str, &chunks)?;
        indexer.ingest_chunks(project_id, &path_str, &chunks)?;
    }

    // Clean up temp restore directory
    let _ = std::fs::remove_dir_all(&tmp_restore);

    Ok(())
}

/// QUIC handler: restore a project from a named backup.
pub async fn handle_restore_backup(
    frame: Frame,
    send: &mut quinn::SendStream,
    state: &AppState,
) -> Result<()> {
    let project_id = ProjectId::from_u128(frame.project_id);
    let backup_name = String::from_utf8_lossy(&frame.payload).to_string();
    match restore_project_backup(
        &state.data_dir,
        &project_id,
        &backup_name,
        &state.indexer,
        &state.block_store,
    ) {
        Ok(()) => {
            let payload = format!("Restored from {backup_name}");
            let hash = blake3::hash(payload.as_bytes()).into();
            send_frame(
                send,
                MSG_CREATE_BACKUP,
                frame.project_id,
                &hash,
                payload.as_bytes(),
            )
            .await
        }
        Err(e) => {
            send_frame(
                send,
                MSG_ERROR,
                frame.project_id,
                &[0u8; 32],
                format!("Restore failed: {e}").as_bytes(),
            )
            .await
        }
    }
}
