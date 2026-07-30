//! WebDAV HTTP/1.1 Server for ProxyGit
//!
//! Provides native WebDAV mount capability for macOS (mount_webdav / Finder),
//! Linux (davfs2), and Windows (Map Network Drive) without requiring kernel extensions.

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use crate::AppState;
use proxygit_common::types::{FileEntry, ProjectId};

pub async fn start_webdav_server(listen_addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let listener = TcpListener::bind(listen_addr).await?;
    info!("WebDAV HTTP server listening on http://{listen_addr}");

    loop {
        let (socket, _addr) = match listener.accept().await {
            Ok(val) => val,
            Err(e) => {
                warn!("WebDAV accept error: {e}");
                continue;
            }
        };

        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_webdav_connection(socket, state).await {
                debug!("WebDAV connection ended: {e:#}");
            }
        });
    }
}

struct HttpRequest {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        let name_lower = name.to_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == name_lower)
            .map(|(_, v)| v.as_str())
    }
}

async fn handle_webdav_connection(
    mut socket: tokio::net::TcpStream,
    state: Arc<AppState>,
) -> Result<()> {
    let (reader, mut writer) = socket.split();
    let mut reader = BufReader::new(reader);

    loop {
        let mut request_line = String::new();
        let bytes_read = reader.read_line(&mut request_line).await?;
        if bytes_read == 0 {
            break; // Connection closed
        }

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            break;
        }

        let method = parts[0].to_uppercase();
        let uri = parts[1].to_string();

        let mut headers = Vec::new();
        let mut content_length: usize = 0;

        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break; // End of headers
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim().to_string();
                let val = v.trim().to_string();
                if key.eq_ignore_ascii_case("content-length") {
                    content_length = val.parse().unwrap_or(0);
                }
                headers.push((key, val));
            }
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).await?;
        }

        let req = HttpRequest {
            method,
            uri,
            headers,
            body,
        };
        let resp = process_webdav_request(&req, &state).await;

        writer.write_all(&resp).await?;
        writer.flush().await?;

        // HTTP/1.1 Connection: close behavior for simplicity if requested
        if req
            .header("connection")
            .map(|c| c.eq_ignore_ascii_case("close"))
            .unwrap_or(false)
        {
            break;
        }
    }

    Ok(())
}

async fn process_webdav_request(req: &HttpRequest, state: &AppState) -> Vec<u8> {
    // Optional bearer token (same PROXYGIT_TOKEN as QUIC MSG_AUTH).
    if state.auth_required() {
        let presented = req
            .header("authorization")
            .and_then(proxygit_common::auth::bearer_from_authorization);
        if !state.check_token(presented) {
            return make_http_response_with_headers(
                401,
                "text/plain",
                b"Unauthorized\n",
                vec![("WWW-Authenticate", "Bearer realm=\"proxygit\"")],
            );
        }
    }

    let raw_path = req.uri.trim_start_matches('/');
    // Strip leading "webdav/" if present
    let clean_uri = if raw_path.starts_with("webdav/") {
        &raw_path[7..]
    } else {
        raw_path
    };

    let (project_str, path_str) = match clean_uri.split_once('/') {
        Some((proj, path)) => (proj, path),
        None => (clean_uri, ""),
    };

    if project_str.is_empty() {
        return make_http_response(200, "text/plain", b"ProxyGit WebDAV Server Ready\n");
    }

    let project_id = match uuid::Uuid::parse_str(project_str) {
        Ok(u) => ProjectId::from(u),
        Err(_) => {
            return make_http_response(400, "text/plain", b"Invalid Project UUID\n");
        }
    };

    let subpath = match crate::sanitize_path(path_str) {
        Ok(p) => p,
        Err(e) => return make_http_response(403, "text/plain", e.to_string().as_bytes()),
    };

    // ── Backup API Routes ──────────────────────────────────────────────
    // Handle GET /webdav/<uuid>/__backup/create, /list, /restore/<name>
    if subpath == "__backup" || subpath.starts_with("__backup/") {
        return handle_backup_request(req, state, &project_id, &subpath).await;
    }

    match req.method.as_str() {
        "OPTIONS" => {
            let headers = vec![
                ("DAV", "1, 2"),
                (
                    "Allow",
                    "OPTIONS, GET, HEAD, POST, PUT, DELETE, PROPFIND, MKCOL",
                ),
                ("MS-Author-Via", "DAV"),
            ];
            make_http_response_with_headers(200, "text/plain", b"", headers)
        }
        "PROPFIND" => handle_propfind(req, state, &project_id, &subpath).await,
        "GET" => handle_get(state, &project_id, &subpath).await,
        "HEAD" => handle_head(state, &project_id, &subpath).await,
        "PUT" => handle_put(req, state, &project_id, &subpath).await,
        "DELETE" => handle_delete(state, &project_id, &subpath).await,
        "MKCOL" => make_http_response(201, "text/plain", b"Created\n"),
        _ => make_http_response(405, "text/plain", b"Method Not Allowed\n"),
    }
}

async fn handle_get(state: &AppState, project_id: &ProjectId, subpath: &str) -> Vec<u8> {
    if subpath.is_empty() {
        let files = state.indexer.list_files(project_id).unwrap_or_default();
        let json = serde_json::to_vec_pretty(&files).unwrap_or_default();
        return make_http_response(200, "application/json", &json);
    }

    match state.indexer.stat_file(project_id, subpath) {
        Ok(Some(_entry)) => {
            match state.indexer.get_file_blocks(project_id, subpath) {
                Ok(blocks) if !blocks.is_empty() => match state.block_store.read_blocks(&blocks) {
                    Ok(content) => make_http_response(200, "application/octet-stream", &content),
                    Err(e) => make_http_response(500, "text/plain", e.to_string().as_bytes()),
                },
                Ok(_) => make_http_response(200, "application/octet-stream", b""), // Zero-byte file
                Err(e) => make_http_response(500, "text/plain", e.to_string().as_bytes()),
            }
        }
        Ok(None) => make_http_response(404, "text/plain", b"File Not Found\n"),
        Err(e) => make_http_response(500, "text/plain", e.to_string().as_bytes()),
    }
}

async fn handle_head(state: &AppState, project_id: &ProjectId, subpath: &str) -> Vec<u8> {
    match state.indexer.stat_file(project_id, subpath) {
        Ok(Some(entry)) => {
            let size_str = entry.size.to_string();
            let date_str = format_http_date(entry.mtime);
            let headers = vec![
                ("Content-Length", size_str.as_str()),
                ("Last-Modified", date_str.as_str()),
            ];
            make_http_response_with_headers_str(200, "application/octet-stream", "", headers)
        }
        Ok(None) => make_http_response(404, "text/plain", b""),
        Err(_) => make_http_response(500, "text/plain", b""),
    }
}

async fn handle_put(
    req: &HttpRequest,
    state: &AppState,
    project_id: &ProjectId,
    subpath: &str,
) -> Vec<u8> {
    if subpath.is_empty() {
        return make_http_response(400, "text/plain", b"Cannot PUT to project root\n");
    }

    let chunker = proxygit_common::cdc::CdcChunker::default_config();
    let chunks = match chunker.process_buffer(&req.body) {
        Ok(c) => c,
        Err(e) => return make_http_response(500, "text/plain", e.to_string().as_bytes()),
    };

    if let Err(e) = state.block_store.store_file(project_id, subpath, &chunks) {
        return make_http_response(500, "text/plain", e.to_string().as_bytes());
    }

    if let Err(e) = state.indexer.ingest_chunks(project_id, subpath, &chunks) {
        return make_http_response(500, "text/plain", e.to_string().as_bytes());
    }

    make_http_response(201, "text/plain", b"Created\n")
}

async fn handle_delete(state: &AppState, project_id: &ProjectId, subpath: &str) -> Vec<u8> {
    if subpath.is_empty() {
        return make_http_response(400, "text/plain", b"Cannot DELETE project root\n");
    }
    // Delete from indexer
    if let Err(e) = state.indexer.delete_file(project_id, subpath) {
        return make_http_response(500, "text/plain", e.to_string().as_bytes());
    }
    make_http_response(204, "text/plain", b"")
}

async fn handle_propfind(
    req: &HttpRequest,
    state: &AppState,
    project_id: &ProjectId,
    subpath: &str,
) -> Vec<u8> {
    let depth = req.header("depth").unwrap_or("1");
    let all_files = state.indexer.list_files(project_id).unwrap_or_default();

    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\">\n",
    );

    if subpath.is_empty() {
        // Root directory PROPFIND
        xml.push_str(&render_xml_response(
            &format!("/webdav/{project_id}/"),
            "",
            true,
            0,
            0,
        ));
        if depth != "0" {
            for f in &all_files {
                let href = format!("/webdav/{project_id}/{}", f.path);
                xml.push_str(&render_xml_response(&href, &f.path, false, f.size, f.mtime));
            }
        }
    } else {
        // Subdirectory PROPFIND — filter files under the subpath prefix
        let prefix = format!("{subpath}/");
        let matching: Vec<&FileEntry> = all_files
            .iter()
            .filter(|f| f.path.starts_with(&prefix))
            .collect();

        // Always include the directory entry itself
        xml.push_str(&render_xml_response(
            &format!("/webdav/{project_id}/{subpath}"),
            subpath,
            true,
            0,
            0,
        ));

        if depth != "0" {
            for f in &matching {
                let href = format!("/webdav/{project_id}/{}", f.path);
                xml.push_str(&render_xml_response(&href, &f.path, false, f.size, f.mtime));
            }
        }
    }

    xml.push_str("</d:multistatus>\n");
    make_http_response_with_headers_str(
        207,
        "application/xml; charset=\"utf-8\"",
        &xml,
        vec![("DAV", "1, 2")],
    )
}

fn render_xml_response(href: &str, name: &str, is_dir: bool, size: u64, mtime: u64) -> String {
    let resourcetype = if is_dir {
        "<d:resourcetype><d:collection/></d:resourcetype>"
    } else {
        "<d:resourcetype/>"
    };
    let date_str = format_http_date(mtime);
    format!(
        "  <d:response>\n\
         \x20   <d:href>{href}</d:href>\n\
         \x20   <d:propstat>\n\
         \x20     <d:prop>\n\
         \x20       <d:displayname>{name}</d:displayname>\n\
         \x20       <d:getcontentlength>{size}</d:getcontentlength>\n\
         \x20       <d:getlastmodified>{date_str}</d:getlastmodified>\n\
         \x20       {resourcetype}\n\
         \x20     </d:prop>\n\
         \x20     <d:status>HTTP/1.1 200 OK</d:status>\n\
         \x20   </d:propstat>\n\
         \x20 </d:response>\n"
    )
}

fn format_http_date(timestamp: u64) -> String {
    let dt = std::time::UNIX_EPOCH + std::time::Duration::from_secs(timestamp);
    format!("{dt:?}")
}

fn make_http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    make_http_response_with_headers(status, content_type, body, vec![])
}

fn make_http_response_with_headers_str(
    status: u16,
    content_type: &str,
    body_str: &str,
    headers: Vec<(&str, &str)>,
) -> Vec<u8> {
    make_http_response_with_headers(status, content_type, body_str.as_bytes(), headers)
}

fn make_http_response_with_headers(
    status: u16,
    content_type: &str,
    body: &[u8],
    headers: Vec<(&str, &str)>,
) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        207 => "Multi-Status",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Unknown",
    };

    let mut resp = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Server: ProxyGit-WebDAV/0.1.0\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n",
        body.len()
    );

    for (k, v) in headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }

    resp.push_str("\r\n");

    let mut out = resp.into_bytes();
    out.extend_from_slice(body);
    out
}

// ── Backup API Handlers ──────────────────────────────────────────────

/// Route backup requests within the WebDAV endpoint.
/// GET /webdav/<uuid>/__backup/{create,list,restore/<name>}
async fn handle_backup_request(
    req: &HttpRequest,
    state: &AppState,
    project_id: &ProjectId,
    subpath: &str,
) -> Vec<u8> {
    // GET for create/list, POST for restore
    if req.method != "GET" && req.method != "POST" {
        return make_http_response(405, "text/plain", b"Method Not Allowed\n");
    }

    // Strip the "__backup/" prefix
    let action = if subpath == "__backup" {
        ""
    } else {
        subpath.strip_prefix("__backup/").unwrap_or("")
    };

    match action {
        "" | "list" => {
            // List backups
            match crate::list_project_backups(&state.data_dir, project_id) {
                Ok(backups) => {
                    let body = serde_json::to_vec_pretty(&backups).unwrap_or_default();
                    make_http_response(200, "application/json", &body)
                }
                Err(e) => make_http_response(500, "text/plain", format!("{e:#}").as_bytes()),
            }
        }
        "create" => {
            // Create a backup
            match crate::create_project_backup(
                &state.data_dir,
                project_id,
                &state.indexer,
                &state.block_store,
            ) {
                Ok((name, size)) => {
                    let json = serde_json::json!({ "name": name, "size": size });
                    let body = serde_json::to_vec_pretty(&json).unwrap_or_default();
                    make_http_response(200, "application/json", &body)
                }
                Err(e) => make_http_response(500, "text/plain", format!("{e:#}").as_bytes()),
            }
        }
        rest if rest.starts_with("restore/") => {
            let backup_name = rest.strip_prefix("restore/").unwrap_or("");
            if backup_name.is_empty() {
                return make_http_response(400, "text/plain", b"Missing backup name\n");
            }
            // Sanitize backup name — prevent path traversal
            if backup_name.contains('/') || backup_name.contains("..") {
                return make_http_response(403, "text/plain", b"Invalid backup name\n");
            }
            match crate::restore_project_backup(
                &state.data_dir,
                project_id,
                backup_name,
                &state.indexer,
                &state.block_store,
            ) {
                Ok(()) => make_http_response(200, "text/plain", b"Restored successfully\n"),
                Err(e) => make_http_response(500, "text/plain", format!("{e:#}").as_bytes()),
            }
        }
        _ => make_http_response(404, "text/plain", b"Unknown backup action\n"),
    }
}
