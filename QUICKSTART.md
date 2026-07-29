# ProxyGit Quickstart

Agnostic setup — no assumed hostnames, VPN products, or personal paths.
Replace `$SERVER_HOST` with `127.0.0.1` (local) or your private-network hostname/IP.

## Prerequisites

- Rust 1.78+ (to build from source)
- Optional: Docker + Docker Compose (containerized server)
- Optional mounts:
  - macOS WebDAV: built-in `mount_webdav` / Finder → Connect to Server
  - Linux WebDAV: `davfs2`
  - FUSE (optional): macFUSE (`brew install --cask macfuse`) or Linux FUSE3

## 1. Build

```bash
cd proxygit
cargo build --release
```

Produces:

- `target/release/proxygit-server`
- `target/release/proxygit-client`

## 2. Run the server

### Option A — local binary (recommended first run)

```bash
mkdir -p ./data
export PROXYGIT_DATA_DIR="$PWD/data"
export PROXYGIT_LISTEN=127.0.0.1:8080
export PROXYGIT_WEBDAV_LISTEN=127.0.0.1:3900
./target/release/proxygit-server
```

Expected log lines:

```
ProxyGit Server
Index directory: .../data/indexes
Block store:     .../data/blocks
Listening on:    127.0.0.1:8080
QUIC endpoint ready on 127.0.0.1:8080
WebDAV HTTP server ready on http://127.0.0.1:3900
```

Self-signed TLS material for QUIC is created at `$PROXYGIT_DATA_DIR/server_cert.der` on first boot.

### Option B — Docker Compose

From the repo root:

```bash
docker compose -f docker/docker-compose.yml up -d --build
docker compose -f docker/docker-compose.yml logs -f proxygit-server
```

Default publish:

| Port | Proto | Service |
|------|-------|---------|
| 8080 | UDP | QUIC |
| 3900 | TCP | WebDAV |

Data persists in the Docker volume `proxygit-data`.

To point Compose at a specific host interface, edit `PROXYGIT_LISTEN` /
`PROXYGIT_WEBDAV_LISTEN` in `docker/docker-compose.yml` (defaults are `0.0.0.0`
inside the container — only safe on trusted networks; see [Security](README.md#security)).

### Option C — remote Linux host over SSH

Any SSH-reachable Linux box works. Example pattern:

```bash
rsync -av --exclude target/ --exclude .git/ ./ user@your-server:~/proxygit/
ssh user@your-server 'cd ~/proxygit && docker compose -f docker/docker-compose.yml up -d --build'
```

Copy the server cert for the QUIC client if you verify pins locally:

```bash
ssh user@your-server 'docker cp proxygit-server:/data/server_cert.der /tmp/server_cert.der'
mkdir -p ~/.config/proxygit
scp user@your-server:/tmp/server_cert.der ~/.config/proxygit/server_cert.der
```

## 3. Choose a project id

ProxyGit addresses projects by UUID. Generate one or use a fixed lab id:

```bash
# random
PROJECT=$(uuidgen | tr '[:upper:]' '[:lower:]')
# or fixed for demos
PROJECT=00000000-0000-0000-0000-000000000001

SERVER=127.0.0.1:8080   # or your-server:8080
```

The server creates index state for a project on first write.

## 4. Access the project (four ways)

### A. One-shot CLI (no mount, no FUSE)

```bash
proxygit-client write  "$SERVER" "$PROJECT" src/main.rs 'fn main() { println!("hi"); }'
proxygit-client ls     "$SERVER" "$PROJECT"
proxygit-client cat    "$SERVER" "$PROJECT" src/main.rs
proxygit-client stat   "$SERVER" "$PROJECT" src/main.rs
proxygit-client search "$SERVER" "$PROJECT" "main entrypoint" 5

# backups via WebDAV helper routes (server host, not :8080)
proxygit-client backup create  "$SERVER_HOST" "$PROJECT"
proxygit-client backup list    "$SERVER_HOST" "$PROJECT"
```

`write` with no text argument reads stdin:

```bash
proxygit-client write "$SERVER" "$PROJECT" notes.md < ./local-notes.md
```

### B. WebDAV mount (no kernel extension)

```bash
# macOS
mkdir -p /tmp/my-project
mount_webdav "http://127.0.0.1:3900/webdav/$PROJECT" /tmp/my-project

# Linux (davfs2)
sudo mount -t davfs "http://127.0.0.1:3900/webdav/$PROJECT" /mnt/my-project
```

Finder: **Go → Connect to Server** → `http://127.0.0.1:3900/webdav/<project-uuid>`.

### C. MCP for AI agents

Stdio (typical for Claude / Cursor / custom runners):

```bash
proxygit-client mcp "$SERVER" "$PROJECT"
```

The process speaks JSON-RPC 2.0 MCP on stdin/stdout. Tools include
`read_file`, `write_file`, `list_directory`, `stat`, `get_project_map`,
and `semantic_search`.

When the client also brings up its TCP MCP listener, it binds `localhost:8082`
by default (loopback only).

### D. FUSE mount (optional)

Requires a FUSE-enabled build and platform support:

```bash
cargo build --release -p proxygit-client --features fuse
proxygit-client mount "$SERVER" "$PROJECT"
# default mount point: /tmp/proxygit/mount
proxygit-client status
proxygit-client unmount
```

## 5. `.proxygit` manifest (optional, for agents)

A workspace root may include a TOML manifest so agents auto-discover connection
info. **Use placeholders — never commit personal tailnet IPs.**

```toml
[project]
name = "example"
uuid = "00000000-0000-0000-0000-000000000001"

[remote]
address = "127.0.0.1:8080"

[mcp]
address = "localhost:8082"
tools = ["read_file", "write_file", "list_directory", "stat", "get_project_map", "semantic_search"]

[webdav]
url = "http://127.0.0.1:3900/webdav/00000000-0000-0000-0000-000000000001/"
```

Agent flow:

1. Read `.proxygit` → server, uuid, MCP endpoint  
2. `tools/list` on MCP  
3. Use MCP file tools (prefer over shelling out to the mount)

## 6. Environment reference

| Variable | Default | Notes |
|----------|---------|--------|
| `PROXYGIT_DATA_DIR` | binary: `/tmp/proxygit-server/data`; Docker: `/data` | indexes, blocks, certs, backups |
| `PROXYGIT_LISTEN` | `0.0.0.0:8080` | QUIC; prefer `127.0.0.1` locally |
| `PROXYGIT_WEBDAV_LISTEN` | `0.0.0.0:3900` | WebDAV HTTP; **no auth** |

## 7. Feature checklist

| Feature | Status |
|---------|--------|
| QUIC transport + SQLite index + block store | ✅ |
| CLI verbs (`ls` / `cat` / `stat` / `write` / `search`) | ✅ |
| MCP (stdio + optional TCP) | ✅ |
| WebDAV native mount | ✅ |
| FUSE mount | Optional feature |
| Server-side backup create/list/restore | ✅ |
| Object-store backend (Garage/S3) | Future |
| Application-level auth | Future — trusted network only today |

## 8. Troubleshooting

| Symptom | Check |
|---------|--------|
| Client can't connect | UDP 8080 open? Correct `host:port`? Firewall allowing QUIC/UDP? |
| WebDAV mount fails | TCP 3900 reachable? URL includes `/webdav/<uuid>`? |
| Empty `ls` | No files written yet for that UUID — `write` first |
| Cert errors | Copy `server_cert.der` from the server data dir, or use a client path that trusts the generated cert |
| Tests fail on macOS paths | Run with `TMPDIR=/tmp` |

## 9. Next reading

- [`README.md`](README.md) — problem statement & security posture  
- [`SPEC.md`](SPEC.md) — protocols, MCP schema, milestones  
- [`ARCHITECTURE-ROADMAP.md`](ARCHITECTURE-ROADMAP.md) — known gaps & priorities  
