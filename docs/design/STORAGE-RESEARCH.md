# ProxyGit — Storage & Security Research Report

> Compiled: July 2026
> Scope: Fast content-addressed storage backends and multi-interface auth models for ProxyGit

---

## Table of Contents

1. [Topic 1: Faster Storage Alternatives](#topic-1-faster-storage-alternatives)
   - [1.1 Current Architecture](#11-current-architecture)
   - [1.2 Backend Storage Options](#12-backend-storage-options)
   - [1.3 Block Layout Strategies](#13-block-layout-strategies)
   - [1.4 CDC Chunk Size Analysis](#14-cdc-chunk-size-analysis)
   - [1.5 Compression Trade-offs](#15-compression-trade-offs)
   - [1.6 Production VFS Systems Analysis](#16-production-vfs-systems-analysis)
   - [1.7 Recommendations](#17-recommendations)
2. [Topic 2: Security Model](#topic-2-security-model)
   - [2.1 Current State](#21-current-state)
   - [2.2 Auth Options Considered](#22-auth-options-considered)
   - [2.3 How Peer Systems Handle Auth](#23-how-peer-systems-handle-auth)
   - [2.4 Multi-Interface Auth Strategy](#24-multi-interface-auth-strategy)
   - [2.5 Recommendations](#25-recommendations)
3. [Implementation Effort Estimates](#3-implementation-effort-estimates)
4. [References](#4-references)

---

## Topic 1: Faster Storage Alternatives

### 1.1 Current Architecture

ProxyGit currently stores blocks as individual files on the local filesystem:

```
data/blocks/
├── a3/
│   └── a3f8b2c1d4e5... (BLAKE3 hex hash = filename)
├── f7/
│   └── f7e1d2c3b4a5...
├── staging/
│   └── <uuid>/ (batch staging dir for atomic writes)
└── ...
```

**Key characteristics:**
- One file per CDC block: ~4–64 KB each (avg ~16 KB)
- 256-way sharding by first 2 hex chars of BLAKE3 hash
- Write path: batch staging dir → fsync stagedir → atomic rename → fsync parent
- Read path: `std::fs::read()` per block → concatenate in order
- No compression
- No caching layer
- BLAKE3 hashing (fast, no HW acceleration needed)

**Current bottlenecks:**
1. **Inode pressure**: millions of tiny files = high inode usage, slow directory operations
2. **Read amplification**: N fsyncs + N renames per write batch (improved to 2 fsyncs/batch)
3. **Read amplification on read**: O(N) `std::fs::read()` syscalls per file read
4. **No prefetch/cache**: every block read hits disk sequentially
5. **Sequential block reads**: blocks are read one-by-one and concatenated—no parallelism

---

### 1.2 Backend Storage Options

#### Option A: Local Filesystem (Current) — Baseline

| Aspect | Assessment |
|--------|-----------|
| **Pros** | Simple, debuggable, no dependencies, works everywhere, predictable |
| **Cons** | Inode-bound at scale, directory operations slow past ~10K files per prefix, no built-in replication |
| **io_uring potential** | Can mitigate syscall overhead for small reads/writes but doesn't fix inode pressure |
| **Best for** | Single-user, small-scale, development |

#### Sub-option: io_uring-optimized Local FS

Using `tokio-uring` or `monoio` for async io_uring-based file I/O:

- **Pros**: Zero-copy buffering, fewer context switches, batched submission/completion
- **Cons**: Linux-only, requires `io_uring` kernel support (5.1+), adds C/Rust dependency complexity
- **Estimated gain**: 20–40% reduction in syscall overhead for small-block workloads
- **Rust crate status**: `tokio-uring` (active, alpha), `compio` (active, more complete IO abstraction)
- **Verdict**: High-value optimization **after** block layout is fixed, not before

#### Option B: SQLite as Block Store

Store all blocks as BLOBs in a single SQLite database per project (or globally).

| Aspect | Assessment |
|--------|-----------|
| **Pros** | Single file = no inode pressure, WAL mode for concurrent reads/writes, transactional, already a dependency in ProxyGit, built-in caching (page cache), fsync-batched by SQLite |
| **Cons** | BLOB retrieval requires SQL query (small overhead vs direct file read), database size limits (practical max: 100–200 GB for WAL mode), vacuum needed after GC |
| **Read perf** | SQLite page cache (~2MB default, configurable) provides hot-block caching automatically |
| **Write perf** | WAL mode batches writes; single wal sync per batch vs current 2 fsyncs |
| **GC** | DELETE + VACUUM (or auto_vacuum) — but VACUUM rewrites entire DB, expensive |
| **Scaling** | Single DB file becomes contention point for concurrent writes; per-project DB mitigates this |
| **Verdict** | Strong candidate for ProxyGit's scale — simple, already a dep, eliminates inode problem |

**Benchmark data point (known from SQLite literature):**
- SQLite on NVMe: ~50K–120K small BLOB reads/sec with page cache warm
- SQLite on NVMe: ~30K–60K small BLOB writes/sec in WAL mode
- Raw fs on NVMe: ~100K–300K small file reads/sec (but degrades with directory size)
- **Key insight**: SQLite's advantage is consistency of performance at scale, not raw peak throughput

#### Option C: LMDB (Lightning Memory-Mapped Database)

Embedded B+tree key-value store using memory-mapped files.

| Aspect | Assessment |
|--------|-----------|
| **Pros** | Blazing fast reads (mmap = zero-copy once mapped), single file, ACID, no WAL overhead, read transactions don't block each other, incredibly low overhead |
| **Cons** | Write transaction is single-writer (serialized), DB size limited by virtual address space, data file is "live" mmap — corruption if disk fills, must tune map size upfront |
| **Read perf** | Best-in-class for read-heavy workloads — mmap means kernel page cache does the work |
| **Write perf** | Single writer = bottleneck for concurrent PUT operations; perf ~30K–80K writes/sec |
| **GC** | LMDB doesn't truly delete — marked free and reused on next write via copy-on-write B-tree |
| **Maturity** | Battle-tested (used in OpenLDAP, Lightning Server), Rust bindings (`heed`, `libmdbx`) |
| **Verdict** | Excellent read path; write path is the concern. Best if writes are batched and infrequent |

#### Option D: RocksDB

Log-Structured Merge-tree (LSM-tree) embedded key-value store.

| Aspect | Assessment |
|--------|-----------|
| **Pros** | Excellent write throughput (compaction-based, not in-place update), configurable compression per column family, bloom filters for point lookups, range scans for iteration |
| **Cons** | Complex tuning (memtable size, compaction strategy, bloom filter bits), write amplification (compaction), background compaction threads, larger footprint |
| **Read perf** | Good (bloom filters) but can spike during compaction; range scans excellent |
| **Write perf** | Best among embedded KVS — 200K+ writes/sec on NVMe |
| **Compaction** | Wears out SSD with write-amplification (5–20x before tuning, 2–5x tuned) |
| **Rust bindings** | `rust-rocksdb` (mature, C++ bridge via bindgen) |
| **Verdict** | Overkill for ProxyGit's access pattern (small blocks, sequential reads, burst writes) |

#### Option E: Garage S3 (Remote Backend)

Distributed object store with S3-compatible API.

| Aspect | Assessment |
|--------|-----------|
| **Pros** | Geo-distributed, resilient (erasure coding), multi-tenancy, S3 API for broad tooling compatibility |
| **Cons** | Network latency (even local), higher per-request overhead, requires cluster operation, small-object penalty (each block is a distinct S3 PUT/GET) |
| **Garage vs MinIO** | Garage designed for geo-distributed, KISS principle, no consensus (Dynamo-style); MinIO uses Raft consensus, better single-cluster perf but worse geo-latency |
| **Small objects** | Both incur ~1–5ms per request for small objects (HTTP/S3 overhead dominates) |
| **Verdict** | Good for replication/off-site, poor as primary block store for ProxyGit |

#### Option F: Memory-Mapped Block File

Instead of `std::fs::read()`, open a single (or few) large file(s) and mmap it, locating blocks via a separate index.

| Aspect | Assessment |
|--------|-----------|
| **Pros** | Zero-copy reads (kernel page cache), no per-block syscall overhead, pages are demand-loaded |
| **Cons** | Mmap overhead for small random writes (write amplification if not page-aligned), SIGBUS on disk full if file is sparse, portability issues (mmap on macOS vs Linux) |
| **Verdict** | High-risk, moderate reward. LMDB essentially *is this* done correctly. |

---

### 1.3 Block Layout Strategies

#### Strategy 1: One File Per Block (Current)

```
blocks/a3/a3f8b2c1d4e5...
blocks/a3/a3f8b2c1d4e6...
```

- **Pros**: Simple, easy to debug, easy GC (just delete files)
- **Cons**: Inode explosion at scale, `readdir` on large prefix dirs gets slow, many small writes
- **Max practical scale**: ~100K blocks per prefix dir before ops slow noticeably

#### Strategy 2: Pack Files (Used by restic, borg, git)

Multiple blocks concatenated into larger pack files (e.g., 50–200 MB each), with an index mapping hash → pack-offset.

```
packs/
├── 001.pack
├── 001.pack.idx   (hash → (pack, offset, length))
├── 002.pack
├── 002.pack.idx
└── ...
```

**How restic does it:**
- Packs contain 1+ encrypted blobs + encrypted header + header length
- Index tracks which blocks are in which pack
- GC rewrites packs (remove dead blocks)
- Pack size: configurable (default ~16 MB)

**How SeaweedFS does it:**
- "Volumes" are 30 GB files on disk
- Each volume stores many "needles" (small files/blocks)
- Needle index in memory (or LevelDB on disk)
- Volume compaction for GC (rewrites volume dropping deleted needles)

**Pros vs one-file-per-block:**
- Drastically fewer files (thousands vs millions)
- Better filesystem layout (sequential writes within pack)
- Better readahead (locality of reference within pack)
- Lower metadata overhead

**Cons:**
- GC requires pack rewriting (expensive)
- Random-access within pack requires index
- Pack fragmentation over time without compaction

**Verdict: Strong recommendation for ProxyGit.** Pack files are the proven approach at scale.

#### Strategy 3: Column-Family Key-Value (LMDB/RocksDB)

Blocks stored as (hash → data) in an embedded KVS.

- Already covered in Options C/D above.
- **Key difference from pack files**: No explicit GC — compaction/COW is automatic.
- Trade-off: Less control over data layout, but dramatically simpler code.

#### Strategy 4: Content-Routed S3

Blocks stored as individual objects in S3/Garage/MinIO.

- Simple, but each block requires a full HTTP round-trip.
- **Mitigation**: Batch block writes into multi-part upload transactions.
- **Mitigation**: Use S3 gateway cache layer (e.g., `s3fs`, `goofys`, `s3backer`).
- Still suffers from per-object overhead.

---

### 1.4 CDC Chunk Size Analysis

ProxyGit currently uses min=4KB, avg=16KB, max=64KB. Here's how different workloads react:

| Workload | Small chunks (avg 4–8 KB) | Medium chunks (avg 16–32 KB) | Large chunks (avg 64–128 KB) |
|----------|--------------------------|-----------------------------|------------------------------|
| **Source code** | Best dedup ratio, many tiny files already small | Good dedup, decent storage overhead | Poor dedup, wasted space on small files |
| **Binaries (.wasm, .o)** | Moderate dedup, overhead for runtime | Sweet spot for binary dedup | Good dedup, better streaming read |
| **Media (PNG, JPEG, MP4)** | Poor dedup (already compressed), many useless chunks | Poor dedup, wasted CPU cycles | Best — avoid re-chunking pre-compressed data |
| **Archives (.tar, .zip)** | Good dedup if similar archives differ slightly | Same as small | Misses dedup opportunities |
| **Documents (.docx, .pdf)** | Good dedup for editorial changes | Good tradeoff | Misses fine-grained dedup |

**Known practices in production systems:**

| System | Chunk size | Notes |
|--------|-----------|-------|
| **restic** | Default ~1 MiB • 512 KiB min • 8 MiB max | Large chunks, optimized for backup (large files) |
| **borg** | Default 16 KiB (CDC) | Proven for deduplicating backups of many small files |
| **casync** | Default 16 KiB | Systemd's content-addressable archiver |
| **syncthing** | Default 128 KiB (fixed, not CDC) | Uses fixed-size blocks, not CDC |
| **Git** | Not CDC — uses whole-file + delta | Different approach entirely |

**Recommendation for ProxyGit:**

The current defaults (min=4K, avg=16K, max=64K) are reasonable for a VFS that primarily stores source code and documents. However:

- **Source code repos**: 16 KB avg is good. Consider dropping min to 2K for tighter dedup on small source files.
- **Binary/media projects**: Consider a **workload-aware chunker** that detects compressed data (via magic bytes) and skips CDC entirely (whole-file chunking with max-size cap).
- **Hybrid approach**: Per-project chunk config in SQLite index (store chunker params in project metadata).

**Suggested revised defaults:**
- Default: min=4K, avg=16K, max=64K (keep)
- Source-code detected (heuristic): avg=8K for better dedup on small files
- Pre-compressed detected: avg=128K to avoid futile chunking
- Make configurable per-project via `proxygit.toml`

---

### 1.5 Compression Trade-offs

#### No Compression (Current)

- **Pro**: Zero CPU overhead, max throughput
- **Con**: Storage-inefficient for text (source code, configs compress 3–5x)
- **Worst-case**: Logs, JSON, CSVs — extremely compressible, wasting bandwidth and disk

#### Per-Block Zstd

Each CDC chunk compressed independently.

| Level | Comp Ratio (text) | Comp Speed | Decomp Speed | Use Case |
|-------|-------------------|-----------|-------------|----------|
| 1     | ~3x               | 500 MB/s  | 1.2 GB/s    | Default for ProxyGit |
| 3     | ~3.5x             | 300 MB/s  | 1.1 GB/s    | Balanced |
| 6     | ~4x               | 150 MB/s  | 900 MB/s    | Archive |
| 16    | ~5x               | 20 MB/s   | 700 MB/s    | Deep archive (not recommended) |

**Recommendation: Zstd level 1 (or 3)**

- Zstd level 1 is **extremely fast** — often faster than reading uncompressed from disk due to reduced I/O
- Decompression at level 1 is ~1.2 GB/s — won't bottleneck anything
- Per-block compression enables **random access**: no need to decompress entire pack to get one block
- Downside: each small block compresses less efficiently than a large batch (small-packet overhead)
- Zstd has dictionary mode for even better compression on similar data (e.g., source files in same project) — more complex to implement

#### Batch Compression (Pack-level)

Compress entire pack file together (as restic does).

- **Pro**: Better compression ratio (cross-block redundancy, larger corpus)
- **Con**: Must decompress entire pack to access one block (or keep uncompressed index)
- **Verdict**: Good for backup workloads (sequential reads), bad for VFS (random block access)

#### Adaptive Compression

Skip compression for already-compressed content (detected via magic bytes or a quick entropy check):
- Media files (JPEG, PNG, MP4, MP3) — skip compression (or try at most once)
- Binary formats (.wasm, .o, .so) — skip (already compiled)
- Source code, configs, JSON — always compress

**Estimated savings**: 30–50% storage reduction with ~5% throughput hit for compressible content.

#### Recommendation for ProxyGit

1. **Default**: Zstd level 1 per-block compression
2. **Skip heuristic**: Check first 4 bytes (magic bytes) — skip compression for:
   - JPEG (`0xFF D8 FF`), PNG, GIF, WebP
   - ZIP/zlib/gzip (0x78, 0x1F 0x8B)
   - MP4 (`ftyp`), MP3 (`ID3` or `0xFF FB`)
3. **Future**: Consider zstd dictionary trained per-project for an extra ~20% compression on source code

---

### 1.6 Production VFS Systems Analysis

#### SeaweedFS
- **Storage**: Large volumes (30 GB files on disk), each storing many small "needles"
- **Index**: LevelDB (on-disk) for needle → volume mapping; plus in-memory needle index per volume
- **Small objects**: **Excellent** — this is SeaweedFS's primary design goal (billions of small files)
- **Key lesson for ProxyGit**: Batch small objects into larger chunks on disk, maintain separate index

#### MinIO
- **Storage**: Each object is a file on the underlying filesystem (xfs/ext4 recommended)
- **Small objects**: Not optimized — suffers from the same one-file-per-object problem at scale
- **MinIO's answer**: Erasure coding splits objects into data+parity shards (typically 4–16 shards)
- **Key lesson for ProxyGit**: Even MinIO struggles with billions of tiny objects; they recommend using larger objects or batching

#### Ceph (RADOS)
- **Storage**: Objects stored in OSDs, each OSD uses a flat filesystem (BlueStore uses RocksDB internally)
- **Small objects**: BlueStore uses RocksDB to index small objects, stores large objects as raw files
- **Configurable min_alloc_size**: Controls minimum allocation unit (4K, 16K, 64K)
- **Key lesson for ProxyGit**: Hybrid approach — use KVS for metadata/index, raw storage for bulk data

#### Garage
- **Storage**: Each object stored as a file, with metadata in SQLite
- **Small objects**: Not specifically optimized; designed for geo-distribution over small-object performance
- **Key lesson for ProxyGit**: The SQLite-for-metadata approach is sound; blocks stored as files is the bottleneck

#### Git
- **Storage**: Pack files (`.pack`) + index (`.idx`) — conceptually what we want
- **Small objects**: Packs compress together; loose objects only temporarily
- **Key lesson for ProxyGit**: Git's proven approach is the model: loose objects → periodic repack

#### Summary of Production Lessons

| System | Block Layout | Index | Small-Object Strategy |
|--------|-------------|-------|----------------------|
| **SeaweedFS** | Large volumes | LevelDB + in-memory | Merge into 30 GB volumes |
| **MinIO** | One file per object | Filesystem | Suffers at tiny objects |
| **Ceph BlueStore** | RocksDB KVS + raw | RocksDB | Hybrid KVS/filesystem |
| **Garage** | One file per object | SQLite | Default ext4, no batching |
| **Git** | Pack files | .idx + bitmap | Loose → periodic pack |
| **ProxyGit (current)** | One file per block | SQLite | No optimization yet |

---

### 1.7 Storage Recommendations for ProxyGit

#### Immediate (Phase 1 — 5 days)

**Keep local filesystem, but switch to pack-file layout.**

- Pack size: 16 MB target (configurable)
- Pack file: `packs/<prefix>/<pack_id>.pack`
- Index: SQLite table `pack_index` (block_hash → pack_id, offset, compressed_len, decompressed_len)
- Write: Buffer block writes to 16 MB → flush as pack → update index
- Read: Index lookup → seek within pack → read + decompress range
- Compression: Zstd level 1 per-block within pack

This eliminates inode pressure, enables sequential pack writes, and gives all the read-locality benefits.

#### Short-term (Phase 2 — 3–5 days)

**Add pack compaction (GC) and compression-skip heuristic.**

- GC: Read pack, filter to live blocks, rewrite new pack, delete old pack, update index
- Compression-skip: Magic-byte detection for incompressible content
- Configurable per-project chunk sizes

#### Medium-term (Phase 3 — 7–10 days)

**Evaluate LMDB as unified block+index store.**

- Single LMDB env per project (or per server) containing:
  - block data (key=hash, value=compressed_data)
  - file metadata (key="file:{path}", value=FileEntry)
  - file blocks (key="fblock:{path}:{offset}", value=block_hash)
- Eliminates separate SQLite index and pack files
- Built-in caching (mmap)
- Single-writer is acceptable for ProxyGit's expected write volume

#### Long-term (Phase 4)

**io_uring optimization + Garage S3 tier.**

- `tokio-uring` for async I/O on pack files (Linux only; fallback to tokio fs)
- Garage S3 as remote replication target (async sync of packs)
- Local → S3 tiering with LRU eviction policy

---

## Topic 2: Security Model

### 2.1 Current State

ProxyGit currently has **zero authentication** on all three interfaces:

| Interface | Transport | Auth | Status |
|-----------|-----------|------|--------|
| **QUIC** | TLS 1.3 (self-signed cert, `with_no_client_auth()`) | None | Anyone who connects can read/write |
| **WebDAV** | Raw TCP (no TLS) | None | Anyone on network can mount as drive |
| **MCP** (future) | TBD | None | TBD |

This is acceptable for development but must be addressed before any real deployment.

---

### 2.2 Auth Options Considered

#### Option A: Tailscale Identity-Based Auth

**Concept**: All ProxyGit nodes are on the same tailnet. The server trusts `whoami` from Tailscale (node identity, not user identity).

- **How it works:**
  - Tailscale assigns each node a unique identity (node key + tailnet IP)
  - QUIC: Tailscale already encrypts inter-node traffic with WireGuard. No additional TLS needed — trust the node's Tailscale IP.
  - WebDAV: Bind to Tailscale interface only (`tailscale0`, `100.x.x.x`). Only tailnet nodes can reach it.
  - MCP: Same trust model — if you're on the tailnet, you're authorized.

- **Pros:**
  - Zero configuration for auth (Tailscale handles identity)
  - Tailscale ACLs provide granular access control
  - WebDAV simply binds to Tailscale IP — no auth code needed
  - Works off-network via Tailscale relay/DERP

- **Cons:**
  - Requires all users to have Tailscale installed and on the same tailnet
  - Not usable outside Tailscale ecosystem
  - Tailscale ACL changes are global (admin console)
  - Proxies the identity question to Tailscale (move trust)

- **Verdict: Excellent "just works" model for small teams.** The simplest path to auth.

#### Option B: Mutual TLS (mTLS)

**Concept**: Both client and server present X.509 certificates. Server verifies client cert's CA; client identity is extracted from the cert CN or SAN.

- **How it works:**
  - Server has a CA certificate
  - Each client generates a key pair and gets a cert signed by the server's CA
  - QUIC: Rustls `with_client_auth()` — built-in, no extra code
  - WebDAV: HTTP-over-TLS with `SSLVerifyClient require` (or in Rust, wrap with TLS acceptor that requires client certs)
  - MCP: Same as QUIC — mTLS is transport-level

- **Pros:**
  - Strong cryptographic identity — no external dependency
  - Works everywhere, no Tailscale required
  - Certificate can embed roles (e.g., O=admin, OU=readonly via custom SAN/OU)
  - QUIC and HTTP share the same Rustls/TLS stack

- **Cons:**
  - Certificate management (issuance, rotation, revocation)
  - Must build a PKI or integrate with `step-ca`, cert-manager, etc.
  - CRL distribution / OCSP stapling
  - Self-signed CA means manual trust anchor distribution
  - More complex than Tailscale identity

- **Verdict: Strong model for production deployments without Tailscale.** Recommended as the non-Tailscale auth path.

#### Option C: Token-Based Auth (Bearer Tokens)

**Concept**: Each client authenticates with a bearer token (per-user or per-project). Token verified against a server-side store.

- **How it works:**
  - WebDAV: `Authorization: Bearer <token>` header on every request
  - QUIC: Custom frame with token on connect, or token in protocol handshake
  - MCP: Standard HTTP `Authorization` header

- **Pros:**
  - Familiar model (works like GitHub, Docker, etc.)
  - Token scoping (per-project: different tokens for different repos)
  - Can be short-lived, revocable, and auditable
  - No TLS infrastructure needed (but still recommended)

- **Cons:**
  - Must implement token issuance, validation, and revocation logic
  - Token leak is dangerous (like password leak)
  - Every request carries token overhead
  - QUIC doesn't have native "header" concept — must add to protocol frame
  - No identity binding (token != person)
  - Token rotation requires client coordination

- **Verdict: Useful as a complement (e.g., CI/CD tokens) but weak as primary auth for a personal/team VFS.**

#### Option D: SSH-Style Key Auth

**Concept**: Clients have an SSH public key (or similar) registered with the server. Authentication is proof of private key possession.

- **How it works:**
  - Client connects, sends public key fingerprint
  - Server challenges client to sign a nonce
  - Client signs with private key, server verifies

- **Pros:**
  - Familiar to developers (like SSH to servers)
  - No central PKI needed
  - Keys can be managed via `ssh-keygen` / `~/.ssh/authorized_keys`
  - No TLS certificate management

- **Cons:**
  - Must implement custom challenge-response in protocol
  - Not built into TLS stack — must be application-level
  - Key management at scale (hundreds of keys) is painful
  - No standard for WebDAV SSH auth

- **Verdict: Niche. Not recommended as primary auth for multi-interface system.**

---

### 2.3 How Peer Systems Handle Auth

#### Garage S3

| Auth feature | Implementation |
|-------------|----------------|
| **Primary auth** | HMAC-SHA256 (AWS Signature V4) — access key + secret key per user |
| **Multi-tenancy** | Per-user API keys, bucket-level permissions |
| **OIDC/LDAP** | Not built-in; ask for it |
| **TLS** | Supports TLS termination (no mTLS) |
| **Admin token** | Single global admin token (`-a` flag) for cluster management |
| **ACL model** | S3-style bucket policies (limited) + per-key permissions |

**Key takeaway**: Garage uses S3's HMAC model, not TLS identity. It's designed for the S3 API ecosystem.

#### MinIO

| Auth feature | Implementation |
|-------------|----------------|
| **Primary auth** | AWS Signature V4 (access key + secret key) |
| **Multi-tenancy** | IAM policies, groups, users |
| **OIDC** | First-class OIDC support (Keycloak, Dex, Google, etc.) |
| **LDAP** | Enterprise SSO via LDAP/AD |
| **mTLS** | Not typically used for client auth (terminal-only TLS) |
| **Policy engine** | Rich policy language (effect, action, resource, condition) |

**Key takeaway**: MinIO has the richest auth model — OIDC integration is the recommended path for enterprise deployments.

#### Syncthing

| Auth feature | Implementation |
|-------------|----------------|
| **Device identity** | TLS certificate fingerprint (SHA-256 of certificate = Device ID) |
| **Cluster auth** | Device ID must be explicitly added to the cluster (accept list) |
| **Transport** | TLS 1.3 between all devices |
| **User auth** | GUI has username/password (optional); data-plane is device-to-device only |
| **Relay** | Relay connections are TLS-encrypted, relay operator cannot read data |

**Key takeaway**: Syncthing's model is the closest analog to ProxyGit's use case. Device identity via TLS fingerprint + explicit authorization. No central auth server. This is **exactly** what ProxyGit should emulate.

---

### 2.4 Multi-Interface Auth Strategy

The challenge: QUIC, WebDAV, and MCP all need to authenticate with the same identity and authorization model.

#### Option: Transport-Level Auth (Recommended)

Use **TLS client certificates (mTLS)** as the common identity layer:

```
┌─────────────────────────────────────────────────┐
│                 ProxyGit Server                 │
│                                                  │
│  ┌──────────────────────────────────────────┐   │
│  │         Auth Layer (TLS + CA)            │   │
│  │  Client cert → extract CN/subject →      │   │
│  │  → role/identity → authorized operations │   │
│  └──────────────────────────────────────────┘   │
│        ↑ TLS       ↑ HTTP+TLS    ↑ HTTP+TLS     │
│   ┌──────────┐ ┌──────────┐ ┌──────────┐       │
│   │  QUIC    │ │ WebDAV   │ │   MCP    │       │
│   │ (quinn)  │ │ (hyper)  │ │ (axum)   │       │
│   └──────────┘ └──────────┘ └──────────┘       │
└─────────────────────────────────────────────────┘
```

**Implementation:**

1. **Server CA**: Generate a CA certificate. All client certs signed by this CA.
2. **QUIC**: Rustls `ServerConfig::with_client_auth()` with CA cert store. `quinn` passes client cert chain.
3. **WebDAV**: Upgrade to HTTPS. Use `rustls::ServerConfig` with client auth. Extract cert from TLS session in request handler.
4. **MCP**: Same TLS stack as WebDAV — reuse the same listener/acceptor.

**Authorization (once identity is established):**

```
┌──────────┐    ┌──────────┐    ┌──────────────┐
│ Identity  │───→│ Role     │───→│ Permissions  │
│ (CN from  │    │ Mapper   │    │ (project R/W)│
│  cert)    │    └──────────┘    └──────────────┘
└──────────┘
```

- Simple file-based role mapping: `cert_cn → role`
- Or directory-based: external process watches a directory for `*.toml` role files
- Roles: `admin` (all projects, all ops), `writer:<project>` (read+write specific project), `reader:<project>` (read-only)

**Certificate issuance workflow:**

```
# For each user/client:
1. openssl genpkey -algorithm ED25519 -out client.key
2. openssl req -new -key client.key -out client.csr -subj "/CN=alice@team"
3. openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out client.crt
```

Or automate with `step-ca` for auto-renewal.

#### Alternative: Tailscale-Only Mode

If running within Tailscale, auth is **delegated entirely to Tailscale**:

- Bind WebDAV/MCP to `tailscale0` interface IP only
- QUIC uses Tailscale's WireGuard encryption (disable app-level TLS or keep it for defense-in-depth)
- "Who are you?" = Tailscale node IP / hostname
- Authorization via Tailscale ACLs:
  ```
  "acls": [
    {"action": "accept", "src": ["tag:proxygit-client"], "dst": ["tag:proxygit-server:*"]}
  ]
  ```

This is the **simplest possible auth model** — zero implementation, works out of box.

#### Hybrid: Tailscale + Bearer Tokens for CI

- Human users: Tailscale identity (or mTLS if off-tailnet)
- CI/CD / automation: Bearer tokens per project (stored as GitHub Actions secrets, etc.)
- Token validation happens at application layer regardless of transport

---

### 2.5 Security Recommendations for ProxyGit

#### Phase 1: Tailscale-Only (2–3 days)

1. Bind all interfaces to Tailscale IP
2. Document "run within Tailscale" as the default deployment mode
3. Optionally, add node-identity check (accept connections only from whitelisted tailnet IPs)
4. **Result**: Full auth with **zero code changes** — just configuration

#### Phase 2: mTLS Auth (7–10 days)

For deployments outside Tailscale:

1. Add CA support to server: `--ca-cert` flag
2. Add client-certificate generation command to CLI: `proxygit auth init`
3. Upgrade WebDAV to HTTPS with client cert verification
4. Add cert fingerprint to QUIC identity verification
5. Simple role mapper: `acl.toml` file mapping cert CN → project permissions
6. Wire MCP with same TLS config

#### Phase 3: Bearer Tokens for CI (3–5 days)

1. Add `proxygit token create --project <id> --readonly` command
2. Token validation in server (blake3-hashed tokens stored in SQLite)
3. WebDAV: `Authorization: Bearer` header
4. QUIC: New `MSG_AUTH` frame type (token sent once per connection)
5. Rate limiting and token revocation

---

## 3. Implementation Effort Estimates

### Storage

| Task | Effort | Dependencies |
|------|--------|-------------|
| **Phase 1: Pack files** | 5 days | None |
| • Design pack format + index schema | 0.5 day | — |
| • `BlockStore` rewrite (pack writer + reader) | 2 days | Pack format |
| • Index (SQLite `pack_index` table) | 1 day | Pack format |
| • Block path migration utility | 1 day | Pack format |
| • Tests + benchmark | 0.5 day | All above |
| **Phase 2: Compression** | 3 days | Phase 1 |
| • Zstd per-block in pack writer | 1 day | Phase 1 |
| • Magic-byte skip heuristic | 0.5 day | — |
| • Configurable chunk sizes | 0.5 day | — |
| • Pack compaction/GC | 1 day | Phase 1 |
| **Phase 3: LMDB evaluation** | 7–10 days | (independent) |
| • LMDB integration prototype | 2 days | — |
| • Benchmark: LMDB vs pack+SQLite | 2 days | Prototype |
| • Migration path (pack → LMDB) | 3 days | Decision |
| • Tests | 1 day | — |
| **Phase 4: io_uring + Garage** | 7–10 days | Phase 1–2 |
| • `tokio-uring` integration | 3 days | — |
| • Garage S3 backend | 3–5 days | — |
| • Tiering (local ↔ remote) | 3 days | Both backends |

### Security

| Task | Effort | Dependencies |
|------|--------|-------------|
| **Phase 1: Tailscale-Only** | 2–3 days | None |
| • Tailscale-aware listener binding | 1 day | — |
| • ACL documentation | 1 day | — |
| • Readiness check (`proxygit check`) | 0.5 day | — |
| **Phase 2: mTLS** | 7–10 days | (partial overlap with Phase 1) |
| • CA generation / CLI tooling | 1 day | — |
| • Rustls client-auth config on QUIC | 2 days | — |
| • WebDAV HTTPS upgrade + client cert | 2 days | Rustls stack |
| • Role mapper (acl.toml) | 1 day | — |
| • Integration tests | 1–2 days | All above |
| • Cert rotation tooling | 1 day | — |
| **Phase 3: Bearer Tokens** | 3–5 days | Phase 2 mTLS |
| • Token storage + hashing | 1 day | — |
| • WebDAV Bearer header parsing | 0.5 day | — |
| • QUIC MSG_AUTH frame | 1 day | — |
| • CLI: `proxygit token` subcommand | 1 day | — |
| • Test CI token workflow | 0.5 day | — |

### Total Estimate

| Area | Immediate (weeks 1–2) | Short-term (month 1) | Medium-term (month 2–3) |
|------|----------------------|---------------------|------------------------|
| **Storage** | 5 days (pack files) | +3 days (compression) | +7–10 days (LMDB eval) |
| **Security** | 2–3 days (Tailscale) | +7–10 days (mTLS) | +3–5 days (tokens) |

**Parallel tracks**: Storage and security are largely independent and can proceed in parallel after initial design alignment.

---

## 4. References

### Storage

| Resource | URL |
|----------|-----|
| Garage benchmarks & design | [garagehq.deuxfleurs.fr](https://garagehq.deuxfleurs.fr/documentation/design/benchmarks/) |
| SeaweedFS — architecture & design | [github.com/seaweedfs](https://github.com/seaweedfs/seaweedfs) |
| SeaweedFS — small file optimization | [seaweedfs.com](https://seaweedfs.com/) |
| Restic pack format documentation | [restic.readthedocs.io](https://restic.readthedocs.io/en/latest/100_references.html) |
| Restic chunker (CDC library) | [github.com/restic/chunker](https://github.com/restic/chunker) |
| LMDB documentation | [lmdb.tech/doc](http://www.lmdb.tech/doc/) |
| RocksDB — embedded KVS | [github.com/facebook/rocksdb](https://github.com/facebook/rocksdb) |
| Zstd — compression algorithm | [facebook.github.io/zstd](https://facebook.github.io/zstd/) |
| SQLite as an application file format | [sqlite.org/appfileformat.html](https://sqlite.org/appfileformat.html) |
| LMDB vs LevelDB vs RocksDB benchmarks | [lmdbjava/benchmarks](https://github.com/lmdbjava/benchmarks) |

### Security

| Resource | URL |
|----------|-----|
| Tailscale ACL reference | [tailscale.com/kb/1018/acls](https://tailscale.com/kb/1018/acls) |
| Tailscale grants (new policy syntax) | [tailscale.com/docs/features/access-control/grants](https://tailscale.com/docs/features/access-control/grants) |
| Syncthing security principles | [docs.syncthing.net/security](https://docs.syncthing.net/users/security.html) |
| mTLS — Cloudflare guide | [cloudflare.com](https://www.cloudflare.com/learning/access-management/what-is-mutual-tls/) |
| mTLS — complete guide | [blog.gitguardian.com](https://blog.gitguardian.com/mutual-tls-mtls-authentication/) |
| Garage — S3 API auth | [garagehq.deuxfleurs.fr](https://garagehq.deuxfleurs.fr/documentation/connect/cli/) |
| Rustls client-auth documentation | [github.com/rustls/rustls](https://github.com/rustls/rustls) |
| QUIC TLS 1.3 details | [RFC 9001](https://www.rfc-editor.org/rfc/rfc9001) |

### Rust Ecosystem (relevant crates)

| Crate | Purpose | Notes |
|-------|---------|-------|
| `quinn` | QUIC transport | Already used |
| `rustls` | TLS | Already used (v0.23) |
| `tokio-uring` | io_uring async I/O | Linux only, alpha |
| `compio` | Cross-platform async I/O | More mature, Windows+Linux |
| `heed` | LMDB Rust bindings | Type-safe, zero-copy |
| `rust-rocksdb` | RocksDB Rust bindings | C++ bridge, mature |
| `zstd` (or `zstd-sys`) | Zstd compression | Safe bindings, well-maintained |
| `rusqlite` | SQLite | Already used |

---

## Appendix: Decision Matrix

### Storage Backend Decision

| Criterion | Current FS | SQLite | LMDB | Pack Files | RocksDB | Garage S3 |
|-----------|-----------|--------|------|-----------|---------|-----------|
| **Simplicity** | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐ | ⭐⭐ |
| **Write perf** | ⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐ |
| **Read perf** | ⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐ |
| **Inode pressure** | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Random access** | ✅ | ✅ | ✅ | ✅ (via index) | ✅ | ✅ |
| **Built-in caching** | ❌ | ✅ (page cache) | ✅ (mmap) | ❌ | ✅ (block cache) | ❌ |
| **GC complexity** | Low | Medium | Auto | Medium | Auto | Low |
| **External deps** | None | None (bundled) | C lib | None | C++ (bindgen) | Running service |

### Auth Model Decision

| Criterion | Tailscale | mTLS | Bearer Tokens | SSH Keys |
|-----------|-----------|------|--------------|----------|
| **Setup complexity** | ⭐⭐⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐ |
| **Off-net support** | ❌ (needs Tailscale) | ✅ | ✅ | ✅ |
| **Per-project scoping** | Via ACLs | Via cert OU/SAN | ✅ | ❌ |
| **Revocation** | Tailscale admin | CRL/OCSP | Token DB | Key file |
| **Transport impact** | None (WireGuard) | At TLS handshake | Per-request header | Custom handshake |
| **CI/CD compatible** | ❌ (no Tailscale) | ✅ | ✅ | ❌ |

---

*End of research report.*
