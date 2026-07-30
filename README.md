# ProxyGit

**Experimental agent-native network filesystem (MVP).**

ProxyGit lets multiple agents and developers share one remote project tree through
a small Rust server. Files are stored as content-addressed blocks; clients reach
them via QUIC, WebDAV, CLI, or MCP. Small edits reuse server blocks via sparse
writes (see limits).

```
  Agent / IDE / shell                 Your LAN or VPN
  ┌──────────────────┐               ┌─────────────────────────┐
  │ proxygit-client  │── QUIC :8080 ─▶│ proxygit-server         │
  │  • CLI verbs     │── WebDAV:3900─▶│  • SQLite project index │
  │  • MCP tools     │               │  • FastCDC block store  │
  │  • FUSE (opt.)   │               │  • optional embeddings  │
  └──────────────────┘               └─────────────────────────┘
```

## What problem this solves

| Pain | Without ProxyGit | With ProxyGit today |
|------|------------------|----------------------|
| Agents thrash the filesystem | Every tool call shells out to `cat`/`sed` on a full checkout | MCP tools (`read_file`, `write_file`, …) hit a structured API |
| Shared remote tree | Ad-hoc NFS/SMB/rsync copies | Self-hosted project store over QUIC + optional WebDAV mount |
| Concurrent agent edits | Uncoordinated shared folders | Client WAL → async flush; **server is source of truth** (last-writer-wins, no merge yet) |
| Mount friction | Kernel FUSE required everywhere | **WebDAV** (Finder / davfs) and **one-shot CLI** work with zero kexts |
| “Where is the tree?” | Recursive `find` / `grep` | `get_project_map` / index helpers in one call |

**In one sentence:** ProxyGit is a small self-hosted **project file proxy** aimed at agent tool use — not a git host, not JuiceFS/Mutagen, and not production multi-tenant storage.

### Honest limits (MVP)

- **Sparse writes are 64 KiB block-aligned.** CLI write + WAL flush send only changed fixed-size blocks (plus a `HAS_BLOCKS` handshake on the CLI path). Not byte-granular VCDIFF; a 1‑byte edit still ships up to one 64 KiB block.
- **`content_search` is lexical feature-hash (default), not a language model.** Token bag → 384-d vectors (`PROXYGIT_EMBEDDING=features`). Set `=hash` for the old BLAKE3 identity mock. Legacy MCP name `semantic_search` is an alias. ONNX/BGE remains optional later.
- **“Versioned” ≠ git history.** Content-addressed blocks + tarball backups, not branches/merges.
- **Auth is optional and layered.** Unset token + unset mTLS CA = open trusted-network mode. `PROXYGIT_TOKEN` → QUIC `MSG_AUTH` + WebDAV Bearer. `PROXYGIT_MTLS_CA` → require CA-signed client cert (`proxygit-client gen-mtls`).
- **Conflicts are opt-in.** Default last-writer-wins; `PROXYGIT_WRITE_CONFLICT=reject_stale` rejects stale `expected_tree_hash`. In reject mode the CLI/MCP auto-stats the current base when you omit the hash.

### Sparse write microbench (loopback, release)

`scripts/bench-edit.sh` — 1 MiB file, then a 1 KiB mid-file edit:

| Step | `WRITE_BLOCKS_SPARSE` payload | vs file |
|------|-------------------------------|---------|
| Initial write | 1 049 169 B | ~100% |
| 1 KiB edit rewrite | **66 129 B** | **~6.3%** (~15.9× smaller) |

Includes fixed 64 KiB changed block + per-block hash headers for the whole file.
Handshake (`HAS_BLOCKS`) is extra (~0.5–1 KiB) and not in the payload column.
The same script also prints rsync/cp (and scp when loopback SSH works) whole-file baselines.
Re-run the script after protocol changes before citing new numbers.

### What it is not

- Not a replacement for `git` (no commit/branch UI yet; git integration is roadmap)
- Not multi-tenant SaaS auth (trusted-network / lab deployment today — see [Security](#security))
- Not an object store API (S3/Garage backend is planned; MVP uses local disk)

## Status

| Surface | State |
|---------|--------|
| QUIC transport + SQLite index + local block store | ✅ |
| CLI: `ls` / `cat` / `stat` / `write` / `search` / `backup` | ✅ |
| MCP agent interface (stdio + TCP `:8082`) | ✅ |
| WebDAV native mount (`:3900`) | ✅ |
| FUSE mount | Optional (`--features fuse`, macOS/Linux) |
| Content-addressed block storage (FastCDC + BLAKE3) | ✅ |
| Sparse wire write (64 KiB block diff + `HAS_BLOCKS`) | ✅ |
| Content search (`content_search`) | ✅ feature-hash embeddings (default); `hash` mock optional |
| ONNX / BGE language-model embeddings | ❌ roadmap |
| Optimistic concurrency (`expected_tree_hash`) | ✅ optional `PROXYGIT_WRITE_CONFLICT=reject_stale` (+ auto base hash) |
| CRDT / automatic merge | ❌ |
| Client auth (optional bearer token) | ✅ off by default (`PROXYGIT_TOKEN` / `gen-token`) |
| Optional mTLS (client certs) | ✅ off by default (`PROXYGIT_MTLS_CA` / `gen-mtls`) |
| Garage S3 backend, A2A bus | Roadmap — see [`ARCHITECTURE-ROADMAP.md`](ARCHITECTURE-ROADMAP.md) |

### Platform matrix

| | macOS | Linux | Windows |
|--|-------|-------|---------|
| Server | ✅ | ✅ primary | ❌ |
| CLI + MCP client | ✅ | ✅ | 🟡 untested build |
| WebDAV client | ✅ Finder | ✅ davfs2 | ✅ Map Network Drive |
| FUSE | ✅ optional | ✅ optional | ❌ |

Network path is **yours**: localhost, Tailscale, WireGuard, LAN, or VPC. ProxyGit
only needs reachability to UDP 8080 (+ TCP 3900 for WebDAV). See the diagrams in
[`docs/site/`](docs/site/).

**Build gate (verified):** `cargo check --release`, unit + smoke tests, `cargo fmt --check`,
plus a local CLI + WebDAV + MCP smoke of the documented quickstart path.

## Quick start (local, no remote host)

### Prerequisites

- Rust 1.78+ (edition 2021 workspace)
- Optional: Docker (server container)
- Optional: macFUSE / FUSE3 only if you want the FUSE mount

### 1. Build

```bash
git clone <this-repo> proxygit
cd proxygit
cargo build --release
```

Binaries land in `target/release/proxygit-server` and `target/release/proxygit-client`.

### 2. Run the server (foreground)

```bash
export PROXYGIT_DATA_DIR=./data
export PROXYGIT_LISTEN=127.0.0.1:8080
export PROXYGIT_WEBDAV_LISTEN=127.0.0.1:3900
./target/release/proxygit-server
```

On first start the server writes a self-signed cert under `$PROXYGIT_DATA_DIR/server_cert.der`.
Pin it for the client (avoids stale `~/.config/proxygit/server_cert.der`):

```bash
export PROXYGIT_SERVER_CERT="$PWD/data/server_cert.der"
```

> **Bind defaults:** if you omit the env vars, both QUIC and WebDAV listen on
> `0.0.0.0`. That is fine on a trusted VPN/lab link; **do not expose to the public
> internet** (no application auth yet). Prefer `127.0.0.1` for laptop-only demos.

### 3. Or run via Docker

```bash
# all host interfaces (private network / lab)
docker compose -f docker/docker-compose.yml up -d --build

# loopback only (laptop)
docker compose -f docker/docker-compose.localhost.yml up -d --build

docker compose -f docker/docker-compose.localhost.yml logs -f
```

Maps host UDP `8080` (QUIC) and TCP `3900` (WebDAV). Persist data in `proxygit-data`.
Host publish is controlled by Compose `ports:`, not only `PROXYGIT_*_LISTEN`.

### 4. Talk to it

```bash
PROJECT=00000000-0000-0000-0000-000000000001
SERVER=127.0.0.1:8080
export PROXYGIT_SERVER_CERT="$PWD/data/server_cert.der"

./target/release/proxygit-client write "$SERVER" "$PROJECT" README.md "hello from proxygit"
./target/release/proxygit-client ls    "$SERVER" "$PROJECT"
./target/release/proxygit-client cat   "$SERVER" "$PROJECT" README.md
./target/release/proxygit-client stat  "$SERVER" "$PROJECT" README.md

# MCP (stdio)
./target/release/proxygit-client mcp "$SERVER" "$PROJECT"

# WebDAV (macOS)
mkdir -p /tmp/pg-mount
mount_webdav "http://127.0.0.1:3900/webdav/$PROJECT" /tmp/pg-mount
```

Full install / deploy variants: [`QUICKSTART.md`](QUICKSTART.md).

## Architecture (short)

| Piece | Role | Default port |
|-------|------|--------------|
| `proxygit-server` | QUIC listener, WebDAV, SQLite index, block store, backups | UDP 8080, TCP 3900 |
| `proxygit-client` | CLI, MCP, optional FUSE, WAL journal, stream pool | MCP TCP 8082 (when enabled) |
| `proxygit-common` | Frame protocol, FastCDC chunker, shared types | — |

Storage model:

1. Files are split with **FastCDC** into variable-size chunks.
2. Each chunk is addressed by **BLAKE3**.
3. Per-project **SQLite** indexes path → block list / tree hash.
4. Client writes can journal through a local **WAL**, then flush over QUIC.

Detailed component diagram and message types: [`SPEC.md`](SPEC.md).

## MCP tools

When the client runs in MCP mode, agents get:

| Tool | Purpose |
|------|---------|
| `read_file` | Read path relative to project root |
| `write_file` | Write UTF-8 (binary via base64 field where supported) |
| `list_directory` | Directory listing |
| `stat` | Size / mtime / hash metadata |
| `get_project_map` | Full tree in one round-trip |
| `content_search` | Feature-hash neighbors (default) or BLAKE3 mock (`PROXYGIT_EMBEDDING=hash`); alias: `semantic_search` |

A checked-out ProxyGit workspace may also contain a `.proxygit` TOML manifest so
agents can discover `server`, `uuid`, and MCP endpoint without hard-coding hosts.

## Configuration

| Variable | Default | Meaning |
|----------|---------|---------|
| `PROXYGIT_DATA_DIR` | `/tmp/proxygit-server/data` (binary) / `/data` (Docker) | Indexes, blocks, certs, backups |
| `PROXYGIT_LISTEN` | `0.0.0.0:8080` | QUIC bind address |
| `PROXYGIT_WEBDAV_LISTEN` | `0.0.0.0:3900` | WebDAV bind address |
| `PROXYGIT_TOKEN` / `_FILE` | unset | Optional bearer (server + client); off when unset |
| `PROXYGIT_MTLS_CA` | unset | Server: require client certs signed by this CA DER |
| `PROXYGIT_CLIENT_CERT` / `_KEY` | unset | Client: present mTLS leaf (use `gen-mtls`) |
| `PROXYGIT_WRITE_CONFLICT` | `last_writer_wins` | `reject_stale` → expected-hash checks (+ auto base) |
| `PROXYGIT_EXPECTED_TREE_HASH` | unset | CLI override for conditional write (64 hex) |
| `PROXYGIT_EMBEDDING` | `features` | `features` = token bag; `hash` = BLAKE3 mock |

Client defaults (overridable later via config file / flags): mount and cache under
`/tmp/proxygit/…`, server `127.0.0.1:8080`.

## Security
**Default posture: trusted network (lab / VPN / localhost).** Auth layers are optional and off until configured.

- **Bearer token** — set `PROXYGIT_TOKEN` (or `_FILE`) on server *and* client. QUIC first stream = `MSG_AUTH`; WebDAV needs `Authorization: Bearer`. Generate with `proxygit-client gen-token`.
- **mTLS** — set `PROXYGIT_MTLS_CA` on the server to a CA cert DER. Clients set `PROXYGIT_CLIENT_CERT` + `PROXYGIT_CLIENT_KEY`. Bundle helper: `proxygit-client gen-mtls ./mtls`.
- **WebDAV** remains plain HTTP (Bearer only when token mode is on) — still not public-internet safe without a private path.
- No multi-tenant RBAC or rate limits yet.

Recommended deployment:

1. Bind to `127.0.0.1` for single-machine use, **or**
2. Place the server on a private overlay (Tailscale, WireGuard, VPC) and do **not**
   publish `8080/udp` or `3900/tcp` to the public internet.
3. Prefer token and/or mTLS when more than one trusted host can reach the ports.
4. Treat project UUIDs as unguessable *capabilities*, not as real auth.

## Repo layout

```
proxygit/
├── crates/
│   ├── proxygit-common/     # protocol, CDC, types
│   ├── proxygit-server/     # QUIC + WebDAV + index + blocks
│   └── proxygit-client/     # CLI, MCP, WAL, optional FUSE
├── docker/
│   ├── docker-compose.yml
│   └── server.Dockerfile
├── scripts/                 # installers + local demo
├── docs/
│   ├── design/              # research & future design (not required to run)
│   └── reviews/             # historical review notes
├── SPEC.md                  # full system specification
├── ARCHITECTURE-ROADMAP.md  # priorities and known gaps
├── QUICKSTART.md            # deploy & mount guide
├── AGENTS.md                # conventions for coding agents in this repo
└── LICENSE                  # MIT
```

## Develop

```bash
# typecheck
cargo check --release -p proxygit-server -p proxygit-client

# tests (TMPDIR=/tmp avoids some macOS sandbox path issues)
TMPDIR=/tmp cargo test -p proxygit-server -p proxygit-client -p proxygit-common

# formatting
cargo fmt --check
```

Optional FUSE client build:

```bash
cargo build --release -p proxygit-client --features fuse
```

## License

MIT License — see [`LICENSE`](LICENSE).
