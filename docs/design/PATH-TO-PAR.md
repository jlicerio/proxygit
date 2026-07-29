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

## Still false / weak (the gap list)

| Gap | Why it matters | Target |
|-----|----------------|--------|
| Wire path is whole-file | WAL flush fetch→patch→full upload | Delta or chunk-sparse write on wire |
| No auth | Cannot leave lab overlay | Shared secret or mTLS |
| No conflicts | Two agents clobber | Detect + surface; then simple policy |
| Mock “semantic” search | Misleading tool name | Real model **or** rename to hash search |
| No competitive numbers | Claims are vibes | Bench vs baseline rsync/Mutagen-class |
| MCP `2024-11-05` | Agent compat drift | Bump when runners need it |

---

## Phases (risk order)

### Phase A — Make “incremental” real on the wire  ← **start here**

**Why first:** this is the core differentiator we already almost have in storage
but lie about on the network. Auth without this = secure slow rsync.

| Step | Work | Exit criterion (observable) |
|------|------|------------------------------|
| A0 | Spike: instrument bytes-on-wire for `write` of 1KB edit to 1MB file | Log line: `wire_bytes=` before/after; document number |
| A1 | Protocol: sparse write — send only new/changed chunk hashes + data (reuse CDC on client; server already stores by hash) | Server accepts sparse write; unchanged chunks not re-uploaded |
| A2 | WAL flush uses A1 (stop full-file `mcp_write_file` of merged buffer) | 1KB edit bench: wire_bytes **≤ 64KB** (stretch &lt;20KB with compress) |
| A3 | Optional: zstd on chunk payloads | Same bench improves further on text |

**Non-goal in A:** VCDIFF perfection, group-commit WAL, JuiceFS parity.

**Decision log seed:** Prefer **chunk-sparse WRITE** over VCDIFF first — server
already content-addresses chunks; client just shouldn’t resend bytes the server
has. Revisit VCDIFF if chunk boundaries destroy small-edit locality.

### Phase B — Auth (lab → shareable private deploy)

| Step | Work | Exit criterion |
|------|------|----------------|
| B1 | `PROXYGIT_TOKEN` (or file) — client sends on each QUIC stream / WebDAV `Authorization: Bearer` | Request without token → reject; with token → OK |
| B2 | Docs: token mode default-off; compose example with token | QUICKSTART section; site security blurb updated |
| B3 | (Later) mTLS optional | Wrong client cert rejected |

**Non-goal in B:** full multi-tenant RBAC, OAuth, Tailscale ACL integration.

### Phase C — Conflict awareness (not full CRDT)

| Step | Work | Exit criterion |
|------|------|----------------|
| C1 | Write carries expected `tree_hash` (optimistic concurrency) | Stale hash → explicit conflict error, no silent clobber |
| C2 | MCP/CLI surface conflict (`conflict` / `base_hash` / `server_hash`) | Agent can read error and re-fetch |
| C3 | Policy flag: `last_writer_wins` (default) vs `reject_stale` | Documented; test both |

**Non-goal in C:** automatic 3-way merge, OT/CRDT.

### Phase D — Search honesty + optional real embeddings

| Step | Work | Exit criterion |
|------|------|----------------|
| D0 | Rename tool to `content_search` **or** mark `semantic_search` description as hash-stub in all UIs | No user-facing “semantic” without model |
| D1 | (Optional) ONNX BGE-small server-side | Query “authentication middleware” returns relevant rust files on a fixture repo |

Do **D0 immediately** if Phase A slips; never ship hype.

### Phase E — Benchmarks & narrative

| Step | Work | Exit criterion |
|------|------|----------------|
| E1 | Script: create 50MB tree; edit 1KB; measure wall + wire bytes for ProxyGit write path | `scripts/bench-edit.sh` checked in, prints table |
| E2 | Same edit via `rsync` and (if available) Mutagen | Comparison table in README “Benchmark” |
| E3 | Lossy profile optional (`tc netem`) | Numbers under profile A in roadmap |

Only after A2 is green, update README/site from “roadmap” → measured claims.

### Phase F — Stretch (after A–E)

- Group-commit WAL / fewer fsyncs (throughput)
- Block GC (already partially present — verify)
- MCP protocol version bump when agent runners require it
- Windows server / WinFSP
- Garage/S3 backend
- Git-aware history (real “versioned”)

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
- [ ] Token (or mTLS) can lock server; anonymous rejected when enabled
- [ ] Stale concurrent write returns conflict, not silent overwrite (when policy on)
- [ ] No “semantic” / “only diffs” claims without green checks above
- [ ] Bench table in README vs naive full-file baseline
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
