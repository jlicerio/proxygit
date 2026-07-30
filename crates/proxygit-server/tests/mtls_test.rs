//! Optional mTLS: server requires CA-signed client cert when PROXYGIT_MTLS_CA is set.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use uuid::Uuid;

fn free_port() -> u16 {
    let s = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

fn clear_auth_env() {
    std::env::remove_var("PROXYGIT_TOKEN");
    std::env::remove_var("PROXYGIT_TOKEN_FILE");
    std::env::remove_var("PROXYGIT_WRITE_CONFLICT");
    std::env::remove_var("PROXYGIT_CLIENT_CERT");
    std::env::remove_var("PROXYGIT_CLIENT_KEY");
    std::env::remove_var("PROXYGIT_MTLS_CA");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_rejects_missing_client_cert_and_accepts_valid() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().to_path_buf();
    let mtls_dir = temp.path().join("mtls");
    proxygit_common::mtls::write_mtls_bundle(&mtls_dir, "test-client")?;

    let quic_port = free_port();
    let dav_port = free_port();
    let listen: SocketAddr = format!("127.0.0.1:{quic_port}").parse()?;

    clear_auth_env();
    std::env::set_var("PROXYGIT_WEBDAV_LISTEN", format!("127.0.0.1:{dav_port}"));
    let ca_path = mtls_dir.join("ca_cert.der");
    assert!(ca_path.exists());
    std::env::set_var("PROXYGIT_MTLS_CA", ca_path.to_string_lossy().as_ref());

    // Prove make_server_config sees mTLS before boot.
    let _cfg = proxygit_server::make_server_config(&data_dir)?;

    let server_data = data_dir.clone();
    let handle =
        tokio::spawn(async move { proxygit_server::run_server(listen, server_data).await });

    let cert = data_dir.join("server_cert.der");
    for _ in 0..50 {
        if cert.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        cert.exists(),
        "missing server cert — server failed to start?"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    // No client cert → TLS handshake must fail.
    std::env::remove_var("PROXYGIT_CLIENT_CERT");
    std::env::remove_var("PROXYGIT_CLIENT_KEY");
    let no_cert = tokio::time::timeout(
        Duration::from_secs(5),
        proxygit_client::connect_to_server_with_cert(listen, Some(cert.clone())),
    )
    .await;
    match &no_cert {
        Ok(Err(e)) => {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("Certificate")
                    || msg.contains("certificate")
                    || msg.contains("TLS")
                    || msg.contains("tls")
                    || msg.contains("handshake")
                    || msg.contains("Connection")
                    || msg.contains("closed")
                    || msg.contains("reset")
                    || msg.contains("failed"),
                "unexpected error shape: {msg}"
            );
        }
        Ok(Ok(conn)) => {
            // Some stacks surface mTLS failure on first stream, not connect.
            let pool = proxygit_client::StreamPool::new(conn.clone(), 1);
            let project = Uuid::new_v4();
            let res = tokio::time::timeout(
                Duration::from_secs(3),
                proxygit_client::mcp_write_file(&pool, project, "should-fail.txt", b"x"),
            )
            .await;
            let failed = match &res {
                Err(_) => true,
                Ok(v) => v.get("status").and_then(|s| s.as_str()) != Some("ok"),
            };
            assert!(
                failed,
                "connect without client cert must not allow writes under mTLS; got {res:?}"
            );
        }
        Err(_) => panic!("connect without client cert timed out (expected quick TLS fail)"),
    }

    // Valid client cert → connect + write OK.
    std::env::set_var(
        "PROXYGIT_CLIENT_CERT",
        mtls_dir.join("client_cert.der").to_string_lossy().as_ref(),
    );
    std::env::set_var(
        "PROXYGIT_CLIENT_KEY",
        mtls_dir.join("client_key.der").to_string_lossy().as_ref(),
    );
    let conn = tokio::time::timeout(
        Duration::from_secs(5),
        proxygit_client::connect_to_server_with_cert(listen, Some(cert)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("mTLS connect timed out"))??;
    let pool = proxygit_client::StreamPool::new(conn, 4);
    let project = Uuid::new_v4();
    let res = proxygit_client::mcp_write_file(&pool, project, "mtls.txt", b"ok").await;
    assert_eq!(res["status"], "ok", "{res}");

    handle.abort();
    clear_auth_env();
    std::env::remove_var("PROXYGIT_WEBDAV_LISTEN");
    Ok(())
}
