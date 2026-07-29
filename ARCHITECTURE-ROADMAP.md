# ProxyGit Architecture Roadmap

Consolidated from Codex Sol research, Codex CLI review, and OMP review.

> **Current catch-up plan:** [`docs/design/PATH-TO-PAR.md`](docs/design/PATH-TO-PAR.md)
> (delta-on-wire → auth → conflicts → benches). Several P0/P1 rows below are
> already fixed in tree — treat the matrix as historical until refreshed.


## Priority Matrix

| Priority | Issue | Location | Effort | Impact |
|----------|-------|----------|--------|--------|
| 🔴 P0 | `sync_data()` error dropped — false durability | `wal/mod.rs:59` | 1h | Data loss on I/O error |
| 🔴 P0 | `parent_f.sync_all()` error dropped | `block_store/mod.rs:59` | 1h | Data loss on rename |
| 🔴 P0 | `max_early_data_size = u32::MAX` — 0-RTT replay | `server/lib.rs:157` | 30m | Security: replay writes |
| 🟠 P1 | Binary content corruption via MCP `write_file` | `client/lib.rs:423` | 2h | Interop: binary files |
| 🟠 P1 | Partial WAL record loses all prior records | `wal/mod.rs:106` | 3h | Durability on crash |
| 🟠 P1 | MCP protocol version frozen at `2024-11-05` | `client/lib.rs:340` | 30m | Agent compatibility |
| 🟡 P2 | 2 fsyncs/block → 128 fsyncs per 1MB file | `block_store/mod.rs:47` | 4h | Write throughput |
| 🟡 P2 | Entire-file fetch-and-repatch in WAL flush | `wal/mod.rs:230` | 8h | Bandwidth waste |
| 🟡 P2 | N+1 project map stat queries | `client/lib.rs:475` | 2h | Agent latency |
| 🟡 P2 | No QUIC stream reuse across MCP calls | `client/lib.rs` | 4h | Connection overhead |
| 🟡 P2 | Duplicate blake3 hash computation | `server/lib.rs:332/344` | 30m | CPU waste |
| 🔵 P3 | Sequential block reads via N syscalls | `block_store/mod.rs:70` | 4h | Read throughput |
| 🔵 P3 | No GC for orphaned blocks | `block_store/mod.rs` | 6h | Storage bloat |
| 🔵 P3 | Hardcoded `mode = 0o644` in FileEntry | `common/types.rs` | 1h | Permission fidelity |

## Already Fixed

These Codex-identified issues were resolved during the stress-test session:

| Fix | Files |
|-----|-------|
| FUSE write → `EIO` on WAL failure (was silent success) | `fuse_mount.rs:467` |
| WAL flush: network failure defers instead of silently emptying data | `wal/mod.rs:230` |
| Block store: explicit `File::create` + `sync_all` + parent dir `sync_all` | `block_store/mod.rs:43-55` |
| `for (path, patches)` loop restored (was lost in patch) | `wal/mod.rs:228` |
| `use std::io::Write` import added | `block_store/mod.rs:5` |
| `get_project_map` MCP tool (one-call full tree, replaces N+1) | `client/lib.rs:398,459` |

## Architectural Solutions (from Research)

### Phase 1 — Durability & Correctness (P0-P1, ~2 days)

**1.1 Propagate WAL sync errors**
```
// wal/mod.rs:59 — current:
let _ = journal.sync_data();
// fix:
journal.sync_data().context("failed to sync WAL")?;
```
Same fix at `write_staged_records` line 162.

**1.2 Propagate parent dir fsync errors**
```
// block_store/mod.rs:59 — current:
let _ = parent_f.sync_all();
// fix:
parent_f.sync_all().context("failed to sync parent dir")?;
```

**1.3 Bound 0-RTT early data**
```
// server/lib.rs:157 — current:
tls_config.max_early_data_size = u32::MAX;
// fix:
tls_config.max_early_data_size = 8192;  // 8KB, idempotent ops only
```
Also enforce: `READ_FILE`, `STAT_FILE`, `LIST_PROJECT` may use 0-RTT; `WRITE_BLOCKS`, `WAL_FLUSH` require 1-RTT handshake.

**1.4 CRC32C-framed WAL records**
Replace current `[path_len:2B][path][offset:8B][data_len:4B][data]` with:
```
[magic:4B][seq:8B][path_len:2B][path][offset:8B][data_len:4B][data][crc32c:4B][magic_end:4B]
```
On recovery: scan forward. On CRC mismatch or truncated tail, salvage all valid leading records and trim the corrupt tail instead of discarding the entire stage file to `.corrupt`.

**1.5 Base64 MCP binary support**
Add a `base64_content` field to the MCP `write_file` input schema. If present, decode from base64 before writing. This coexists with the existing `content` (UTF-8) field.

### Phase 2 — Performance (P2, ~1 week)

**2.1 Lock-free group-commit WAL**
Replace per-entry `sync_data()` with:
- Lock-free ring buffer for appends
- Background flusher thread at 10ms intervals (or 64KB threshold)
- Single batched `sync_data()` per flush cycle
- `FlushedFuture` returned to callers that resolves when batch sync completes

Expected: ~1000× reduction in fsync calls (from 1 per append to 1 per 10ms batch).

**2.2 Staged batch writes in block store**
Instead of 2 fsyncs per block:
1. Write all N blocks to a `staging/` subdirectory
2. Single `sync_all()` on the staging directory
3. Atomic renames into `blocks/`
4. Single `sync_all()` on the parent directory

Expected: 128 fsyncs/1MB → 2 fsyncs/1MB.

**2.3 Server-side project map with single payload**
Instead of fetching all files then stat'ing each one (N+1 queries):
- Add a `GET_PROJECT_MAP` QUIC message type that returns the full hierarchical tree + sizes in one response
- Cache the result server-side with a generation counter, invalidated on write

**2.4 QUIC stream pool**
Maintain a bounded pool of pre-opened bidirectional streams instead of calling `conn.open_bi()` for every MCP tool invocation.

### Phase 3 — Wireless Optimization (P3, ~2 weeks)

**3.1 Pre-trained Zstd dictionaries**
Train FASTCOVER dictionaries on ProxyGit content types (Rust source, JSON, markdown, SQLite blobs). Ship immutable dictionary IDs in both client and server. Compress CDC chunks before transmitting over QUIC.

**3.2 VCDIFF similarity delta encoding**
When flushing WAL patches:
1. Fetch the **content hash** of the base block (not the entire block)
2. If the base block exists in local cache, compute VCDIFF delta locally
3. Transmit delta instructions instead of full file content
4. Server reconstructs by applying delta to its stored base block

Expected: 80-98% bandwidth reduction for small incremental edits.

**3.3 Batched QUIC frame multiplexing**
Aggregate multiple `STAT`/`READ` requests into a single multi-object QUIC frame. Use QUIC datagrams (RFC 9221) for loss-tolerant speculative pre-fetching.

## Benchmark Protocol

From the research document, to validate gains over simulated lossy wireless:

| Profile | Latency | Jitter | Loss | Simulates |
|---------|---------|--------|------|-----------|
| A | 50ms | 15ms | 1% | 4G LTE |
| B | 200ms | 50ms | 5% | Satellite / poor Wi-Fi |

```
# Linux: simulate with tc netem
tc qdisc add dev eth0 root netem delay 50ms 15ms 1%
```

### Key Metrics

| Metric | Current | Target | How |
|--------|---------|--------|-----|
| WAL write throughput | ~1k writes/s | ≥10k writes/s | Group commit WAL |
| Bandwidth (1KB edit) | Full file | <20KB delta | VCDIFF + Zstd |
| MCP project map | N+1 RTTs | 1 RTT | Bulk query |
| Block writes/1MB | 128 fsyncs | 2 fsyncs | Staged batch I/O |

## Document History

| Date | Author | Change |
|------|--------|--------|
| 2026-07-29 | Codex Sol (Antigravity) | Wireless compression research |
| 2026-07-29 | Codex CLI v0.145.0 | Code review (14 findings) |
| 2026-07-29 | OMP v17.1.2 | Code review (8 findings) |
| 2026-07-29 | Hermes Agent | Consolidation + P0 fixes deployed |
