# ProxyGit — Embeddings & Semantic Search Research

> **Compiled:** July 2026
> **Scope:** Adding semantic search over project files in ProxyGit (distributed content-addressed VFS)

---

## Table of Contents

1. [Context & Constraints](#1-context--constraints)
2. [Embedding Model Options](#2-embedding-model-options)
3. [Vector Store Options](#3-vector-store-options)
4. [Integration Point & Storage Location](#4-integration-point--storage-location)
5. [Incremental Indexing Strategy](#5-incremental-indexing-strategy)
6. [Recommendation](#6-recommendation)
7. [Implementation Plan & Effort Estimates](#7-implementation-plan--effort-estimates)
8. [References](#8-references)

---

## 1. Context & Constraints

ProxyGit is a **Rust** workspace (3 crates: `proxygit-common`, `proxygit-client`, `proxygit-server`) with:

| Technology | Detail |
|------------|--------|
| Language | Rust (Cargo workspace, edition 2021) |
| Database | SQLite per project via `rusqlite 0.31` (bundled) |
| Block Store | Content-addressed, FastCDC-chunked, BLAKE3-hashed |
| Agent Interface | MCP server on `localhost:8082` |
| Sync Daemon | Python 3 (`scripts/proxygit-sync-daemon.py`) |
| WebDAV | Built into server |

**Key constraint:** The project compiles on both macOS (development) and Linux (server). Semantic search must work in both environments with minimal external dependencies.

---

## 2. Embedding Model Options

### 2.1 OpenAI `text-embedding-3-small`

| Property | Value |
|----------|-------|
| Dimensions | 1,536 (configurable down to 256 via `dimensions` param) |
| Pricing | $0.02 / 1M tokens (~1M pages/day for $0.02) |
| MTEB Score | 62.3 |
| Latency | ~100–300ms per API call (network-bound) |
| Dependency | `openai` Python crate or Rust `reqwest` + API key |
| Privacy | Data leaves the host |
| Pros | Best quality, cheap, tiny model overhead |
| Cons | Requires internet, API key management, recurring cost, latency |

**Verdict:** Good for production deployments where latency is acceptable and a few dollars/month is fine.

### 2.2 OpenAI `text-embedding-3-large`

| Property | Value |
|----------|-------|
| Dimensions | 3,072 (configurable) |
| Pricing | $0.13 / 1M tokens |
| MTEB Score | 64.6 |
| Verdict | Not worth the extra cost for code search — `text-embedding-3-small` already outperforms BERT-based models. |

### 2.3 Local: `all-MiniLM-L6-v2` via `sentence-transformers`

| Property | Value |
|----------|-------|
| Dimensions | 384 |
| Model Size | ~75 MB (on disk) |
| MTEB Score | 56.3 |
| Latency | ~10–30ms per text on modern CPU (M-series, modern x86) |
| Dependency | Python `sentence-transformers`, `torch` (~800 MB CUDA-less, ~2 GB with CUDA) |
| Privacy | Fully local, no data leaves |
| Pros | Free, private, low latency once loaded, works offline |
| Cons | Heavy Python ML stack (PyTorch), memory ~500 MB loaded |

**Verdict:** Best quality-to-cost ratio for local-only setups. The Python dependency is a poor fit for a Rust codebase but could be wrapped as a sidecar process or embedded via `ort` (ONNX Runtime).

### 2.4 Local: All-minilm via ONNX / `ort` crate (Rust-native)

| Property | Value |
|----------|-------|
| Dimensions | 384 |
| Model Size | ~75 MB (ONNX export) |
| Inference | `ort` crate (ONNX Runtime for Rust) |
| Dependency | `ort = "2.0"` + ONNX model file |
| Privacy | Fully local |
| Pros | Pure Rust, no Python, small binary footprint, fast inference |
| Cons | `ort` crate adds ~20 MB to binary, model auto-download on first run |
| Status | ✅ Proven path — `fastembed` (Qdrant) uses this approach in Rust |

**Verdict:** Best fit for ProxyGit's Rust stack — no Python dependency, no network calls, fast CPU inference.

### 2.5 Anthropic Embeddings

| Property | Value |
|----------|-------|
| Status | ⚠️ Anthropic does not currently offer a dedicated embeddings API. Their models are chat/completion only. Not an option. |

### 2.6 Other Local Models (via ONNX)

| Model | Dims | Quality | Size | Notes |
|-------|------|---------|------|-------|
| `BAAI/bge-small-en-v1.5` | 384 | Slightly better than MiniLM | 33 MB | Good for code |
| `intfloat/e5-small-v2` | 384 | Competitive | ~50 MB | Good for retrieval |
| `sentence-transformers/msmarco-distilbert-base-v4` | 768 | Better quality | ~260 MB | Too heavy for edge |

### 2.7 Model Comparison Summary

| Model | Dims | Quality | Latency | Cost | Privacy | Rust-native |
|-------|------|---------|---------|------|---------|-------------|
| OpenAI text-embedding-3-small | 1,536 | Best | 100–300ms | $0.02/1M tokens | ❌ | ❌ (HTTP) |
| OpenAI text-embedding-3-large | 3,072 | Best | 200–500ms | $0.13/1M tokens | ❌ | ❌ (HTTP) |
| all-MiniLM-L6-v2 (Python) | 384 | Good | 10–30ms | Free | ✅ | ❌ |
| all-MiniLM-L6-v2 (ONNX/Rust) | 384 | Good | 5–20ms | Free | ✅ | ✅ |
| BAAI/bge-small-en-v1.5 (ONNX) | 384 | Better | 5–30ms | Free | ✅ | ✅ |
| Anthropic | — | N/A | — | — | — | No API exists |

---

## 3. Vector Store Options

### 3.1 sqlite-vec — 🥇 **Recommended**

| Property | Value |
|----------|-------|
| Type | SQLite loadable extension (C, no deps) |
| Rust Crate | `sqlite-vec 0.1.9` on crates.io |
| Depends on | `rusqlite ^0.31.0` ✅ (exact match with ProxyGit) |
| Dimensions | Up to 8,000 |
| Index type | Brute-force (exact) KNN + IVF approximation |
| Storage | Within same SQLite DB or separate |
| Query | SQL: `SELECT rowid, distance FROM vec_items ORDER BY vector_distance(vector, ?) LIMIT ?` |
| Pip install | `pip install sqlite-vec` |
| License | MIT / Apache 2.0 |
| Stars | 7.9k ⭐ |

**Pros:**
- Zero infrastructure — loads as SQLite extension (`SELECT load_extension(...)`)
- Reuses existing SQLite connection in ProxyGit's indexer
- Same transactional guarantees as the project index
- Rust bindings available and compatible with current `rusqlite` version
- Written in C, no extra runtime deps
- Can store vectors in the **same SQLite database** alongside project metadata
- Backup-in-a-file (same as SQLite)

**Cons:**
- Brute-force search is O(n) for exact KNN (fine for <100K files)
- IVF approximate index can mitigate for larger datasets
- No built-in full-text + vector hybrid search (but SQLite FTS5 can be added)

### 3.2 ChromaDB

| Property | Value |
|----------|-------|
| Type | Client-server vector database |
| Install | `pip install chromadb` — ~200 MB with deps |
| API | Python gRPC / HTTP client |
| Rust | No official Rust client (community wrappers only) |
| Storage | Persistent on disk (SQLite + Parquet) |
| Query | `collection.query(query_texts=["..."], n_results=10)` |
| Stars | 27k ⭐ |

**Pros:**
- Rich feature set (metadata filter, full-text, hybrid search)
- Production-proven at scale
- Good developer experience

**Cons:**
- Must run as a separate server process (~200 MB RAM)
- No Rust client — would need Python sidecar or HTTP REST calls
- Overkill for ProxyGit's scale (single-machine, <100K files)
- Adds deployment complexity (health checks, restarts, port conflicts)

### 3.3 LanceDB

| Property | Value |
|----------|-------|
| Type | Embedded columnar vector database |
| Install | `pip install lancedb` or Rust crate `lancedb` |
| Rust API | Yes — `lancedb = "0.12"` on crates.io |
| Storage | Lance columnar format (directory) |
| Query | `table.search(query_vector).limit(10).to_df()` |

**Pros:**
- Embedded (no server) — good fit for ProxyGit's architecture
- Fast vector search with disk-based IVF PQ index
- Rust-native
- Columnar format good for analytics too

**Cons:**
- Newer / less mature than ChromaDB
- Adds ~40 MB of Rust dependencies (Arrow + Lance)
- Separate file format from SQLite — can't reuse existing DB
- Not as battle-tested for small-scale workloads

### 3.4 Plain NumPy + Cosine Similarity

| Property | Value |
|----------|-------|
| Type | In-memory computation |
| Install | `numpy` via pip or if not Rust: `ndarray` crate |
| Storage | `.npy` file or raw BLOBs |

**Pros:**
- Dead simple, no infra
- Good for <10K vectors
- Full control over search logic

**Cons:**
- Everything in RAM — doesn't scale
- No persistence logic built in
- No metadata filtering
- Have to reimplement dedup, update, delete

**Verdict:** Fine for prototyping, not production.

### 3.5 pgvector (PostgreSQL)

Not suitable — ProxyGit has no PostgreSQL dependency and adding one would be architectural overkill for a VFS project.

### 3.6 Vector Store Comparison Summary

| Store | Rust-native | Server? | Persistence | Search | Dep overhead | Best for |
|-------|-------------|---------|-------------|--------|-------------|----------|
| **sqlite-vec** | ✅ (FFI) | No | SQLite file | Exact KNN + IVF | Minimal (C ext) | ✅ ProxyGit |
| ChromaDB | ❌ | Yes | On disk | HNSW | ~200 MB + server | Multi-user |
| LanceDB | ✅ (native) | No | Lance dir | IVF PQ | ~40 MB deps | Large-scale |
| NumPy | ❌ (Python) | No | File | Brute-force | numpy only | Prototyping |
| pgvector | ❌ | Yes | PG | IVFFlat/HNSW | Full PG | Already on PG |

---

## 4. Integration Point & Storage Location

### 4.1 MCP Tool (Primary Interface)

Add a **new MCP tool** to the existing proxygit-server MCP interface:

```json
{
  "name": "search_files",
  "description": "Semantic search over project file contents",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query": {"type": "string", "description": "Natural language search query"},
      "project": {"type": "string", "description": "project UUID"},
      "limit": {"type": "number", "description": "max results (default: 10)"},
      "path_glob": {"type": "string", "description": "optional filter: only search *.py files"}
    },
    "required": ["query", "project"]
  }
}
```

### 4.2 CLI Command

Add a `proxygit-client search <project-uuid> <query>` verb for terminal use that delegates to the server's existing QUIC protocol.

### 4.3 Auto-Indexing

- **On-demand:** Embedding is triggered lazily when a `search` call is first made
- **On-sync:** The sync daemon updates embeddings for changed files
- **On-write:** Server re-embeds files after `WRITE_BLOCKS` completes

**Recommendation:** Hybrid — lazy embedding on first search call + incremental updates during sync.

### 4.4 Storage Location

**Option A: In the per-project SQLite database** (alongside file index) ← **RECOMMENDED**

```
# Extend existing project.sqlite with:
CREATE TABLE vec_embeddings (
    file_path TEXT PRIMARY KEY,
    file_hash TEXT NOT NULL,         -- BLAKE3 hash, cache invalidation
    embedding BLOB NOT NULL,          -- float32 bytes (384 dims × 4 bytes = 1536 bytes)
    file_size INTEGER,
    mtime INTEGER,
    indexed_at INTEGER NOT NULL
);

-- FTS5 table for filename/content search (optional, pairs with vector)
CREATE VIRTUAL TABLE vec_fts USING fts5(
    file_path, content,
    tokenize='porter unicode61'
);
```

**Option B: Separate `.proxygit-embeddings.sqlite` file**

**Option C: Flat `.npz` / `.lance` directory alongside `data/`**

| Option | Pros | Cons |
|--------|------|------|
| **A** (same SQLite) | ✅ Transactional with index, same backup, atomic updates | DB grows larger; vacuum cost increases |
| **B** (separate SQLite) | Isolates backup/GC | Two connections, sync complexity |
| **C** (separate format) | Optimized for vectors | Extra code, can't reuse SQLite txns |

**Recommendation: Option A** — simplicity wins. A per-project SQLite DB with ~10K files adds ~15 MB for embeddings (384 floats × 4 bytes × 10K files = ~15 MB raw, plus index overhead). Negligible next to code blocks.

---

## 5. Incremental Indexing Strategy

### 5.1 Algorithm

```
on_search(query):
    if !index_exists(project):
        build_index(project)  ← full scan, ~100ms/file
    results = query_vectors(query)
    return results

on_file_change(project, path, hash):
    upsert vec_embeddings SET file_hash=new_hash, embedding=compute_embedding(content)
    
on_sync_complete(project):
    # Find and update changed files from the sync pass
    for changed_path in sync_diff:
        update_embedding(project, changed_path)
```

### 5.2 Cache Invalidation

- Store `file_hash` (BLAKE3) alongside each embedding
- Before search, compare file hashes → only re-embed changed files
- Or simpler: re-embed on write, trust the sync daemon

### 5.3 Embedding Granularity

| Strategy | Chunks per file | Embeddings | Quality | Storage |
|----------|----------------|------------|---------|---------|
| **Whole-file** | 1 | 1 embedding | Good for small files | Minimal |
| **Chunked** (by line/paragraph) | ~10–50 | Multiple | Better for large files | ~10× more |
| **Whole-file + chunked** | Hybrid | Both | Best | ~11× more |

**Recommendation:** Start with **whole-file** embeddings (one vector per file). It's fast, simple, and works well for ≤100K files. The embedding of a file is the embedding of its content string (source code or document). For ProxyGit's use case — finding "which file talks about X" — whole-file is sufficient. Add chunk-level search in Phase 2 if needed.

### 5.4 Handling Large Files

- Truncate content to ~8,000 tokens before embedding (context window for all models)
- For files >100K tokens, chunk into 8K-token segments and average embeddings
- Binary files: skip (embed filename only, or use file extension heuristic)

---

## 6. Recommendation

### 🏆 Recommended Stack

| Layer | Choice | Why |
|-------|--------|-----|
| **Embedding Model** | `BAAI/bge-small-en-v1.5` via ONNX Runtime (`ort` crate) | Best quality-to-size ratio, Rust-native, free, private, 384 dims |
| **Vector Store** | `sqlite-vec` | Zero infra, reuses existing SQLite DB, compatible with current `rusqlite` 0.31, OSS |
| **Integration Point** | MCP tool `search_files` + `proxygit-client search` CLI | Follows existing patterns in SPEC.md |
| **Storage** | Same per-project SQLite DB (new tables) | Simplest, transactional, same backup |
| **Indexing Strategy** | Hybrid: lazy build on first search + incremental updates on write/sync | No wasted work for projects that don't need search |

### Why This Stack Wins

1. **Zero new infrastructure** — sqlite-vec loads as a SQLite extension (`.so`/`.dylib` on disk). No server, no daemon, no container.
2. **Rust-native** — `ort` for ONNX inference keeps everything in one language. The sync daemon (Python) stays in Python.
3. **Progressive** — Ships as an MCP tool immediately. CLI + auto-index can follow in the same PR.
4. **Incremental by design** — Embeddings are cheap to compute per file, and BLAKE3 hash comparison makes re-indexing a no-op for unchanged files.
5. **Battle-tested components** — sqlite-vec is a Mozilla Builders project (7.9k ⭐); `ort` is the official ONNX Runtime Rust binding.

### Fallback Options

- **Budget/offline constraint:** `all-MiniLM-L6-v2` ONNX model instead of `bge-small-en-v1.5` (slightly lower quality, smaller model)
- **No ONNX:** Use OpenAI API via HTTP — swap model, everything else stays the same
- **No Rust ML stack:** Run a Python sidecar using `sentence-transformers` + `sqlite-vec` (Python). Trade simplicity for faster time-to-ship.

---

## 7. Implementation Plan & Effort Estimates

### Phase 1: Core Embedding Engine (Rust crate, ~2 days)

| Task | Est. | Files |
|------|------|-------|
| Add `ort` + `tokenizers` deps to `proxygit-server` | 1h | `crates/proxygit-server/Cargo.toml` |
| Add model download logic (auto-fetch ONNX from HuggingFace on first run) | 2h | `crates/proxygit-server/src/embeddings/model.rs` |
| Implement `embed_text(text: &str) -> Vec<f32>` | 2h | `crates/proxygit-server/src/embeddings/mod.rs` |
| Implement `embed_file(path: &str, content: &str) -> (Vec<f32>, String)` | 1h | `crates/proxygit-server/src/embeddings/mod.rs` |
| Integration test: embed a Rust file, verify 384-d vector | 1h | `crates/proxygit-server/tests/embedding_test.rs` |
| **Total** | **~7h** | |

### Phase 2: Vector Storage via sqlite-vec (~1.5 days)

| Task | Est. | Files |
|------|------|-------|
| Add `sqlite-vec` crate dep + `.so`/`.dylib` loading | 1h | `Cargo.toml` (workspace) + extension init code |
| Create `vec_embeddings` table + `vec_fts` table | 1h | `indexer/mod.rs` (or new `vector_index.rs`) |
| Implement `upsert_embedding(project_id, path, hash, vec)` | 2h | `vector_index.rs` |
| Implement `search_similar(project_id, query, k) -> Vec<(path, score)>` | 3h | `vector_index.rs` |
| Implement full index rebuild for a project | 2h | `vector_index.rs` |
| Migration for existing SQLite DBs | 1h | `indexer/mod.rs` |
| **Total** | **~10h** | |

### Phase 3: Integration (~1 day)

| Task | Est. | Files |
|------|------|-------|
| MCP `search_files` tool in server | 3h | `server/lib.rs` (MCP handler) |
| `proxygit-client search` CLI verb | 2h | `client/lib.rs` + `client/src/main.rs` |
| QUIC message type 0x0E for `SEARCH_FILES` | 1h | `common/protocol.rs` |
| On-write embedding update in server block store handler | 2h | `server/lib.rs` |
| **Total** | **~8h** | |

### Phase 4: Sync Daemon Integration (~4h, Python)

| Task | Est. | Files |
|------|------|-------|
| Sync daemon calls re-index endpoint after sync pass | 2h | `scripts/proxygit-sync-daemon.py` |
| Debounce rapid file changes during bulk sync | 1h | `scripts/proxygit-sync-daemon.py` |
| Test: semantic search after bidirectional sync | 1h | Manual |
| **Total** | **~4h** | |

### Total Effort

| Phase | Hours | Days |
|-------|-------|------|
| P1: Embedding Engine | 7 | ~1 |
| P2: Vector Storage | 10 | ~1.5 |
| P3: Integration | 8 | ~1 |
| P4: Sync Daemon | 4 | ~0.5 |
| **Total** | **~29h** | **~4 days** |

### Effort by Approach

| Approach | Embedding | Vector Store | Integration | Total | Notes |
|----------|-----------|-------------|-------------|-------|-------|
| **Rust + ONNX + sqlite-vec** | 7h | 10h | 12h | **~29h** | Recommended — pure Rust |
| Python sidecar + sqlite-vec | 3h | 10h | 16h | ~29h | Faster start, more glue code |
| OpenAI API + sqlite-vec | 2h | 10h | 12h | ~24h | Fastest, but recurring cost + latency |
| ChromaDB + OpenAI | 2h | 4h | 16h | ~22h | Cheapest to build, highest ops cost |
| LanceDB + ONNX (Rust) | 7h | 8h | 12h | ~27h | Comparable to recommended, less battle-tested |

---

## 8. References

- [sqlite-vec GitHub](https://github.com/asg017/sqlite-vec) — MIT/Apache 2.0, 7.9k ⭐
- [sqlite-vec Rust crate (crates.io)](https://crates.io/crates/sqlite-vec) — v0.1.9, rusqlite 0.31
- [all-MiniLM-L6-v2 on HuggingFace](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2) — 384-d, 75 MB
- [BAAI/bge-small-en-v1.5 on HuggingFace](https://huggingface.co/BAAI/bge-small-en-v1.5) — 384-d, 33 MB, slightly better quality
- [ort crate (ONNX Runtime for Rust)](https://crates.io/crates/ort) — official Rust binding for ONNX Runtime
- [OpenAI Embeddings API](https://platform.openai.com/docs/guides/embeddings) — $0.02/1M tokens for text-embedding-3-small
- [ProxyGit SPEC.md](./SPEC.md) — Existing MCP tool schema, QUIC protocol, architecture
- [ProxyGit AGENTS.md](./AGENTS.md) — Agentic workflow and repo structure

---

*End of research report. See [ARCHITECTURE-ROADMAP.md](./ARCHITECTURE-ROADMAP.md) for priority scheduling.*
