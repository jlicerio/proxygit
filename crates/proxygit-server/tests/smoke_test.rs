use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use proxygit_client::fuse_mount::{build_cache_path, is_build_path};
use proxygit_client::wal::LocalWal;
use uuid::Uuid;

#[tokio::test]
async fn smoke_test_m1_to_m6_end_to_end() -> Result<()> {
    // Ensure lab defaults: no auth / last-writer-wins (other tests may set these).
    std::env::remove_var("PROXYGIT_TOKEN");
    std::env::remove_var("PROXYGIT_TOKEN_FILE");
    std::env::remove_var("PROXYGIT_WRITE_CONFLICT");

    // 1. Setup temp data dir for server and client
    let temp_dir = tempfile::tempdir()?;
    let data_dir = temp_dir.path().to_path_buf();
    let listen_addr: SocketAddr = "127.0.0.1:18080".parse()?;
    std::env::set_var("PROXYGIT_WEBDAV_LISTEN", "127.0.0.1:13900");

    // 2. Spawn real production server (M1)
    let server_data_dir = data_dir.clone();
    let server_handle =
        tokio::spawn(
            async move { proxygit_server::run_server(listen_addr, server_data_dir).await },
        );

    // Give server time to bind and generate cert
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 3. Connect real production client (M2)
    let cert_path = data_dir.join("server_cert.der");
    assert!(
        cert_path.exists(),
        "Server should persist server_cert.der on startup"
    );

    let conn = proxygit_client::connect_to_server_with_cert(listen_addr, Some(cert_path)).await?;
    let pool = proxygit_client::StreamPool::new(conn.clone(), 8);
    let project_id = Uuid::new_v4();

    // 4. Test M3: Binary & Zero-Byte Read/Write
    let binary_data = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF, 0xFE, 0x80,
    ];
    let write_res =
        proxygit_client::mcp_write_file(&pool, project_id, "src/logo.png", &binary_data).await;
    assert_eq!(write_res["status"], "ok");

    let read_bytes = proxygit_client::mcp_read_raw_bytes(&pool, project_id, "src/logo.png").await?;
    assert_eq!(read_bytes, binary_data);

    let empty_write_res = proxygit_client::mcp_write_file(&pool, project_id, ".gitkeep", &[]).await;
    assert_eq!(empty_write_res["status"], "ok");

    let empty_read_bytes =
        proxygit_client::mcp_read_raw_bytes(&pool, project_id, ".gitkeep").await?;
    assert!(empty_read_bytes.is_empty());

    // 5. Test M4: NVMe WAL Append + Async Worker Flush
    let wal_dir = data_dir.join("wal");
    let wal = Arc::new(LocalWal::new(&wal_dir)?);

    // Append write entry to local WAL log with actual file path
    wal.append_entry("src/wal_test.rs", 0, b"fn wal_demo() {}")
        .await?;

    // Start background WAL flush worker (which rotates, flushes, and deletes stage file)
    let flush_handle = proxygit_client::wal::start_wal_flush_worker(
        wal.clone(),
        conn.clone(),
        project_id,
        data_dir.join("cache"),
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    // Read back flushed WAL file from server
    let flushed_bytes =
        proxygit_client::mcp_read_raw_bytes(&pool, project_id, "src/wal_test.rs").await?;
    assert_eq!(flushed_bytes, b"fn wal_demo() {}");
    flush_handle.abort();

    // 6. Test M5: Build Directory Interception Rules
    assert!(is_build_path("/target/debug/main.o"));
    assert!(is_build_path("/node_modules/express/index.js"));
    assert!(!is_build_path("/src/main.rs"));

    let cache_dir = data_dir.join("build_cache");
    let intercepted_path = build_cache_path(&project_id, "/target/debug/main.o", &cache_dir);
    assert!(intercepted_path.starts_with(cache_dir.join(project_id.to_string())));
    assert!(intercepted_path.ends_with("debug/main.o"));

    // Write build artifact directly to local NVMe build cache
    std::fs::create_dir_all(intercepted_path.parent().unwrap())?;
    std::fs::write(&intercepted_path, b"compiled binary output")?;
    assert_eq!(std::fs::read(&intercepted_path)?, b"compiled binary output");

    // Test nested Tauri workspace files ingestion
    let tauri_cargo = proxygit_client::mcp_write_file(
        &pool,
        project_id,
        "src-tauri/Cargo.toml",
        b"[package]\nname = \"tauri-app\"",
    )
    .await;
    assert_eq!(tauri_cargo["status"], "ok");

    let tauri_main = proxygit_client::mcp_write_file(
        &pool,
        project_id,
        "src-tauri/src/main.rs",
        b"fn main() {}",
    )
    .await;
    assert_eq!(tauri_main["status"], "ok");

    let react_app = proxygit_client::mcp_write_file(
        &pool,
        project_id,
        "src/App.tsx",
        b"export function App() { return <div>Tauri</div>; }",
    )
    .await;
    assert_eq!(react_app["status"], "ok");
    // 7. Test M6: Official Anthropic MCP JSON-RPC 2.0 Protocol Format (Initialize + Tools List + Tools Call)
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test-agent", "version": "1.0.0" }
        }
    })
    .to_string();
    let init_resp = proxygit_client::handle_mcp_jsonrpc_request(&init_req, &pool, project_id).await;
    assert_eq!(init_resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "proxygit-mcp");

    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    })
    .to_string();
    let list_resp = proxygit_client::handle_mcp_jsonrpc_request(&list_req, &pool, project_id).await;
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert!(tools.iter().any(|t| t["name"] == "read_file"));
    assert!(tools.iter().any(|t| t["name"] == "write_file"));

    let call_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "read_file",
            "arguments": { "path": "src-tauri/Cargo.toml" }
        }
    })
    .to_string();
    let call_resp = proxygit_client::handle_mcp_jsonrpc_request(&call_req, &pool, project_id).await;
    let content_text = call_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(content_text.contains("tauri-app"));

    // 8. Test WebDAV HTTP Server Endpoints (GET, PROPFIND, PUT)
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // GET file via WebDAV
    let mut webdav_stream = tokio::net::TcpStream::connect("127.0.0.1:13900").await?;
    let req = format!(
        "GET /webdav/{project_id}/src-tauri/Cargo.toml HTTP/1.1\r\nHost: 127.0.0.1:13900\r\nConnection: close\r\n\r\n"
    );
    webdav_stream.write_all(req.as_bytes()).await?;
    let mut resp_buf = Vec::new();
    webdav_stream.read_to_end(&mut resp_buf).await?;
    let resp_str = String::from_utf8_lossy(&resp_buf);
    assert!(resp_str.contains("HTTP/1.1 200 OK"));
    assert!(resp_str.contains("tauri-app"));

    // PROPFIND directory via WebDAV
    let mut propfind_stream = tokio::net::TcpStream::connect("127.0.0.1:13900").await?;
    let req = format!(
        "PROPFIND /webdav/{project_id}/ HTTP/1.1\r\nHost: 127.0.0.1:13900\r\nDepth: 1\r\nConnection: close\r\n\r\n"
    );
    propfind_stream.write_all(req.as_bytes()).await?;
    let mut resp_buf = Vec::new();
    propfind_stream.read_to_end(&mut resp_buf).await?;
    let resp_str = String::from_utf8_lossy(&resp_buf);
    assert!(resp_str.contains("207 Multi-Status"));
    assert!(resp_str.contains("src-tauri/Cargo.toml"));

    // PUT new file via WebDAV
    let mut put_stream = tokio::net::TcpStream::connect("127.0.0.1:13900").await?;
    let body = b"hello from webdav put";
    let req = format!(
        "PUT /webdav/{project_id}/webdav_test.txt HTTP/1.1\r\nHost: 127.0.0.1:13900\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    put_stream.write_all(req.as_bytes()).await?;
    put_stream.write_all(body).await?;
    let mut resp_buf = Vec::new();
    put_stream.read_to_end(&mut resp_buf).await?;
    let resp_str = String::from_utf8_lossy(&resp_buf);
    assert!(resp_str.contains("201 Created"));

    // Verify put file via QUIC read
    let read_back =
        proxygit_client::mcp_read_raw_bytes(&pool, project_id, "webdav_test.txt").await?;
    assert_eq!(read_back, body);

    conn.close(0u32.into(), b"done");
    server_handle.abort();

    Ok(())
}
