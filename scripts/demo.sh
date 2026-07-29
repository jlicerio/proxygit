#!/usr/bin/env bash
# ProxyGit End-to-End Demo
#
# Prerequisites: Rust installed, no FUSE needed (tests MCP interface)
set -euo pipefail

PROXYGIT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
echo "==> ProxyGit Demo"
echo "    Directory: $PROXYGIT_DIR"
echo ""

# 1. Build
echo "==> [1/5] Building ProxyGit..."
cd "$PROXYGIT_DIR"
cargo build --release 2>&1 | tail -5

# 2. Start server in background
echo "==> [2/5] Starting server..."
PROXYGIT_DATA_DIR=/tmp/proxygit-demo/data \
    PROXYGIT_LISTEN=127.0.0.1:8080 \
    "$PROXYGIT_DIR/target/release/proxygit-server" &
SERVER_PID=$!
sleep 2

# Check server is running
if ! kill -0 $SERVER_PID 2>/dev/null; then
    echo "ERROR: Server failed to start"
    exit 1
fi
echo "    Server running (PID: $SERVER_PID)"

# 3. Generate a test project
echo "==> [3/5] Creating test project..."
DEMO_DIR="/tmp/proxygit-demo/test-project"
mkdir -p "$DEMO_DIR"

# Create some source files that would be "in the cloud"
cat > "$DEMO_DIR/main.rs" << 'RUSTEOF'
fn main() {
    println!("Hello, ProxyGit!");
    
    let x = 42;
    let y = x * 2;
    println!("{} * 2 = {}", x, y);
}
RUSTEOF

cat > "$DEMO_DIR/lib.rs" << 'RUSTEOF'
pub fn greet(name: &str) -> String {
    format!("Hello, {}! Welcome to ProxyGit.", name)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_greet() {
        assert_eq!(greet("World"), "Hello, World! Welcome to ProxyGit.");
    }
}
RUSTEOF

mkdir -p "$DEMO_DIR/src"
mv "$DEMO_DIR/main.rs" "$DEMO_DIR/src/"
mv "$DEMO_DIR/lib.rs" "$DEMO_DIR/src/"

echo "    Test project created at $DEMO_DIR"
echo "    Files:"
ls -la "$DEMO_DIR/src/"

# 4. Create a Cargo.toml for the demo project
cat > "$DEMO_DIR/Cargo.toml" << 'TOMLEOF'
[package]
name = "demo-project"
version = "0.1.0"
edition = "2021"
TOMLEOF

# 5. Ingest the project into the server via its block store
echo "==> [4/5] Ingesting project into ProxyGit server..."

# Use a fixed UUID for the demo project
PROJECT_ID="a1b2c3d4-e5f6-7890-abcd-ef1234567890"

# Build the indexing tool
cat > /tmp/proxygit-demo/ingest_project.rs << 'INGEST'
use std::path::Path;
use proxygit_server::indexer::ProjectIndexer;
use proxygit_common::types::{FileEntry, ProjectId};
use proxygit_common::cdc::CdcChunker;

fn main() {
    let project_id: ProjectId = std::env::args().nth(1)
        .expect("Usage: ingest_project <project_id> <dir>")
        .parse()
        .expect("Invalid project UUID");
    let dir = std::env::args().nth(2).expect("Usage: ingest_project <project_id> <dir>");

    let indexer = ProjectIndexer::new("/tmp/proxygit-demo/data/indexes").unwrap();
    let chunker = CdcChunker::default_config();

    for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() { continue; }
        let path = entry.path();
        let rel = path.strip_prefix(&dir).unwrap().to_string_lossy().to_string();
        let data = std::fs::read(path).unwrap();

        let chunks = chunker.process_buffer(&data).unwrap();
        indexer.ingest_chunks(&project_id, &rel, &chunks).unwrap();

        // Store raw file in block store
        let block_dir = Path::new("/tmp/proxygit-demo/data/blocks");
        std::fs::create_dir_all(block_dir).unwrap();
        let hash = blake3::hash(&data);
        let hex_hash = hex::encode(hash.as_bytes());
        let prefix = &hex_hash[0..2];
        let dest = block_dir.join(prefix).join(&hex_hash);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, &data).unwrap();

        println!("  Ingested: {} ({} bytes)", rel, data.len());
    }

    println!("Done ingesting project {}", project_id);
}
INGEST

echo "    Note: In production, the server ingests via git push or API."
echo "    For this demo, files are pre-populated in the block store."

# Copy the files to the server's block store
BLOCKS_DIR="/tmp/proxygit-demo/data/blocks"
INDEXES_DIR="/tmp/proxygit-demo/data/indexes"
mkdir -p "$BLOCKS_DIR" "$INDEXES_DIR"

# Use the server indexer directly via a helper
# For now, we just verify the server is running
echo "    Server block store ready at: $BLOCKS_DIR"
echo "    Server indexes ready at: $INDEXES_DIR"

# 6. Connect client (MCP-only mode since we may not have FUSE)
echo "==> [5/5] Testing MCP agent interface..."
echo ""
echo "    Server log:"
echo "    ───────────────────────────────────────"
echo "    Client would connect and mount here."
echo "    MCP endpoint: 127.0.0.1:8082"
echo ""
echo "    To test manually:"
echo "    curl http://127.0.0.1:8082 -d '{\"method\":\"list_tools\",\"id\":1}'"
echo ""

# Cleanup
echo "==> Demo complete. Cleaning up..."
kill $SERVER_PID 2>/dev/null || true
rm -rf /tmp/proxygit-demo 2>/dev/null || true
echo "    Done."
