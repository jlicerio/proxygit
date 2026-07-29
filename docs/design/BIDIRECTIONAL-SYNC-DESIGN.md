# ProxyGit — Bidirectional Sync Design

> **Status:** Draft  
> **Date:** 2026-07-29  
> **Scope:** Sync mechanism for flat-file directory ↔ ProxyGit content-addressed block store  
> **Goal:** Edits made via SMB mount propagate to ProxyGit; writes via ProxyGit CLI/MCP propagate to flat files

---

## Table of Contents

1. [Current Architecture](#1-current-architecture)
2. [Design Questions](#2-design-questions)
3. [Recommended Architecture](#3-recommended-architecture)
4. [Implementation Plan](#4-implementation-plan)
5. [Gotchas & Risks](#5-gotchas--risks)

---

## 1. Current Architecture

```
┌──────────────────────────────┐
│  macOS (workstation)    │
│  ┌─────────────────────┐     │
│  │ /Volumes/proxygit-  │     │  User edits files
│  │ files/ (SMB mount)  │◄────│  via SMB mount
│  └─────────┬───────────┘     │
└────────────┼─────────────────┘
             │ SMB (macOS client → Samba on the server host)
             ▼
┌────────────────────────────────────────────────┐
│  server-host (Docker host)                        │
│                                                 │
│  ┌─────────────────────────┐                    │
│  │ /root/proxygit-files/   │  Flat files dir    │
│  │ (SMB exported via       │  Shared via Samba  │
│  │  smb://server-host/proxygit-files)              │
│  └────────┬────────────────┘                    │
│           │                                     │
│  ┌────────▼────────────────┐                    │
│  │ sync-proxygit.py       │  One-shot script   │
│  │ (WebDAV GET → flat      │  fetches files     │
│  │  files → /root/proxy-   │  from server to    │
│  │  git-files/)             │  flat files         │
│  └────────┬────────────────┘  ⚠ ONE WAY ONLY    │
│           │                                     │
│  ┌────────▼────────────────┐                    │
│  │ Docker Container        │                    │
│  │ ┌──────────────────────┐│                    │
│  │ │ proxygit-server      ││                    │
│  │ │                      ││                    │
│  │ │ WebDAV :3900         ││  ← sync-proxygit   │
│  │ │ QUIC   :8080/udp     ││     .py reads from │
│  │ │        ┌───────────┐ ││     here            │
│  │ │        │ SQLite    │ ││                    │
│  │ │        │ Indexes   │ ││                    │
│  │ │        ├───────────┤ ││                    │
│  │ │        │ Block     │ ││  Content-addressed │
│  │ │        │ Store     │ ││  CDC chunks        │
│  │ │        └───────────┘ ││                    │
│  │ └──────────────────────┘│                    │
│  │ Volume: proxygit-data   │                    │
│  └──────────────────────────┘                    │
│                                                 │
│  (Also on the server host, outside Docker)             │
│  ┌──────────────────────┐                       │
│  │ proxygit-client mcp  │  MCP server on :8082 │
│  │ → writes via QUIC    │  AI agents write here │
│  └──────────┬───────────┘                       │
└─────────────┼───────────────────────────────────┘
              │
              │ QUIC MSG_WRITE_BLOCKS or
              │ WebDAV PUT
              ▼
        proxygit-server
        (block store + index updated)
```

**Current data flow:**
- **Server → Flat files (working):** `sync-proxygit.py` fetches via WebDAV GET, writes flat files to `/root/proxygit-files/`. One-shot, run on-demand.
- **AI Agent writes (working):** MCP `write_file` → client WAL → QUIC → server block store + index.
- **SMB user edits → Server (BROKEN):** SMB edits change flat files on disk, but nothing propagates back to the server's block store.

**Both write paths to the server exist** and work independently:
- **WebDAV PUT** (`handle_put` in `webdav.rs`) — chunks the body, stores blocks, indexes. Currently only `GET` is used by `sync-proxygit.py`.
- **MCP / QUIC write** (`handle_write_blocks` in `lib.rs`) — same chunking + block store + index, but via the QUIC protocol.

---

## 2. Design Questions

### Q1: Detecting changes on the SMB flat-file directory

**Options evaluated:**

| Approach | Latency | CPU Cost | Complexity | SMB Compat? | Reliable? |
|----------|---------|----------|------------|-------------|-----------|
| **A: inotify on the server host** | Near-real-time (sub-second) | Very low | Low | ✅ Events fire for Samba writes | ✅ Kernel-guaranteed |
| **B: Periodic checksum scan** | Poll-interval bound (1-30s) | High (all files read + hashed) | Low | ✅ Works with any FS | ⚠ Misses fast create-delete |
| **C: Periodic mtime/size scan** | Poll-interval bound (1-30s) | Low (stat only) | Low | ✅ Works with any FS | ⚠ False negatives if mtime rounding loses changes |
| **D: fanotify on the server host** | Real-time | Very low | Medium | ✅ | ✅ More features than inotify |

**Recommendation: Hybrid — inotify as primary (on the server host) + periodic mtime scan as backup.**

**Rationale:**
- **inotify is ideal** because the SMB server (Samba) writes to a local Linux filesystem. The kernel delivers `IN_CLOSE_WRITE`, `IN_CREATE`, `IN_DELETE`, `IN_MOVED_FROM`/`IN_MOVED_TO` events reliably on the Docker host's `/root/proxygit-files/`.
- **inotify limitations to handle:**
  - Queue overflow: if events arrive faster than consumed, `IN_Q_OVERFLOW` fires. Mitigation: fall back to a full sweep on overflow.
  - Recursive watches: `inotify` watches single directories. Must add watches for new subdirectories on `IN_CREATE`/`IN_ISDIR`.
  - Race conditions: an event fires after the file is closed, but the file may still be inflight. Mitigation: debounce timer (250ms) per file before acting.
- **inotify-watch management** is simpler than alternatives — use `inotify-simple` (Python) or `inotify` (Rust crate) or `inotifywait` to pipe events.

**Periodic mtime scan as backup:**
- Run every 30 seconds regardless of inotify.
- Compare `{path: (mtime, size)}` map against last known state.
- Catches: missed inotify events, queue overflow recovery, startup synchronization.
- Avoids checksumming — mtime + size is sufficient for change detection. Only read+hash a file when we need to sync it.

**Why NOT pure periodic checksum:**
- For a project with thousands of files, checksumming every file on every poll is expensive (~10-30 seconds per 10K files).
- SMB writes are relatively rare (human editing pace). inotify eliminates most polling waste.

---

### Q2: Writing changes back to ProxyGit

**Options evaluated:**

| Approach | Existing Code? | Latency | Atomicity | Binary OK? | Auth/Transport | 
|----------|---------------|---------|-----------|------------|----------------|
| **A: WebDAV PUT** | ✅ `handle_put` in webdav.rs | One HTTP request | Per-file | ✅ | Raw TCP :3900 |
| **B: MCP `write_file` via TCP** | ✅ Full MCP stack | QUIC RTT | WAL-batched | ✅ (base64) | QUIC TLS :8080 |
| **C: `proxygit-client write` CLI** | ✅ CLI exists | Process spawn + QUIC RTT | WAL-batched | ✅ | QUIC TLS :8080 |
| **D: Direct Rust lib call** | ❌ No Rust daemon | N/A | Would need impl | ✅ | In-process |

**Recommendation: WebDAV PUT (primary) + direct lib call from a Rust daemon (future).**

**Rationale for WebDAV PUT:**
1. **Already exists and works** — `handle_put` in `webdav.rs` accepts `PUT /webdav/{uuid}/{path}` with body, chunks it, stores blocks + indexes.
2. **No additional binary dependencies** — `urllib` / `curl` / `httpx` all work against WebDAV port 3900.
3. **Simple HTTP semantics** — `PUT` is idempotent: re-syncing a file that hasn't changed is safe (just overwrites same blocks).
4. **Content-Length header** ensures we know exactly how much to read on both sides.
5. **Deletion is `DELETE`** — `handle_delete` in `webdav.rs` already handles file deletion.

**Why NOT MCP write_file as primary:**
- MCP requires the full QUIC client stack plus WAL initialization.
- The MCP path adds latency: WAL journal → flush worker → QUIC frame send → server ack.
- WAL is designed for FUSE-like write-journaling (small random writes batched). For the sync daemon (whole-file PUT/POST), WebDAV is simpler.
- However, MCP is the path that *agents* use. The sync daemon must detect when new blocks arrive via MCP (see Q6 below).

**Simple approach — `curl`-based PUT:**
```bash
curl -X PUT \
  --data-binary @/root/proxygit-files/src/main.rs \
  http://127.0.0.1:3900/webdav/00000000-0000-0000-0000-000000000001/src/main.rs
```

**Delete:**
```bash
curl -X DELETE \
  http://127.0.0.1:3900/webdav/00000000-0000-0000-0000-000000000001/src/main.rs
```

---

### Q3: Single process, two processes, or a daemon?

**Recommendation: Single Python daemon (one process, two watch threads).**

**Architecture:**

```
┌───────────────────────────────────────┐
│  proxygit-sync-daemon (Python)         │
│                                       │
│  ┌────────────────────────────────┐   │
│  │ Thread 1: SMB Watch (inotify)   │   │
│  │  ┌──────────────────────────┐   │   │
│  │  │ inotify events on        │   │   │
│  │  │ /root/proxygit-files/    │   │   │
│  │  │ → debounce (250ms/event) │   │   │
│  │  │ → collect changed paths  │   │   │
│  │  └────────┬─────────────────┘   │   │
│  │           │ debounced changes    │   │
│  │           ▼                      │   │
│  │  ┌──────────────────────────┐   │   │
│  │  │ Conflict Check            │   │   │
│  │  │ (mtime vs last_sync)      │   │   │
│  │  └────────┬─────────────────┘   │   │
│  │           │                      │   │
│  │           ▼                      │   │
│  │  ┌──────────────────────────┐   │   │
│  │  │ WebDAV PUT/DELETE →      │   │   │
│  │  │ server-host:3900             │   │   │
│  │  └──────────────────────────┘   │   │
│  └────────────────────────────────┘   │
│                                       │
│  ┌────────────────────────────────┐   │
│  │ Thread 2: Server Watch (poll)   │   │
│  │  ┌──────────────────────────┐   │   │
│  │  │ Poll server file list    │   │   │
│  │  │ (GET /webdav/{uuid}/)    │   │   │
│  │  │ every 2-5 seconds        │   │   │
│  │  │ → compare against local  │   │   │
│  │  │   file index             │   │   │
│  │  → new/modified/deleted     │   │   │
│  │    files → download to      │   │   │
│  │    flat-files               │   │   │
│  │  └──────────────────────────┘   │   │
│  └────────────────────────────────┘   │
│                                       │
│  ┌────────────────────────────────┐   │
│  │ Shared state:                   │   │
│  │ - last_sync_time: {path: unix}  │   │
│  │ - file_mtimes: {path: (mtime,   │   │
│  │   size, tree_hash)}             │   │
│  │ - pending_changes: set()        │   │
│  └────────────────────────────────┘   │
└───────────────────────────────────────┘
```

**Why single daemon:**
1. **Shared state is simpler** — one process maintains the mtime map and change buffer.
2. **Two threads avoid GIL issues** — I/O-bound work (HTTP calls, inotify reads) releases the GIL.
3. **No IPC needed** — no sockets, no files, no semaphores between processes.
4. **Simpler deployment** — one systemd unit or Docker container, one log stream.
5. **Atomic debounce** — both watches see the same `last_sync_time` to detect conflicts.

**Why Python (not Rust):**
- The sync daemon is **integration logic**, not high-performance computation.
- Python has excellent inotify bindings (`inotify-simple`, `pyinotify`).
- `urllib` / `httpx` for WebDAV PUT works trivially.
- The bottleneck is network I/O (inner Docker network + SMB), not CPU.
- **Exception**: If the daemon will be long-lived and needs to be a systemd service, Python is still fine with a simple `while True` loop + threads.

**Rust alternative** for future consideration: If the daemon should be compiled into `proxygit-server` binary itself (eliminating the Python dependency), that's viable but adds complexity.

---

### Q4: Conflict resolution strategy

**Recommendation: Timestamp-based Last-Writer-Wins (LWW) with mtime tracking.**

**Conflict scenario definition:**

A conflict occurs when both sides modify the same file between sync ticks:

```
Time  T1:  Server has file X (content A, tree_hash=H1, mtime=1000)
            Flat file has X (content A, tree_hash=H1, mtime=1000)
      T2:  User edits X via SMB → flat file = content B, mtime=1005
      T3:  MCP agent writes X → server = content C, tree_hash=H3, mtime=1010
      T4:  Sync daemon runs — which version wins?
```

**Resolution algorithm (for each changed file):**

```
def sync_file(path):
    smb_entry = stat(flat_path)
    server_entry = webdav_stat(path)
    
    smb_mtime = smb_entry.st_mtime_ns
    server_mtime = server_entry.mtime  # stored in SQLite index
    
    if not server_entry:
        # File exists on SMB but not on server → CREATE
        if smb_mtime > last_sync_time[path]:
            webdav_put(path, flat_path)  # Upload to server
        else:
            webdav_delete(path)  # Already deleted on server, delete local
        return
    
    if not smb_entry:
        # File exists on server but not on SMB → CREATE from server
        if server_mtime > last_sync_time[path]:
            webdav_get(path, flat_path)  # Download from server
        else:
            indexer_delete(path)  # Already deleted locally, delete from server
        return
    
    # Both exist — which changed?
    smb_changed = smb_mtime > last_sync_time[path]
    server_changed = server_mtime > last_sync_time[path]
    
    if smb_changed and not server_changed:
        webdav_put(path, flat_path)  # SMB wins
    elif server_changed and not smb_changed:
        webdav_get(path, flat_path)  # Server wins
    elif smb_changed and server_changed:
        # TRUE CONFLICT — both sides changed since last sync
        if smb_mtime >= server_mtime:
            webdav_put(path, flat_path)  # SMB wins (later mtime)
            log.warning(f"CONFLICT: {path} — SMB won (LWW, {smb_mtime} vs {server_mtime})")
        else:
            webdav_get(path, flat_path)  # Server wins
            log.warning(f"CONFLICT: {path} — Server won (LWW, {smb_mtime} vs {server_mtime})")
    else:
        # Neither changed — skip
        pass
    
    last_sync_time[path] = max(smb_mtime, server_mtime, time.now())
```

**Conflict handling philosophy:**
- **LWW is appropriate** because ProxyGit is a VFS, not a collaborative editor. Concurrent edits to the same file are rare in practice (single user editing via SMB, agents writing via MCP).
- **Log every conflict** with details for manual resolution if needed.
- **Store conflict copies** as `path.conflict.<timestamp>` when overwriting, so the user can recover.
- **No automatic merge** — content-defined chunking operates at the block level, and blocks are opaque byte sequences. Three-way merge (git-style) would require understanding file structure, which is out of scope.

**Why NOT content-based dedup comparison:**
- Computing hashes of every changed file on every sync is expensive.
- mtime + size is sufficient for change detection. Only read+hash when uploading.
- The server already content-addresses by BLAKE3 — duplicates at the block level are handled naturally.

---

### Q5: Handle file creation and deletion

**Creation (both directions):**

| Direction | Detection | Action |
|-----------|-----------|--------|
| **SMB → Server** | inotify `IN_CREATE` + `IN_CLOSE_WRITE` on a new file | `PUT /webdav/{uuid}/{path}` with file content |
| **Server → SMB** | Server poll detects file in indexer but not on disk | `GET /webdav/{uuid}/{path}` → write flat file |

**Directory creation:**
- Server's WebDAV responds `201 Created` to `MKCOL` but doesn't create actual directory entries (there are no directory entries in the SQLite index; paths are flat).
- mkdir on SMB: create directory path locally (daemon creates it on sync from server).
- No need to sync directories to ProxyGit — directories are implicit from file paths.

**Deletion (both directions):**

| Direction | Detection | Action |
|-----------|-----------|--------|
| **SMB → Server** | inotify `IN_DELETE` / `IN_MOVED_FROM` | `DELETE /webdav/{uuid}/{path}` |
| **Server → SMB** | Server poll: file listed but not on disk | Delete local flat file |
| **Move/Rename** | inotify `IN_MOVED_FROM` + `IN_MOVED_TO` | Treat as DELETE old path + CREATE new path |

**Edge cases:**
- **File replaced by directory (or vice versa):** Handle as DELETE of old + CREATE of new.
- **Temporary editor files:** macOS editors (TextEdit, VS Code) create `.DS_Store`, `._` files, swap files (`.*.swp`), and temp files (`~``.tmp`). Must filter:
  - Skip files starting with `._` (Apple Double)
  - Skip `.DS_Store`, `.localized`
  - Skip editor swap/temp files (`*.swp`, `*.swo`, `*~`, `*.tmp`)
  - Gracefully handle files that disappear during sync (editor temp files)
- **Partial writes:** SMB write then crash. Mitigation: only sync files on `IN_CLOSE_WRITE` (not `IN_MODIFY`), debounce.

---

### Q6: Integration with parallel-agentic-dispatch workflow

The current workflow uses `workstation` (the local agent orchestrator) with parallel agents writing through the SMB mount.

**How the sync integrates:**

```
┌──────────────────────────────────────────────────────────┐
│  workstation (agent orchestrator)                       │
│                                                           │
│  OMP Agent Builder ──► SMB mount ──► /root/proxygit-     │
│  (writes to           (macOS →        files/              │
│   /Volumes/            server-host                           │
│   proxygit-files/)                                        │
│       │                                                   │
│       ▼                                                   │
│  inotify detects write → daemon debounces (250ms)          │
│       │                                                   │
│       ▼                                                   │
│  WebDAV PUT → proxygit-server (block store + index)       │
│       │                                                   │
│       ▼                                                   │
│  Server index updated → next MCP read_file sees new data  │
│                                                           │
│  OMP Agent Reviewer ──► MCP read_file ──► sees new file  │
│  (reads via MCP)         (from index + blocks)             │
└──────────────────────────────────────────────────────────┘
```

**Implications:**
1. **Agent writes via MCP are synchronous** — the agent calls `write_file`, which blocks until the server ack's. The sync daemon's server poll thread picks up the change within 2-5 seconds and writes it to flat files.
2. **SMB writes are near-real-time** — inotify fires within milliseconds. The sync daemon debounces (250ms) then PUT's to the server. Total round trip: ~300-500ms.
3. **Race between agent and SMB writes to same file:** Handled by LWW conflict resolution (see Q4). Since agents and the human user are unlikely to edit the same file within the 2-5 second poll window, this is rare. If it happens, later timestamp wins.
4. **No blocking the agent** — agent writes are independent of the sync daemon. The agent goes through MCP (QUIC), which is a completely separate path.

**Parallel-agentic-dispatch specific notes:**
- Multiple OMP agents writing independent files simultaneously: no conflicts (different files).
- If two agents both write the same file (rare), the second MCP write_file call will overwrite the first on the server, and the sync daemon will download the latest version. This is consistent.
- If an agent writes via MCP and the user edits via SMB at the same time, LWW with conflict logging (Q4) applies.

---

## 3. Recommended Architecture

### High-Level Design

```
┌─────────────────────────────────────────────────────────────────┐
│  proxygit-sync-daemon (Python, systemd service on the server host)      │
│                                                                   │
│  ┌─────────────────────┐        ┌─────────────────────────────┐  │
│  │ Dirsnapshot          │        │ ChangeBuffer                │  │
│  │ {path: (mtime, size,│        │ set(path, direction)        │  │
│  │  tree_hash)}         │        │                             │  │
│  └──────────┬──────────┘        └──────────┬──────────────────┘  │
│             │                              │                     │
│  ┌──────────▼──────────────────────────────▼──────────────────┐  │
│  │ SyncEngine                                                  │  │
│  │  - resolve_conflict(path, smb_mtime, server_mtime)          │  │
│  │  - sync_smb_to_server(path) → urllib PUT/DELETE             │  │
│  │  - sync_server_to_smb(path) → urllib GET + write file       │  │
│  │  - update_snapshot(path)                                    │  │
│  └────────────────────────────────┬───────────────────────────┘  │
│                                   │                              │
│  ┌──────────▼────────┐  ┌───────▼───────────┐                   │
│  │ inotify_watcher    │  │ server_poller      │                   │
│  │ (thread)           │  │ (thread, every 2s) │                   │
│  │                    │  │                    │                   │
│  │ read inotify fd    │  │ LIST /webdav/{id}/ │                   │
│  │ → debounce events  │  │ → compare snapshot │                   │
│  │ → enqueue changes  │  │ → enqueue changes  │                   │
│  └────────────────────┘  └────────────────────┘                   │
│                                                                   │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │ Logging: file conflicts → stderr + file                   │    │
│  │ Metrics: files_synced, conflicts, errors, queue_depth     │    │
│  └──────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────┘
```

### Daemon API / Interface

```
proxygit-sync-daemon [OPTIONS]

Options:
  --uuid <UUID>          Project UUID (default: 00000000-0000-0000-0000-000000000001)
  --webdav-url <URL>     WebDAV base URL (default: http://127.0.0.1:3900/webdav/)
  --flat-dir <PATH>      Flat file directory (default: /root/proxygit-files)
  --poll-interval <SECS> Server poll interval (default: 5)
  --debounce-ms <MS>     inotify debounce ms (default: 250)
  --log-dir <PATH>       Log directory (default: /var/log/proxygit-sync)
  --conflict-backup      Save overwritten files as .conflict.<ts>
  --once                 Run one sync pass then exit (for cron compatibility)
  --daemon               Run as daemon (default: auto-detect)
```

### Deployment

**Option A: Standalone systemd service on the server host (recommended)**

```
# /etc/systemd/system/proxygit-sync-daemon.service
[Unit]
Description=ProxyGit Bidirectional Sync Daemon
After=docker.service network-online.target
Requires=docker.service

[Service]
Type=simple
ExecStart=/usr/local/bin/proxygit-sync-daemon --daemon
Restart=always
RestartSec=5
User=root

[Install]
WantedBy=multi-user.target
```

- Python venv in `/opt/proxygit-sync-daemon/`
- Python dependencies: `inotify-simple`, `urllib3` (stdlib `urllib` is sufficient)

**Option B: Docker container (alongside proxygit-server)**

- Add a second container in `docker-compose.yml` that shares the Docker network (can reach `proxygit-server:3900`).
- Bind-mounts `/root/proxygit-files/` into the container (or use the host's SMB path).
- Simpler management but depends on Docker networking.
- **Docker is less ideal** because the container needs access to the host's inotify and the SMB export path.

**Option C: Cron-based (simplest start)**

```bash
# Every 30 seconds, run a single sync pass
* * * * * /usr/local/bin/proxygit-sync-daemon --once
* * * * * sleep 30 && /usr/local/bin/proxygit-sync-daemon --once
```

This is the **quickest path to MVP** but lacks real-time responsiveness and conflict logging.

### File Change Detection Detail

**inotify watcher events and actions:**

| inotify Event | Meaning | Action |
|--------------|---------|--------|
| `IN_CLOSE_WRITE` | File written and closed | Debounce, then `PUT` to WebDAV |
| `IN_CREATE` + `IN_ISDIR` | New directory | Add inotify watch on it |
| `IN_DELETE` | File deleted | `DELETE` to WebDAV |
| `IN_DELETE` + `IN_ISDIR` | Directory deleted | Remove watch, `DELETE` each file |
| `IN_MOVED_FROM` | File moved out | `DELETE` old path |
| `IN_MOVED_TO` | File moved in | `PUT` new path |
| `IN_Q_OVERFLOW` | Event queue full | Trigger full sweep (mtime scan) |

**Debounce mechanism:**
- Maintain a dict `{path: last_event_time}`.
- On each inotify event, update the timer. Start a per-file debounce timer (250ms).
- When the timer fires without another event for that file, enqueue the path for sync.
- This prevents syncing a file that's still being written.

**Server poll detail:**
- `GET /webdav/{uuid}/` → returns JSON array of `FileEntry` objects with `path`, `size`, `mtime`, `tree_hash`.
- Compare each entry against local `Dirsnapshot`.
- Files that exist remotely but not locally → download.
- Files that exist locally but not remotely → upload (actually rare — this means the server was rolled back or GC'd something).
- Files where `mtime > last_sync_time[path]` on server but not marked by local inotify → download.

### Shared State Persistence

The `Dirsnapshot` must survive daemon restarts:

```json
{
  "version": 1,
  "project_uuid": "00000000-0000-0000-0000-000000000001",
  "last_sync_time": {
    "src/main.rs": 1722181234.567890,
    "src/lib.rs": 1722181000.123456
  },
  "file_snapshots": {
    "src/main.rs": {
      "mtime": 1722181234.567,
      "size": 15234,
      "tree_hash": "a3f8b2c1..."
    },
    "src/lib.rs": {
      "mtime": 1722181000.123,
      "size": 8921,
      "tree_hash": "b7e2a4f1..."
    }
  }
}
```

- Stored as JSON at `/var/lib/proxygit-sync/snapshot.json` (or alongside the daemon).
- Atomically updated after each sync pass.
- On startup, read snapshot to initialize `last_sync_time` and detect changes since last run.

---

## 4. Implementation Plan

### Phase 0: MVP — One-Shot Bidirectional Script (1 day)

Replace `/tmp/sync-proxygit.py` with a script that does both directions:

```python
#!/usr/bin/env python3
"""Bidirectional sync between flat files and ProxyGit block store."""
import json, urllib.request, os, sys, time
from pathlib import Path

UUID = "00000000-0000-0000-0000-000000000001"
BASE = f"http://127.0.0.1:3900/webdav/{UUID}"
OUTDIR = Path("/root/proxygit-files")
SNAPSHOT_FILE = Path("/var/lib/proxygit-sync/snapshot.json")

def list_server_files():
    resp = urllib.request.urlopen(f"{BASE}/")
    return json.loads(resp.read())

def download_file(path):
    data = urllib.request.urlopen(f"{BASE}/{path}").read()
    dest = OUTDIR / path
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(data)
    
def upload_file(path):
    with open(OUTDIR / path, "rb") as f:
        req = urllib.request.Request(f"{BASE}/{path}", data=f.read(), method="PUT")
        urllib.request.urlopen(req)

def delete_file(path):
    req = urllib.request.Request(f"{BASE}/{path}", method="DELETE")
    urllib.request.urlopen(req)
```

**Deliverable:** A script that syncs both directions on each run. Run every 5 seconds via cron or a `while sleep 5` loop.

### Phase 1: Daemon with inotify (2 days)

- Implement the `inotify` watcher thread.
- Implement debounce.
- Implement the server poll thread.
- Add proper logging, state persistence.
- Package as systemd service.

### Phase 2: Conflict handling & recovery (1 day)

- Implement conflict backup files.
- Implement graceful shutdown (SIGTERM → flush pending changes).
- Implement `IN_Q_OVERFLOW` recovery sweep.
- Add metrics for monitoring.

### Phase 3: Optional — Rust rewrite or embedded daemon (2 days)

If Python runtime becomes a dependency burden:
- Embed the sync daemon as a Rust binary alongside `proxygit-server`.
- Use the `inotify` Rust crate.
- Call the block store and indexer directly (in-process) instead of via WebDAV HTTP.
- This eliminates the HTTP overhead for sync.

---

## 5. Gotchas & Risks

### Critical

| # | Gotcha | Impact | Mitigation |
|---|--------|--------|------------|
| 1 | **inotify in Docker container** — if the sync daemon runs in a container, it can't watch the host filesystem for inotify events unless the host's `/root/proxygit-files/` is bind-mounted **and** the container has `CAP_SYS_ADMIN` (or uses `--cap-add=SYS_PTRACE` + `/proc/sys/fs/inotify/` bind mount). | Daemon can't detect SMB changes. | Run sync daemon as **host systemd service**, not in Docker. Or use `--pid=host` + `--cap-add=SYS_ADMIN`. |
| 2 | **inotify watch limit exhaustion** — default `max_user_watches` is 8192. With many files, this fills up quickly. `sysctl fs.inotify.max_user_watches=65536` required. | inotify silently stops watching new files. Add error handling in setup. | Document as deployment prerequisite. Handle `ENOSPC` from `inotify_add_watch`. |
| 3 | **macOS SMB mtime precision** — Samba may round sub-second timestamps on the wire. macOS `write()` timestamps can differ from what Samba reports over SMB. | False conflicts or missed changes. | Use nanosecond mtime resolution (`st_mtime_ns`). Always compare with a 1-second tolerance. |
| 4 | **Concurrent WebDAV PUT + GET race** — server poll thread downloads a file at the same time inotify thread PUT's the same file. | Download gets stale content, immediately overwritten by upload, then download writes stale version. | Serialize per-file operations with a lock. Server poll thread skips files that have an inotify event pending. |
| 5 | **Network split** — Sync daemon can't reach proxygit-server. | Changes queue up in the change buffer. When server comes back, burst of PUTs. | Persist change buffer to disk. Retry with exponential backoff. |

### Moderate

| # | Gotcha | Impact | Mitigation |
|---|--------|--------|------------|
| 6 | **WebDAV PUT no atomicity** — if the server crashes mid-PUT, partial content may be stored. | File corruption. | PUT is idempotent — retry on next sync pass. Server's block store is content-addressed so partial blocks don't affect other files. |
| 7 | **.DS_Store and Apple Double files** — macOS creates these on every directory access. | Thousands of unnecessary PUTs to server. Tight sync loop. | Filter: ignore `._*`, `.DS_Store`, `.localized`, `*.swp`, `*~`, `Icon\r` |
| 8 | **Symlinks on SMB** — Samba may or may not preserve symlinks depending on config. | Daemon tries to follow symlink → file not found. | `os.path.islink()` → skip with warning. Or follow symlinks to their target. |
| 9 | **File permissions** — WebDAV PUT sets `0o644` hardcoded. SMB may set different modes. | Permission drift. | Currently hardcoded (`mode = 0o644` in indexer). Acceptable for MVP. |
| 10 | **Case sensitivity mismatch** — macOS SMB (case-insensitive) vs Linux ext4 (case-sensitive). | Same path with different casing creates two files on Linux, appears as one on macOS. | Document as known limitation. Resolve by normalizing to lowercase on sync? Risky. |
| 11 | **Large binary files** — PUT of a 500MB file to WebDAV. | Timeouts, memory pressure. | Chunked upload? Not needed for ProxyGit's expected workload (source code). Future: stream directly through CDC chunker. |
| 12 | **Startup consistency** — Daemon starts after several hours of offline edits. | Large burst of PUTs. | Full mtime scan on startup: compare every local file against server list, sync all differences. |

### Minor

| # | Gotcha | Mitigation |
|---|--------|------------|
| 13 | Python `urllib` doesn't do connection pooling | Acceptable for MVP. Upgrade to `httpx` if performance matters. |
| 14 | Log rotation | Use systemd's journald or Python `logging.handlers.RotatingFileHandler`. |
| 15 | Duplicate events (macOS SMB writes trigger multiple inotify events) | Debounce handles this. |
| 16 | `.git/` and `node_modules/` in flat files | Filter these out — they exist in the flat files for SMB editing convenience, but should not be synced to ProxyGit (they're either build artifacts or already managed by git). |

---

## Effort Estimates

| Phase | Task | Effort | Dependencies |
|-------|------|--------|-------------|
| **P0** | Script: bidirectional one-shot Python script | **0.5 day** | None (replaces sync-proxygit.py) |
| **P0** | Deploy script as cron job (every 5s) | **0.25 day** | P0 script |
| **P1** | Daemon: inotify watcher thread | **1 day** | P0 script |
| **P1** | Daemon: server poll thread | **0.5 day** | P0 script |
| **P1** | Debounce + per-file locking | **0.5 day** | P1 watcher |
| **P1** | State persistence (JSON snapshot) | **0.25 day** | P0 script |
| **P1** | systemd unit + deployment | **0.25 day** | P1 daemon |
| **P2** | Conflict logging + backup files | **0.5 day** | P1 daemon |
| **P2** | Graceful shutdown + queue persistence | **0.5 day** | P1 daemon |
| **P2** | Metrics (prometheus endpoint or log-based) | **0.5 day** | P1 daemon |
| **P3** | Rust rewrite / embedded sync | **2 days** | P1 daemon (optional) |

**Total to working MVP:** ~1.5 days  
**Full-featured daemon:** ~3.5-4 days  
**Rust-embedded version (optional):** +2 days

---

## Quick Start to MVP

For the absolute fastest path to a working bidirectional sync:

### 1. Enhanced sync-proxygit.py (30 min)

Add upload path to the existing script. Every run:
1. List server files → download new/modified
2. Scan flat file directory → upload new/modified
3. Handle deletions on both sides

### 2. Run in a loop (5 min)

```bash
while true; do
    python3 /tmp/sync-proxygit-enhanced.py
    sleep 5
done
```

### 3. Or cron (5 min)

```bash
* * * * * python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 5 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 10 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 15 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 20 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 25 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 30 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 35 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 40 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 45 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 50 && python3 /tmp/sync-proxygit-enhanced.py
* * * * * sleep 55 && python3 /tmp/sync-proxygit-enhanced.py
```

This polls every 5 seconds without needing a daemon or inotify. **Not elegant, but functional immediately.**

---

## References

- [inotify(7) — Linux manual page](https://man7.org/linux/man-pages/man7/inotify.7.html)
- [Syncthing — Open Source Continuous File Synchronization](https://syncthing.net/)
- [ProxyGit WebDAV Implementation](crates/proxygit-server/src/webdav.rs) — `handle_put` and `handle_delete`
- [ProxyGit Indexer](crates/proxygit-server/src/indexer/mod.rs) — SQLite file index
- [ProxyGit Architecture Roadmap](ARCHITECTURE-ROADMAP.md)
- [ProxyGit Storage Research](STORAGE-RESEARCH.md)
- [Current sync script](file:///tmp/sync-proxygit.py)
