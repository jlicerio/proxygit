# ProxyGit — Full Specification

> **Living spec + historical gap analysis.** Sections 2–4 describe the target
> architecture and MVP bar. Section 1 captured the pre-implementation gap list;
> many MVP-blocking rows there are **done** in tree (QUIC, server/client mains,
> CLI, MCP, WebDAV, SQLite index, local block store). For *current* ship state
> and security posture, start with [`README.md`](README.md). Auth, Garage/S3,
> A2A, and full git UX remain roadmap items ([`ARCHITECTURE-ROADMAP.md`](ARCHITECTURE-ROADMAP.md)).

## 1. Gap Analysis (historical — pre-MVP snapshot)

### What the original design already had:
| Component | Status | File |
|---|---|---|
| PRD & System Overview | Complete (spec) | Doc 01 |
| VFS Architecture | High-level (spec only, no code) | Doc 02 |
| Protocol & DB Schema | Complete (spec + SQL) | Doc 03 |
| FastCDC Chunker | Implemented + test | `crates/proxygit-common/src/cdc.rs` |
| NVMe WAL Engine | Implemented | `crates/proxygit-client/src/wal/mod.rs` |
| SQLite Project Indexer | Implemented | `crates/proxygit-server/src/indexer/mod.rs` |
| Client Installers (shell, ps1) | Implemented | `scripts/install-client.sh`, `.ps1` |
| Docker Compose (Garage + server) | Implemented | `docker/docker-compose.yml` |

### What's MISSING (gaps):

| # | Gap | Impact | Priority for MVP |
|---|---|---|---|
| 1 | **No FUSE VFS mount code** — no `open`, `read`, `write`, `readdir`, `stat` handlers that delegate to the daemon | Core feature: without FUSE there's no virtual drive | **MVP-blocking** |
| 2 | **No QUIC transport implementation** — frame format defined, no send/recv code on either side | No network communication at all | **MVP-blocking** |
| 3 | **No proxygit-server main.rs** — no daemon lifecycle, no listener, no handler wiring | Server can't start | **MVP-blocking** |
| 4 | **No proxygit-client main.rs** — no daemon lifecycle, no CLI, no mount orchestration | Client can't start | **MVP-blocking** |
| 5 | **No block hydration + LRU cache** — blocks are fetched but never cached or evicted | Every read hits network; no offline mode | **MVP-blocking** |
| 6 | **No build directory intercept** — table exists in spec, no code maps patterns | `target/` and `node_modules/` would flow through VFS → slow | **MVP-blocking** |
| 7 | **No MCP server** — spec says agents connect via MCP, no tool implementations | No agent interface | **MVP-blocking** |
| 8 | **No A2A event bus** — spec says cross-agent visibility, no event types or subscriptions | No live cross-agent sync | Phase 2 |
| 9 | **No git integration** — partial clone, `git status`, server-side commit | Users still need git; VFS must be git-aware | Phase 2 |
| 10 | **No conflict resolution** — concurrent edits from two agents on same file | Data loss risk | Phase 2 |
| 11 | **No auth/security** — no TLS, no client identity | Anyone on LAN can read/write | Phase 2 |
| 12 | **No observability** — no metrics, tracing, health endpoint | Can't debug or monitor | Phase 2 |
| 13 | **No Windows WinFSP implementation** — install script exists, no Rust code | Windows not supported in MVP | Phase 3 |
| 14 | **No proxygit-common Cargo.toml or lib.rs** — types not exported | Nothing compiles | **MVP-blocking** |
| 15 | **No proxygit-client Cargo.toml** — missing deps (fuser, quinn) | Client doesn't compile | **MVP-blocking** |
| 16 | **No proxygit-server Cargo.toml** — missing deps (quinn, aws-sdk-s3 or s3s) | Server doesn't compile | **MVP-blocking** |
| 17 | **No SLM sidecar** — "Predictive Engine" mentioned in diagram, no spec | Advanced feature, skip MVP | Phase 3 |

### Architectural gaps worth noting:

**A. WAL → CDC → QUIC pipeline isn't wired.** The WAL appends raw bytes. The CDC chunks them. The QUIC sends them. But there's no connector code that chains these three stages. Need a `FlushPipeline` struct.

**B. File write path is incomplete.** `write()` → WAL is implemented. But `read()` → hydration, `open()` → directory resolution, and `release()` → close trigger are missing.

**C. Server handler dispatch not specified.** The QUIC frame format has `Msg Type` (1 byte) and `Project ID` (4 bytes). No message type enum is defined.

**D. Block store abstraction missing.** Garage S3 is specified but there's no trait/interface. Need an abstraction so MVP can use local filesystem instead of Garage.

---

## 2. Architecture — Detailed Component Diagram

```
┌─────────────────────────────────────────────────────────────────────┐
│ proxygit-client (Rust daemon, runs on workstation)                   │
│                                                                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────────────────┐ │
│  │ FUSE     │  │ Build    │  │ Local    │  │ Agent Interface     │ │
│  │ Mount    │─▶│ Intercept│  │ Block    │  │ ┌────┐ ┌────────┐  │ │
│  │ (fuser)  │  │ Table    │  │ Cache    │  │ │MCP │ │ A2A    │  │ │
│  │ open/    │  │ pattern  │  │ (LRU     │  │ │Srv │ │ Client │  │ │
│  │ read/    │  │ match →  │  │ eviction)│  │ └────┘ └────────┘  │ │
│  │ write/   │  │ redirect │  └────┬─────┘  └────────────────────┘ │
│  │ stat/    │  └──────────┘       │                               │
│  │ readdir  │                      │ block fetch/store             │
│  └────┬─────┘                      ▼                               │
│       │ file ops           ┌──────────────┐                        │
│       ▼                    │ FlushPipeline│                        │
│  ┌──────────┐              │ ┌──┐ ┌─────┐  │                        │
│  │ NVMe WAL │─────────────▶│ │WAL│▶│CDC  │──▶──┐                   │
│  │ (journal)│  async       │ │   │ │+hash│  │  │                   │
│  └──────────┘              │ └──┘ └─────┘  │  │                   │
│                            └───────────────┘  │                   │
│                                               ▼                   │
│                                      ┌──────────────┐              │
│                                      │ QUIC Transport│              │
│                                      │ (quinn)      │              │
│                                      └──────┬───────┘              │
└─────────────────────────────────────────────┼──────────────────────┘
                                              │ UDP port 8080
                                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│ proxygit-server (Rust, runs on Linux box / NAS)                      │
│                                                                     │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
│  │ QUIC Listener│─▶│ Request      │  │ Agent Interface          │  │
│  │ (quinn)      │  │ Dispatcher   │  │ ┌────┐ ┌────────┐      │  │
│  └──────────────┘  │ route by     │  │ │MCP │ │ A2A    │      │  │
│                    │ MsgType      │  │ │Srv │ │ Bus    │      │  │
│                    └──────┬───────┘  │ └────┘ └────────┘      │  │
│                           │          └──────────────────────────┘  │
│          ┌────────────────┼──────────────┐                         │
│          ▼                ▼              ▼                         │
│  ┌────────────┐  ┌──────────────┐  ┌──────────┐                    │
│  │ Block      │  │ SQLite       │  │ Git      │                    │
│  │ Store      │  │ Project      │  │ Interface│                    │
│  │ (S3 via    │  │ Index        │  │ (partial │                    │
│  │  Garage)   │  │ per-project  │  │  clone,  │                    │
│  └────────────┘  │ .sqlite       │  │  commit) │                    │
│                  └──────────────┘  └──────────┘                    │
└─────────────────────────────────────────────────────────────────────┘
```

---

## 3. Protocols

### 3.1 QUIC Message Types

All client↔server communication uses QUIC (quinn crate) over UDP port 8080.
Each message is a binary frame (defined in Doc 03). Message types:

| Type Byte | Name | Direction | Payload |
|---|---|---|---|
| `0x01` | `LIST_PROJECT` | C→S | Project ID (4 bytes) |
| `0x02` | `LIST_PROJECT_RESP` | S→C | JSON: `{files: [{path, size, mode, mtime}]}` |
| `0x03` | `READ_FILE` | C→S | Project ID + path string |
| `0x04` | `READ_FILE_RESP` | S→C | File data as blocks (CDC chunked) |
| `0x05` | `WRITE_BLOCKS` | C→S | Project ID + path + [block_hash, offset, data][] |
| `0x06` | `WRITE_ACK` | S→C | Path + new tree_hash |
| `0x07` | `STAT_FILE` | C→S | Project ID + path |
| `0x08` | `STAT_FILE_RESP` | S→C | JSON: `{size, mode, mtime, tree_hash}` |
| `0x09` | `EVENT_SUBSCRIBE` | C→S | Project ID + [path_patterns] |
| `0x0A` | `FILE_CHANGED` | S→C | Path + new tree_hash (push event) |
| `0x0B` | `BLOCK_REQUEST` | C→S | Block hash (32 bytes) |
| `0x0C` | `BLOCK_RESP` | S→C | Block hash + compressed block data |
| `0x0D` | `ERROR` | S→C | Error code (1 byte) + message string |

### 3.2 MCP Tool Schema

The client daemon exposes an MCP server on localhost port 8082.
Agents connect and call tools:

```json
{
  "tools": [
    {
      "name": "read_file",
      "description": "Read a file from the mounted ProxyGit workspace",
      "inputSchema": {
        "type": "object",
        "properties": {
          "path": {"type": "string", "description": "path relative to project root"},
          "project": {"type": "string", "description": "project ID"}
        },
        "required": ["path", "project"]
      }
    },
    {
      "name": "write_file",
      "description": "Write content to a file in the ProxyGit workspace",
      "inputSchema": {
        "type": "object",
        "properties": {
          "path": {"type": "string"},
          "content": {"type": "string"},
          "project": {"type": "string"}
        },
        "required": ["path", "content", "project"]
      }
    },
    {
      "name": "list_directory",
      "description": "List files in a directory",
      "inputSchema": {
        "type": "object",
        "properties": {
          "path": {"type": "string"},
          "project": {"type": "string"}
        },
        "required": ["path", "project"]
      }
    },
    {
      "name": "stat",
      "description": "Get file metadata",
      "inputSchema": {
        "type": "object",
        "properties": {
          "path": {"type": "string"},
          "project": {"type": "string"}
        },
        "required": ["path", "project"]
      }
    },
    {
      "name": "subscribe",
      "description": "Subscribe to file change events",
      "inputSchema": {
        "type": "object",
        "properties": {
          "pattern": {"type": "string", "description": "glob pattern like src/**/*.rs"},
          "project": {"type": "string"}
        },
        "required": ["pattern", "project"]
      }
    }
  ]
}
```

### 3.3 A2A Event Types (Phase 2)

```json
{
  "event": "file_changed",
  "project": "my-project",
  "path": "src/main.rs",
  "old_hash": "abc123...",
  "new_hash": "def456...",
  "agent": "agent-A",
  "timestamp": 1711641600
}
```

---

## 4. MVP Scope

### MVP Must-Have (ship in 2-3 weeks, single person)

1. **proxygit-common**: types, QUIC frame encode/decode, CDC chunker
2. **proxygit-server**: QUIC listener, file index (SQLite), block store (local FS, not S3), handler dispatch for LIST, READ, WRITE, STAT
3. **proxygit-client**: FUSE mount (macOS/Linux via `fuser` crate), QUIC transport, block cache (local dir, simple LRU), WAL append, build intercept via symlink
4. **Demo**: mount a repo, `ls`, `cat` a file, edit a file, `cargo build` works at NVMe speed

### MVP Skipped (Phase 2)

- Garage S3 (use local filesystem for blocks)
- A2A event bus (MCP polls instead)
- Git partial clone integration (manual `git clone --filter=blob:none` first)
- Conflict resolution (last-writer-wins)
- Auth/TLS
- Windows WinFSP
- SLM predictive engine
- Monitoring/observability

### MVP Milestones

| Milestone | What works | Verify |
|---|---|---|
| M1: Common + Server start | Server binary starts, listens on QUIC, responds to LIST_PROJECT | `cargo run --bin proxygit-server` shows "listening on 0.0.0.0:8080" |
| M2: Client connects | Client connects to server, fetches file listing | `proxygit-client mount my-project ~/ProxyGit` shows files |
| M3: FUSE read | `cat ~/ProxyGit/src/main.rs` returns file content hydrated from server | Works end-to-end |
| M4: FUSE write | `echo "change" >> ~/ProxyGit/src/main.rs` writes via WAL, async flushes to server | File updated on server after flush |
| M5: Build intercept | `cargo build` in `~/ProxyGit/my-project` — `target/` goes to local NVMe, source reads are proxied | Build completes, `target/` is on local disk |
| M6: MCP agent interface | Agent calls `read_file` / `write_file` / `list_directory` via MCP | Agent sees and edits files |
