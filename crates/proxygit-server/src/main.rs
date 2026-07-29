//! ProxyGit Server CLI Entry Point

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("proxygit_server=debug,info")
        .init();

    let data_dir = std::env::var("PROXYGIT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/proxygit-server/data"));

    let listen_addr: SocketAddr = std::env::var("PROXYGIT_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .context("Invalid PROXYGIT_LISTEN address")?;

    proxygit_server::run_server(listen_addr, data_dir).await
}
