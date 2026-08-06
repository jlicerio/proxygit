# Path to par — ProxyGit

**Goal:** Make the public story true and the product competitive with
“agent-native remote filesystem” expectations — without boiling the ocean into
JuiceFS.

**Bar (from review):** real delta transfer, auth, conflict semantics, honest
benchmarks. Everything else is leverage or polish.

**Invariants**

- Existing CLI / MCP / WebDAV read paths keep working.
- No silent data loss on crash (WAL + block fsync errors stay fatal).
- Trusted-network mode remains available (auth can be optional-off for lab).
- Docs never claim a capability before its exit criterion is green.

**Load-bearing unknown (resolve in Phase A spike):**  
For a 1KB edit in a 1MB file, can we send **only changed CDC chunks** end-to-end
with the current `MSG_WRITE_BLOCKS` shape, or do we need a new message type?
*(Spike ≤1 day. Decision changes Phase A design, not the goal.)*

---

## Already true (do not re-litigate)

| Item | Evidence |
|------|----------|
| End-to-end MVP | QUIC + WebDAV + MCP + CLI smoke green |
| Content-addressed **storage** | FastCDC + BLAKE3 on server block store |
| WAL framing / durability basics | CRC32C frames; `sync_data`/`sync_all` propagate |
| 0-RTT write replay | `max_early_data_size = 0` |
| MCP binary writes | `base64_content` field exists |
| Stream pool | client `StreamPool` in use |
| **Sparse wire writes (Phase A)** | `MSG_WRITE_BLOCKS_SPARSE` + `HAS_BLOCKS`; WAL `build_sparse_diff` 64 KiB; `scripts/bench-edit.sh` |

## Still false / weak (the gap list)

| Gap | Why it matters | Target |
|-----|----------------|--------|
| mTLS optional / multi-tenant RBAC | mTLS landed; RBAC still open | RBAC later |
| Automatic merge / CRDT | Only detect+reject (+ auto base hash) | Later |
| Real ML embeddings | Feature-hash default; ONNX still open | ONNX optional |
| Mutagen/JuiceFS full parity suite | rsync/cp/scp baselines in bench | Expand further if needed |
| MCP `2024-11-05` | Agent compat drift | Bump when runners need it |
| Byte-granular / VCDIFF deltas | 64 KiB block floor | Optional later (A3+) |

---

## Phases (risk order)

### Phase A — Make “incremental” real on the wire  ✅ **landed**

| Step | Status | Notes |
|------|--------|-------|
| A0 wire logging | ✅ | `log_wire_bytes` + `scripts/bench-edit.sh` |
| A1 sparse protocol | ✅ | `0x14` / `0x15` / `0x16`; server handlers |
| A2 WAL + CLI sparse | ✅ | Fixed 64 KiB diff (not CDC-for-diff) |
| A3 zstd payloads | ⬜ optional | |

**Decision locked:** fixed-size block diff for wire sparsity; FastCDC remains a
storage chunker on the legacy `MSG_WRITE_BLOCKS` path. CDC boundaries shift on
small edits and break cross-version alignment.

### Phase B — Auth (lab → shareable private deploy)  ✅ **landed**

| Step | Work | Exit criterion |
|------|------|----------------|
| B1 | `PROXYGIT_TOKEN` (or file) — client sends on each QUIC stream / WebDAV `Authorization: Bearer` | Request without token → reject; with token → OK |
| B2 | Docs: token mode default-off; compose example with token | QUICKSTART section; site security blurb updated |
| B3 | Optional mTLS (`PROXYGIT_MTLS_CA` + client cert/key; `gen-mtls`) | Wrong/missing client cert rejected |

**Non-goal in B:** full multi-tenant RBAC, OAuth, Tailscale ACL integration.

### Phase C — Conflict awareness (not full CRDT)  ✅ **landed**

| Step | Work | Exit criterion |
|------|------|----------------|
| C1 | Write carries expected `tree_hash` (optimistic concurrency) | Stale hash → explicit conflict error, no silent clobber |
| C2 | MCP/CLI surface conflict (`conflict` / `base_hash` / `server_hash`) | Agent can read error and re-fetch |
| C3 | Policy flag: `last_writer_wins` (default) vs `reject_stale` | Documented; test both |
| C4 | Auto base-hash on client when reject mode and hash omitted | Plain write succeeds sequentially under reject_stale |

**Non-goal in C:** automatic 3-way merge, OT/CRDT.

### Phase D — Search honesty + feature embeddings  ✅ **D0 + D1-lite landed**

| Step | Work | Exit criterion |
|------|------|----------------|
| D0 | Rename tool to `content_search` **or** mark `semantic_search` description honestly | No user-facing “semantic ML” without model |
| D1-lite | Feature-hashed bag-of-tokens (`PROXYGIT_EMBEDDING=features`, default); `hash` mock retained | Query with shared tokens outranks unrelated fixture |
| D2 | (Optional) ONNX BGE-small server-side | Real LM neighbors on fixture repo |

### Phase E — Benchmarks & narrative  ✅ **E1/E2 expanded**

| Step | Work | Exit criterion |
|------|------|----------------|
| E1 | Script: create ~1MB file; edit 1KB; measure wall + wire bytes for ProxyGit write path | `scripts/bench-edit.sh` checked in, prints table |
| E2 | Same edit via `rsync` / `cp` / optional `scp` | Comparison table printed by script |
| E3 | Lossy profile optional (`tc netem`) | Numbers under profile A in roadmap |

Only after A2 is green, update README/site from “roadmap” → measured claims.

### Phase F — Throughput (after A–E)  ✅ **F0/F1 landed**

- ✅ F0 Roadmap matrix refreshed (`ARCHITECTURE-ROADMAP.md`)
- ✅ F1 Group-commit WAL (≤10 ms / 64 KiB) + batch `store_blocks` on sparse path
- Group-commit WAL / fewer fsyncs (throughput) — **landed** (see roadmap F1)
- Block GC (already partially present — verify) — `gc_orphans` exists
- MCP protocol version bump when agent runners require it
- Windows server / WinFSP
- Garage/S3 backend
- Git-aware history (real “versioned”)
- F2 zstd sparse payloads (next optional wire win)

---

## Suggested calendar (one focused engineer)

| Week | Focus | Demo at end of week |
|------|-------|---------------------|
| 1 | A0–A2 delta/sparse write | `bench`: 1KB edit ≪ full file on wire |
| 2 | B1–B2 token auth + C1–C2 conflicts | two clients; stale write fails closed |
| 3 | D0 + E1–E2 benches + doc refresh | README numbers; site claims match |
| 4+ | F stretch / real embeddings if needed | optional |

Dual-worker SC OMP still applies per `AGENTS.md`: Builder implements, Reviewer
attacks the exit criteria.

---

## What “at par” means (ship checklist)

- [ ] 1KB edit in ≥1MB file uses ≪ full file bytes on QUIC (measured)
- [x] Token can lock server; bad token rejected when enabled
- [x] Stale concurrent write returns conflict when `reject_stale`
- [x] Docs name content_search stub + measured sparse writes
- [x] Bench table + rsync compare in `scripts/bench-edit.sh` / README
- [ ] `cargo test` + smoke + fmt still green

**Not required for par:** beating JuiceFS at distributed storage, replacing git,
public-internet multi-tenant SaaS.

---

## First command to run Monday

```bash
# A0 spike — measure today's whole-file tax (implement harness if missing)
# edit 1KB in a 1MB file via CLI write; capture payload sizes server-side
```

Then implement A1 against that number.
