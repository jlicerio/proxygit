//! ProxyGit Client Daemon CLI Entry Point

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{info, warn};

use proxygit_client::*;
use proxygit_common::types::{ClientConfig, ProjectConfig};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("proxygit_client=debug,info")
        .init();

    let config = ClientConfig::default();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: proxygit-client <command> [args]");
        eprintln!("  version                           — Print version and exit");
        eprintln!("  mount <server_addr> <project_id>  — Mount a project (FUSE / local)");
        eprintln!(
            "  mcp <server_addr> <project_id>    — Run MCP JSON-RPC 2.0 stdio server for AI agents"
        );
        eprintln!("  ls <server_addr> <project_id> [prefix] — List directory files");
        eprintln!("  cat <server_addr> <project_id> <path>    — Print file contents");
        eprintln!("  stat <server_addr> <project_id> <path>   — Show file metadata");
        eprintln!(
            "  write <server_addr> <project_id> <path> [text] — Write file (or read from stdin)"
        );
        eprintln!(
            "  search <server_addr> <project_id> <query> [limit] — Semantic search for files"
        );
        eprintln!("  backup create <server_addr> <uuid>    — Trigger server backup via WebDAV");
        eprintln!("  backup list <server_addr> <uuid>      — List backups via WebDAV");
        eprintln!("  backup restore <server_addr> <uuid> <name> — Restore a backup via WebDAV");
        eprintln!("  unmount                            — Unmount all");
        eprintln!("  status                            — Show mount status");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "version" => cmd_version().await?,
        "mount" => cmd_mount(&config, &args).await?,
        "mcp" => cmd_mcp(&config, &args).await?,
        "ls" => cmd_ls(&args).await?,
        "cat" => cmd_cat(&args).await?,
        "stat" => cmd_stat(&args).await?,
        "write" => cmd_write(&args).await?,
        "search" => cmd_search(&args).await?,
        "backup" => cmd_backup(&args).await?,
        "unmount" => cmd_unmount(&config).await?,
        "status" => cmd_status().await?,
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn cmd_mount(config: &ClientConfig, args: &[String]) -> Result<()> {
    if args.len() < 4 {
        anyhow::bail!("Usage: proxygit-client mount <server_addr> <project_id>");
    }

    let server_addr: SocketAddr = args[2]
        .parse()
        .context("Invalid server address (use ip:port e.g. 127.0.0.1:8080)")?;
    let project_id: uuid::Uuid = args[3]
        .parse()
        .context("Invalid project ID (must be a UUID)")?;

    let _project_config = ProjectConfig {
        id: project_id,
        name: args.get(4).cloned().unwrap_or_else(|| "default".into()),
        mount_point: config.mount_point.clone(),
        server_addr: args[2].clone(),
    };

    info!("Connecting to server at {}", server_addr);
    info!("Mount point: {}", config.mount_point.display());
    info!("Cache dir:   {}", config.cache_dir.display());
    info!("WAL dir:     {}", config.wal_dir.display());
    info!("Build cache: {}", config.build_cache_dir.display());

    std::fs::create_dir_all(&config.mount_point)?;
    std::fs::create_dir_all(&config.cache_dir)?;
    std::fs::create_dir_all(&config.wal_dir)?;
    std::fs::create_dir_all(&config.build_cache_dir)?;

    let conn = connect_to_server(server_addr).await?;
    info!("QUIC connection established");

    let wal = wal::LocalWal::new(&config.wal_dir)?;
    let wal = Arc::new(wal);
    let _flush_handle = wal::start_wal_flush_worker(
        wal.clone(),
        conn.clone(),
        project_id,
        config.cache_dir.clone(),
    );

    let list = list_project(&conn, project_id).await?;
    info!("Project has {} files", list.len());

    #[cfg(feature = "fuse")]
    {
        let fs =
            fuse_mount::FuseFilesystem::new(conn.clone(), project_id, config.clone(), wal.clone());
        let mount_point = config.mount_point.clone();

        info!("Mounting FUSE filesystem at {}", mount_point.display());

        tokio::task::spawn_blocking(move || {
            let mut session = fuser::Session::new(
                fs,
                &mount_point,
                &[
                    fuser::MountOption::AutoUnmount,
                    fuser::MountOption::AllowOther,
                ],
            )
            .expect("Failed to create FUSE session");

            session.run().expect("FUSE session ended with error");
        });

        info!("ProxyGit mounted at {}", config.mount_point.display());
        info!("Connect agents via MCP at localhost:8082");

        start_mcp_server(conn.clone(), project_id, config.clone()).await?;
        tokio::signal::ctrl_c().await?;
        info!("Shutting down...");
    }

    #[cfg(not(feature = "fuse"))]
    {
        warn!("FUSE not enabled (compile with --features fuse for FUSE mount)");
        warn!("Running in MCP-only mode — agents can connect via localhost:8082");
        start_mcp_server(conn.clone(), project_id, config.clone()).await?;
        tokio::signal::ctrl_c().await?;
    }

    Ok(())
}

async fn cmd_unmount(config: &ClientConfig) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mount_point = &config.mount_point;
        info!("Unmounting {}", mount_point.display());
        std::process::Command::new("umount")
            .arg(mount_point)
            .output()
            .context("Failed to unmount")?;
        info!("Unmounted successfully");
    }
    Ok(())
}

/// `mcp <server_addr> <project_id>` — Run stdio MCP JSON-RPC 2.0 server for AI agents
async fn cmd_mcp(config: &ClientConfig, args: &[String]) -> Result<()> {
    if args.len() < 4 {
        anyhow::bail!("Usage: proxygit-client mcp <server_addr> <project_id>");
    }

    let server_addr: SocketAddr = args[2]
        .parse()
        .context("Invalid server address (use ip:port e.g. 127.0.0.1:8080)")?;
    let project_id: uuid::Uuid = args[3]
        .parse()
        .context("Invalid project ID (must be a UUID)")?;

    let conn = connect_to_server(server_addr).await?;
    info!("QUIC connection established for MCP stdio transport");

    let wal = wal::LocalWal::new(&config.wal_dir)?;
    let wal = Arc::new(wal);
    let _flush_handle = wal::start_wal_flush_worker(
        wal.clone(),
        conn.clone(),
        project_id,
        config.cache_dir.clone(),
    );

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    info!("ProxyGit stdio MCP server ready for JSON-RPC 2.0 requests");
    run_mcp_stdio_stream(stdin, stdout, conn, project_id).await?;
    Ok(())
}

/// `version` — Print version and exit
async fn cmd_version() -> Result<()> {
    println!("proxygit-client v{}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

async fn cmd_status() -> Result<()> {
    println!("ProxyGit Client v{}", env!("CARGO_PKG_VERSION"));
    println!(
        "Status: {}",
        if PathBuf::from("/tmp/proxygit/mount").exists() {
            "Mounted"
        } else {
            "Not mounted"
        }
    );
    Ok(())
}

async fn start_mcp_server(
    conn: quinn::Connection,
    project_id: uuid::Uuid,
    _config: ClientConfig,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:8082").await?;
    info!("MCP server listening on 127.0.0.1:8082");

    loop {
        let (mut socket, addr) = listener.accept().await?;
        info!("MCP connection from {addr}");

        let conn = conn.clone();
        let project_id = project_id;

        tokio::spawn(async move {
            let pool = Arc::new(StreamPool::new(conn, 8));
            pool.prewarm().await;

            let (reader, mut writer) = socket.split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let resp = handle_mcp_jsonrpc_request(&line, &pool, project_id).await;
                        if !resp.is_null() {
                            if let Ok(json) = serde_json::to_string(&resp) {
                                let _ = writer.write_all(json.as_bytes()).await;
                                let _ = writer.write_all(b"\n").await;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
}
async fn cmd_ls(args: &[String]) -> Result<()> {
    if args.len() < 4 {
        anyhow::bail!("Usage: proxygit-client ls <server_addr> <project_id> [prefix]");
    }
    let server_addr: SocketAddr = args[2].parse().context("Invalid server address")?;
    let project_id: uuid::Uuid = args[3].parse().context("Invalid project ID")?;
    let prefix = args.get(4).map(|s| s.as_str()).unwrap_or("");

    let conn = connect_to_server(server_addr).await?;
    let pool = StreamPool::new(conn, 4);
    let res = mcp_list_directory(&pool, project_id, prefix).await;
    if let Some(err) = res.get("error") {
        anyhow::bail!("Error: {}", err.as_str().unwrap_or("unknown"));
    }
    if let Some(files) = res["files"].as_array() {
        for f in files {
            if let Some(p) = f.as_str() {
                if prefix.is_empty() || p.starts_with(prefix) {
                    println!("{p}");
                }
            }
        }
    }
    Ok(())
}

async fn cmd_cat(args: &[String]) -> Result<()> {
    if args.len() < 5 {
        anyhow::bail!("Usage: proxygit-client cat <server_addr> <project_id> <path>");
    }
    let server_addr: SocketAddr = args[2].parse().context("Invalid server address")?;
    let project_id: uuid::Uuid = args[3].parse().context("Invalid project ID")?;
    let path = &args[4];

    let conn = connect_to_server(server_addr).await?;
    let pool = StreamPool::new(conn, 4);
    let bytes = mcp_read_raw_bytes(&pool, project_id, path).await?;
    use std::io::Write;
    std::io::stdout().write_all(&bytes)?;
    Ok(())
}

async fn cmd_stat(args: &[String]) -> Result<()> {
    if args.len() < 5 {
        anyhow::bail!("Usage: proxygit-client stat <server_addr> <project_id> <path>");
    }
    let server_addr: SocketAddr = args[2].parse().context("Invalid server address")?;
    let project_id: uuid::Uuid = args[3].parse().context("Invalid project ID")?;
    let path = &args[4];

    let conn = connect_to_server(server_addr).await?;
    let pool = StreamPool::new(conn, 4);
    let res = mcp_stat(&pool, project_id, path).await;
    if let Some(err) = res.get("error") {
        anyhow::bail!("Error: {}", err.as_str().unwrap_or("unknown"));
    }
    println!("{}", serde_json::to_string_pretty(&res)?);
    Ok(())
}

async fn cmd_write(args: &[String]) -> Result<()> {
    if args.len() < 5 {
        anyhow::bail!("Usage: proxygit-client write <server_addr> <project_id> <path> [text]");
    }
    let server_addr: SocketAddr = args[2].parse().context("Invalid server address")?;
    let project_id: uuid::Uuid = args[3].parse().context("Invalid project ID")?;
    let path = &args[4];

    let content_bytes = if args.len() >= 6 {
        args[5].as_bytes().to_vec()
    } else {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    };

    let conn = connect_to_server(server_addr).await?;
    let pool = StreamPool::new(conn, 4);
    let res = mcp_write_file(&pool, project_id, path, &content_bytes).await;
    if let Some(err) = res.get("error") {
        anyhow::bail!("Error: {}", err.as_str().unwrap_or("unknown"));
    }
    println!("OK tree_hash: {}", res["tree_hash"].as_str().unwrap_or(""));
    Ok(())
}

/// `search <server_addr> <project_id> <query> [limit]` — Semantic file search
async fn cmd_search(args: &[String]) -> Result<()> {
    if args.len() < 5 {
        anyhow::bail!("Usage: proxygit-client search <server_addr> <project_id> <query> [limit]");
    }
    let server_addr: SocketAddr = args[2].parse().context("Invalid server address")?;
    let project_id: uuid::Uuid = args[3].parse().context("Invalid project ID")?;
    let query = &args[4];
    let limit: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(10);

    let conn = connect_to_server(server_addr).await?;
    let pool = StreamPool::new(conn, 4);
    let paths = mcp_semantic_search(&pool, project_id, query, limit).await?;
    for path in &paths {
        println!("{path}");
    }
    if paths.is_empty() {
        println!("(no results)");
    }
    Ok(())
}

/// `backup <create|list|restore> <server_addr> <uuid> [name]` — Backup management via WebDAV
async fn cmd_backup(args: &[String]) -> Result<()> {
    if args.len() < 5 {
        eprintln!("Usage:");
        eprintln!("  proxygit-client backup create <server_addr> <uuid>   — Trigger server backup");
        eprintln!("  proxygit-client backup list <server_addr> <uuid>    — List backups");
        eprintln!(
            "  proxygit-client backup restore <server_addr> <uuid> <name> — Restore a backup"
        );
        std::process::exit(1);
    }

    let subcmd = &args[2];
    let server_addr = &args[3];
    let uuid = &args[4];

    // Extract host from server address (strip port if present)
    let host = server_addr.split(':').next().unwrap_or(server_addr);

    match subcmd.as_str() {
        "create" => cmd_backup_create(host, uuid).await,
        "list" => cmd_backup_list(host, uuid).await,
        "restore" => {
            if args.len() < 6 {
                eprintln!("Usage: proxygit-client backup restore <server_addr> <uuid> <name>");
                std::process::exit(1);
            }
            let name = &args[5];
            cmd_backup_restore(host, uuid, name).await
        }
        _ => {
            eprintln!("Unknown backup subcommand: {subcmd}");
            eprintln!(
                "Usage: proxygit-client backup <create|list|restore> <server_addr> <uuid> [name]"
            );
            std::process::exit(1);
        }
    }
}

/// Trigger a server-side backup via WebDAV: GET /webdav/<uuid>/__backup/create
async fn cmd_backup_create(host: &str, uuid: &str) -> Result<()> {
    let url = format!("http://{host}:3900/webdav/{uuid}/__backup/create");
    let output = std::process::Command::new("curl")
        .arg("-s")
        .arg("-X")
        .arg("GET")
        .arg(&url)
        .output()
        .context("Failed to execute curl for backup create")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("Backup create failed: {stderr}{stdout}");
    }

    if !stdout.trim().is_empty() {
        println!("{}", stdout.trim());
    }
    println!("Backup created successfully");
    Ok(())
}

/// List available backups via WebDAV: GET /webdav/<uuid>/__backup/list
async fn cmd_backup_list(host: &str, uuid: &str) -> Result<()> {
    let url = format!("http://{host}:3900/webdav/{uuid}/__backup/list");
    let output = std::process::Command::new("curl")
        .arg("-s")
        .arg(&url)
        .output()
        .context("Failed to execute curl for backup list")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("Backup list failed: {stderr}{stdout}");
    }

    println!("{}", stdout.trim());
    Ok(())
}

/// Restore a backup via WebDAV: POST /webdav/<uuid>/__backup/restore with name=<name>
async fn cmd_backup_restore(host: &str, uuid: &str, name: &str) -> Result<()> {
    let url = format!("http://{host}:3900/webdav/{uuid}/__backup/restore");
    let output = std::process::Command::new("curl")
        .arg("-s")
        .arg("-X")
        .arg("POST")
        .arg("--data")
        .arg(format!("name={name}"))
        .arg(&url)
        .output()
        .context("Failed to execute curl for backup restore")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("Backup restore failed: {stderr}{stdout}");
    }

    if !stdout.trim().is_empty() {
        println!("{}", stdout.trim());
    }
    println!("Backup restore initiated successfully");
    Ok(())
}
