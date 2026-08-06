# ProxyGit Architecture Roadmap

Consolidated from Codex Sol research, Codex CLI review, and OMP review.

> **Catch-up plan status:** [`docs/design/PATH-TO-PAR.md`](docs/design/PATH-TO-PAR.md)
> Phases A–E + stretch (mTLS, feature-hash search, auto base-hash, expanded bench)
> landed on `main`. This matrix was refreshed **2026-07-30** (F0) so open rows
> match the tree.

## Priority Matrix (current)

| Priority | Issue | Status | Notes |
|----------|-------|--------|-------|
| ~~P0~~ | WAL `sync_data` errors dropped | ✅ fixed | Errors propagate; rotate + group-commit fsync |
| ~~P0~~ | Block parent `sync_all` errors dropped | ✅ fixed | Fatal on batch parent fsync |
| ~~P0~~ | `max_early_data_size = u32::MAX` 0-RTT replay | ✅ fixed | `max_early_data_size = 0` |
| ~~P1~~ | MCP binary `write_file` corruption | ✅ fixed | `base64_content` field |
| ~~P1~~ | Partial WAL record discards all records | ✅ fixed | CRC32C frames; salvage leading valid |
| 🟠 P1 | MCP protocol version frozen at `2024-11-05` | open | Bump when agent runners require it |
| ~~P2~~ | 2 fsyncs/block on sparse path | ✅ fixed (F1) | `store_blocks` batch; sparse write uses it |
| ~~P2~~ | Entire-file fetch-and-repatch on WAL flush | ✅ mitigated | Fixed 64 KiB sparse diff + `HAS_BLOCKS` |
| ~~P2~~ | N+1 project map stats | ✅ fixed | `GET_PROJECT_MAP` / `get_project_map` |
| ~~P2~~ | No QUIC stream reuse | ✅ fixed | `StreamPool` |
| 🟡 P2 | Duplicate blake3 in some server paths | open | Low impact CPU polish |
| ~~P2~~ | Per-append WAL fsync | ✅ fixed (F1) | Group-commit ≤10 ms / 64 KiB dirty |
| 🔵 P3 | Sequential block reads (N syscalls) | open | Read throughput |
| ~~P3~~ | No GC for orphaned blocks | ✅ partial | `gc_orphans` present; wire into ops |
| 🔵 P3 | Hardcoded `mode = 0o644` | open | Permission fidelity |
| 🔵 P3 | zstd sparse payloads (A3) | open | Next wire win after F1 |
| 🔵 P3 | ONNX / BGE embeddings | open | Optional; feature-hash is default |
| 🔵 P3 | Multi-tenant RBAC | open | Token + mTLS cover trusted multi-host |
| 🔵 P3 | CRDT / auto-merge | open | `reject_stale` + auto base is enough for agents |
| 🔵 P3 | Garage/S3 backend | open | Local blocks until multi-disk ops |
| 🔵 P3 | Git-aware history / WinFSP | open | Product stretch |

## Already Fixed (historical + F1)

| Fix | Evidence |
|-----|----------|
| FUSE write → `EIO` on WAL failure | `fuse_mount.rs` |
| WAL flush defers on network failure | `wal/mod.rs` |
| CRC32C-framed WAL + salvage | `wal/mod.rs` |
| 0-RTT disabled | `server/lib.rs` `max_early_data_size = 0` |
| Sparse wire writes + HAS_BLOCKS | Phase A |
| Token auth + optional mTLS | Phase B + stretch |
| `reject_stale` + auto base-hash | Phase C + stretch |
| Feature-hash `content_search` | Phase D stretch |
| Batch `store_blocks` + sparse path | **F1** `block_store/mod.rs`, sparse handler |
| Group-commit WAL (10 ms / 64 KiB) | **F1** `wal/mod.rs` `start_group_commit_worker` |
| FUSE waits group-commit | **F1** `append_entry_durable` |

## Phase F — Throughput (in progress / next)

### F0 — Roadmap honesty ✅
This document.

### F1 — Group-commit WAL + batched block durability ✅ (this change)
- **Block store:** `store_blocks` stages → per-file fsync → rename → parent fsync; sparse writes batch.
- **WAL:** appends dirty-count only; group-commit worker fsyncs journal ≤10 ms or at 64 KiB; `append_entry_durable` for FUSE; rotate still fsyncs before rename.
- **Invariant:** fsync errors remain fatal (no `let _ =`).

**Exit criteria**
- Unit: batch store roundtrip; group-commit notifies waiters; durable append clears dirty.
- Integration: existing smoke / auth / mtls / GC suite green.
- Observable: sparse write of N new blocks does one parent-dir fsync set, not 2N.

### F2 — zstd on sparse payloads (next)
Optional wire compression after F1. Update `bench-edit.sh` measured claims.

### F3 — MCP protocol version bump
Only when a real agent runner rejects `2024-11-05`.

### F4 — ONNX BGE behind `PROXYGIT_EMBEDDING=onnx`
Only if feature-hash fails real agent queries.

## Architectural notes (kept)

### Group-commit WAL (implemented shape)
Not a lock-free ring buffer (overkill for current single-process client). Shape:
- Shared journal + `dirty_bytes` / `needs_commit`
- Background task: `select!` on 10 ms tick **or** `Notify`
- One `sync_data()` per batch; oneshot waiters for durable appends
- Rotate path also barriers and drains waiters

### Batched block I/O (implemented shape)
- Unique `staging/<uuid>/` per batch
- **File** fsync per new block (data durability — dir fsync alone is insufficient)
- Atomic rename into `blocks/{prefix}/`
- One parent-dir fsync **per unique prefix** touched (amortized vs per-block)

### Wireless / VCDIFF (later)
Pre-trained zstd dictionaries and VCDIFF remain Phase 3 research tracks.
64 KiB fixed sparse diffs already beat whole-file rsync on edit payload
(~66 KB vs ~1 MB measured).

## Benchmark Protocol

| Profile | Latency | Jitter | Loss | Simulates |
|---------|---------|--------|------|-----------|
| A | 50ms | 15ms | 1% | 4G LTE |
| B | 200ms | 50ms | 5% | Satellite / poor Wi-Fi |

```bash
# Linux: simulate with tc netem
tc qdisc add dev eth0 root netem delay 50ms 15ms 1%
```

### Key Metrics

| Metric | Was | Now / Target | How |
|--------|-----|--------------|-----|
| Sparse 1 KiB edit payload | full file | **~66 KB** measured | Phase A + bench-edit |
| Block writes/1 MB (sparse new) | 2 fsyncs × N blocks | **N file + ≤N parent** fsyncs batched | F1 `store_blocks` |
| WAL append durability | none until rotate / per-write | **≤10 ms group-commit** | F1 |
| MCP project map | N+1 RTTs | 1 RTT | `get_project_map` |
| Bandwidth further | 64 KiB floor | zstd / VCDIFF later | F2+ |

## Document History

| Date | Author | Change |
|------|--------|--------|
| 2026-07-29 | Codex Sol (Antigravity) | Wireless compression research |
| 2026-07-29 | Codex CLI v0.145.0 | Code review (14 findings) |
| 2026-07-29 | OMP v17.1.2 | Code review (8 findings) |
| 2026-07-29 | Hermes Agent | Consolidation + P0 fixes deployed |
| 2026-07-30 | Agent | PATH-TO-PAR A–E + stretch (auth/mTLS/search/bench) |
| 2026-07-30 | Agent | **F0** matrix refresh; **F1** group-commit WAL + batch `store_blocks` |
