//! Phase B/C: token auth + optimistic concurrency.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use uuid::Uuid;

fn free_port() -> u16 {
    let s = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_token_required_when_enabled() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().to_path_buf();
    let quic_port = free_port();
    let dav_port = free_port();
    let listen: SocketAddr = format!("127.0.0.1:{quic_port}").parse()?;

    std::env::set_var("PROXYGIT_WEBDAV_LISTEN", format!("127.0.0.1:{dav_port}"));
    std::env::remove_var("PROXYGIT_MTLS_CA");
    std::env::remove_var("PROXYGIT_CLIENT_CERT");
    std::env::remove_var("PROXYGIT_CLIENT_KEY");
    std::env::set_var("PROXYGIT_TOKEN", "test-secret-token");
    std::env::remove_var("PROXYGIT_WRITE_CONFLICT");

    let server_data = data_dir.clone();
    let handle =
        tokio::spawn(async move { proxygit_server::run_server(listen, server_data).await });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cert = data_dir.join("server_cert.der");
    assert!(cert.exists(), "missing server cert");

    // Wrong token rejected at connect/auth handshake
    std::env::set_var("PROXYGIT_TOKEN", "wrong-token");
    let bad = proxygit_client::connect_to_server_with_cert(listen, Some(cert.clone())).await;
    assert!(bad.is_err(), "wrong token must fail auth: {bad:?}");

    // Correct token
    std::env::set_var("PROXYGIT_TOKEN", "test-secret-token");
    let conn = proxygit_client::connect_to_server_with_cert(listen, Some(cert)).await?;
    let pool = proxygit_client::StreamPool::new(conn, 4);
    let project = Uuid::new_v4();
    let res = proxygit_client::mcp_write_file(&pool, project, "ok.txt", b"hello").await;
    assert_eq!(res["status"], "ok", "{res}");

    // WebDAV 401 without bearer
    let code = curl_status(&format!(
        "http://127.0.0.1:{dav_port}/webdav/{project}/ok.txt"
    ))?;
    assert_eq!(code, 401, "expected 401 without bearer");

    // WebDAV 200 with bearer
    let status = curl_status_with_auth(
        &format!("http://127.0.0.1:{dav_port}/webdav/{project}/ok.txt"),
        "test-secret-token",
    )?;
    assert_eq!(status, 200, "expected 200 with bearer");

    handle.abort();
    std::env::remove_var("PROXYGIT_TOKEN");
    std::env::remove_var("PROXYGIT_WEBDAV_LISTEN");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reject_stale_write_returns_conflict() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().to_path_buf();
    let quic_port = free_port();
    let dav_port = free_port();
    let listen: SocketAddr = format!("127.0.0.1:{quic_port}").parse()?;

    std::env::set_var("PROXYGIT_WEBDAV_LISTEN", format!("127.0.0.1:{dav_port}"));
    std::env::remove_var("PROXYGIT_TOKEN");
    std::env::remove_var("PROXYGIT_MTLS_CA");
    std::env::remove_var("PROXYGIT_CLIENT_CERT");
    std::env::remove_var("PROXYGIT_CLIENT_KEY");
    std::env::set_var("PROXYGIT_WRITE_CONFLICT", "reject_stale");

    let server_data = data_dir.clone();
    let handle =
        tokio::spawn(async move { proxygit_server::run_server(listen, server_data).await });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cert = data_dir.join("server_cert.der");
    let conn = proxygit_client::connect_to_server_with_cert(listen, Some(cert)).await?;
    let pool = proxygit_client::StreamPool::new(conn, 4);
    let project = Uuid::new_v4();

    let w1 = proxygit_client::mcp_write_file(&pool, project, "c.txt", b"version-a").await;
    assert_eq!(w1["status"], "ok", "{w1}");
    let base = w1["tree_hash"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing tree_hash in {w1}"))?
        .to_string();
    let base_bytes = hex_to_32(&base)?;

    let stale = proxygit_client::mcp_write_file_opts(
        &pool,
        project,
        "c.txt",
        b"version-b",
        Some([0u8; 32]),
    )
    .await;
    assert_eq!(
        stale.get("error").and_then(|e| e.as_str()),
        Some("conflict"),
        "{stale}"
    );
    assert!(stale.get("server_hash").is_some(), "{stale}");

    let ok = proxygit_client::mcp_write_file_opts(
        &pool,
        project,
        "c.txt",
        b"version-b",
        Some(base_bytes),
    )
    .await;
    assert_eq!(ok["status"], "ok", "{ok}");

    handle.abort();
    std::env::remove_var("PROXYGIT_WRITE_CONFLICT");
    std::env::remove_var("PROXYGIT_WEBDAV_LISTEN");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reject_stale_auto_base_hash_on_plain_write() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().to_path_buf();
    let quic_port = free_port();
    let dav_port = free_port();
    let listen: SocketAddr = format!("127.0.0.1:{quic_port}").parse()?;

    std::env::set_var("PROXYGIT_WEBDAV_LISTEN", format!("127.0.0.1:{dav_port}"));
    std::env::remove_var("PROXYGIT_TOKEN");
    std::env::remove_var("PROXYGIT_MTLS_CA");
    std::env::remove_var("PROXYGIT_CLIENT_CERT");
    std::env::remove_var("PROXYGIT_CLIENT_KEY");
    std::env::set_var("PROXYGIT_WRITE_CONFLICT", "reject_stale");

    let server_data = data_dir.clone();
    let handle =
        tokio::spawn(async move { proxygit_server::run_server(listen, server_data).await });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cert = data_dir.join("server_cert.der");
    let conn = proxygit_client::connect_to_server_with_cert(listen, Some(cert)).await?;
    let pool = proxygit_client::StreamPool::new(conn, 4);
    let project = Uuid::new_v4();

    // Create + sequential plain writes both auto-stat base hash.
    let w1 = proxygit_client::mcp_write_file(&pool, project, "auto.txt", b"one").await;
    assert_eq!(w1["status"], "ok", "{w1}");
    let w2 = proxygit_client::mcp_write_file(&pool, project, "auto.txt", b"two").await;
    assert_eq!(w2["status"], "ok", "{w2}");

    // Explicit stale base still conflicts.
    let stale =
        proxygit_client::mcp_write_file_opts(&pool, project, "auto.txt", b"three", Some([0u8; 32]))
            .await;
    assert_eq!(
        stale.get("error").and_then(|e| e.as_str()),
        Some("conflict"),
        "{stale}"
    );

    // stat exposes tree_hash for agents.
    let st = proxygit_client::mcp_stat(&pool, project, "auto.txt").await;
    assert!(
        st.get("tree_hash").and_then(|h| h.as_str()).is_some(),
        "{st}"
    );

    handle.abort();
    std::env::remove_var("PROXYGIT_WRITE_CONFLICT");
    std::env::remove_var("PROXYGIT_WEBDAV_LISTEN");
    Ok(())
}

fn hex_to_32(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    anyhow::ensure!(s.len() == 64, "bad hex len {} ({s})", s.len());
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

fn curl_status(url: &str) -> Result<u16> {
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "3",
            url,
        ])
        .output()?;
    let code = String::from_utf8_lossy(&out.stdout).trim().parse()?;
    Ok(code)
}

fn curl_status_with_auth(url: &str, token: &str) -> Result<u16> {
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "3",
            "-H",
            &format!("Authorization: Bearer {token}"),
            url,
        ])
        .output()?;
    let code = String::from_utf8_lossy(&out.stdout).trim().parse()?;
    Ok(code)
}
