# ProxyGit — Embeddings Design Review

> **Review Date:** July 29, 2026  
> **Documents Reviewed:** `EMBEDDINGS-RESEARCH.md` (19 KB), `EMBEDDINGS-DESIGN.md` (45 KB)  
> **External Research:** LiquidAI LFM2.5-Embedding-350M, LFM2.5-ColBERT-350M, LFM2.5-Encoder models  
> **Codebase References:** `crates/proxygit-common/src/protocol.rs`, `crates/proxygit-server/src/lib.rs`, `AGENTS.md`

---

## 1. Is the sqlite-vec + ONNX Runtime Approach Sound?

### Verdict: Yes, with important caveats

**What's right:**

- sqlite-vec is an excellent fit for ProxyGit's architecture. It loads as a SQLite C extension, works with the existing `rusqlite 0.31` dependency, requires zero infrastructure, and stores vectors in the same per-project SQLite DB that already holds the file index. This is a clean, minimal-dependency choice.
- The `ort` crate (ONNX Runtime for Rust) is the correct way to run ONNX models in Rust without Python. BGE-small-en-v1.5 at 33 MB and 384-d embeddings is a reasonable default.
- Brute-force exact KNN (which sqlite-vec defaults to) is fine for ≤100K vectors — the latency budget of <500ms is easily met.
- L2 normalization of embeddings + cosine distance is the standard pairing for BGE-family models.

**Issues:**

1. **sqlite-vec + `unsafe` extension registration** — The design doc shows (`EMBEDDINGS-DESIGN.md` lines 493–496):
   ```rust
   unsafe {
       sqlite3_auto_extension(Some(sqlite3_vec_init));
   }
   ```
   This registers the extension globally for all future connections. This is the documented approach, but it means every SQLite connection in the process (including non-vector connections) loads the vector extension. For a long-running server, this is fine, but it should be noted as a design constraint.

2. **sqlite-vec virtual table rowid mapping** — The design uses `vec0` virtual tables (`vec_file_index`, `vec_chunk_index`) but the rowid-to-metadata mapping is not fully specified. The `vec_embeddings` table uses `file_path TEXT PRIMARY KEY`, while `vec0` virtual tables have integer rowids. The design mentions "join on rowid ↔ file_path mapping via a side table" (`EMBEDDINGS-DESIGN.md` line 479) but doesn't define this side table. This is an underspecified detail that will surface during implementation.

3. **sqlite-vec version stability** — As of July 2026, sqlite-vec v0.1.9 (the version in the design) is still pre-1.0. The API may change. A minor concern for a private project, but worth pinning and testing after upgrades.

4. **ORT session thread-safety** — The `EmbeddingModel` stores a single `Session` that is shared across queries. ONNX Runtime sessions are technically `Send + Sync` but concurrent calls need `Session::run` to be exclusive (the underlying ORT API is not fully thread-safe for all execution providers). The design needs either an `Arc<Mutex<Session>>` or a session pool to avoid data races under concurrent search.

### LiquidAI Model Compatibility

The design documents only consider BGE-small-en-v1.5 (384-d) and all-MiniLM-L6-v2 (384-d). The LiquidAI models introduce significant changes:

| Model | Type | Dimensions | Format | ONNX Available? |
|-------|------|-----------|--------|----------------|
| **LFM2.5-Embedding-350M** | Dense bi-encoder | **1024** | PyTorch, GGUF | ❌ Manual export needed |
| **LFM2.5-ColBERT-350M** | ColBERT late-interaction | Per-token | PyTorch, GGUF | ❌ Manual export needed |
| **LFM2.5-Encoder-350M** | General encoder (not retrieval) | Hidden states | PyTorch | ❌ Manual export needed |
| BGE-small-en-v1.5 | Dense bi-encoder | 384 | ONNX ✅ | ✅ Official ONNX export |

**Critical finding:** The LiquidAI models are **not available as ONNX exports** on HuggingFace. They ship as:
- PyTorch / `sentence-transformers` format (requires Python or `tch-rs` crate)
- GGUF format (requires `llama.cpp` bindings, not ONNX Runtime)

Running them via the `ort` crate would require manually exporting each model to ONNX (a non-trivial process that requires Python, PyTorch, `torch.onnx.export`, and handling of custom ops for the LFM2 architecture's multiplicative gates and short convolutions).

**If the LiquidAI models are desired, the architecture must support non-ONNX backends** — either via:
- A GGUF inference path (e.g., `llama-cpp-2` Rust crate or calling `llama.cpp` as a subprocess)
- A Python sidecar using `sentence-transformers`
- Manual ONNX export of the LFM2.5-Embedding-350M (unknown difficulty due to custom architecture ops)

---

## 2. Does the Current Design Support Pluggable Backends Well?

### Verdict: No — pluggability is missing entirely

**What exists:**
The design hardcodes a single approach: `BAAI/bge-small-en-v1.5` via `ort` (ONNX Runtime) with 384-d vectors and a specific `EmbeddingModel` struct. There is no abstraction layer for backend selection.

**What's missing:**

1. **No `EmbeddingBackend` trait** — The design should define a Rust trait:
   ```rust
   pub trait EmbeddingBackend: Send + Sync {
       fn embed(&self, text: &str) -> Result<Vec<f32>>;
       fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
       fn dimensions(&self) -> usize;
       fn name(&self) -> &str;
   }
   ```
   Without this, adding a new model (OpenAI, LiquidAI, etc.) requires changing the server core.

2. **No model configuration in `.proxygit` manifest** — The `.proxygit` file format (`AGENTS.md` lines 91–105) has no `[embeddings]` section. A pluggable design needs per-project model selection:
   ```toml
   [embeddings]
   model = "bge-small-en-v1.5"       # or "openai-text-embedding-3-small", "lfm2.5-embedding-350m"
   backend = "onnx"                   # or "openai", "gguf"
   api_key = "${OPENAI_API_KEY}"      # for API backends
   dimensions = 384                   # override (OpenAI supports 256-1536)
   ```

3. **No dimension abstraction** — The SQL schema is hardcoded for 384-d (`float[384]` in vec0 table definitions, `EMBEDDINGS-DESIGN.md` lines 476–483). Different models have different dimensions:
   - BGE-small-en-v1.5: 384
   - LFM2.5-Embedding-350M: 1024
   - OpenAI text-embedding-3-small: 1536 (configurable)
   
   The schema, blob widths, and vec0 table definitions all depend on dimensions. A pluggable design must either:
   - Use per-model schema (table per model variant), or
   - Normalize to a fixed max dimension (e.g., 1536) with padding.

4. **No fallback chain** — For robustness, the design should support a priority chain: try local ONNX first, fall back to OpenAI API if model file not found, etc.

**What the RESEARCH doc gets right:** The research doc mentions fallback options (`EMBEDDINGS-RESEARCH.md` lines 370–373):
> - **Budget/offline constraint:** `all-MiniLM-L6-v2` ONNX model instead of `bge-small-en-v1.5`
> - **No ONNX:** Use OpenAI API via HTTP
> - **No Rust ML stack:** Run a Python sidecar using `sentence-transformers` + `sqlite-vec` (Python)

But these fallbacks are not reflected in the DESIGN doc's architecture. The design commits to a single path without the abstraction layer needed to support them.

---

## 3. What's Missing, Wrong, or Needs Clarification?

### 3.1 Critical: QUIC Message Type Collision

**`EMBEDDINGS-DESIGN.md`** proposes (`lines 717–719`):
```rust
pub const MSG_SEMANTIC_SEARCH: u8 = 0x0E;
pub const MSG_SEMANTIC_SEARCH_RESP: u8 = 0x0F;
```

**The existing code** (`crates/proxygit-common/src/protocol.rs` lines 19–20) already uses:
```rust
pub const MSG_GET_PROJECT_MAP: u8 = 0x0E;
pub const MSG_GET_PROJECT_MAP_RESP: u8 = 0x0F;
```

This is a direct collision. The next available message types are **0x10** and **0x11**. The design needs to be updated.

### 3.2 One-Phase vs Two-Phase vec0 Loading

The design shows (`EMBEDDINGS-DESIGN.md` lines 493–496) registering `sqlite-vec` via `sqlite3_auto_extension`. However, sqlite-vec is also loadable on-demand via `SELECT load_extension(...)` or by using the `sqlite_vec::sqlite3_vec_init` directly per-connection. For ProxyGit, where not all connections need vectors, on-demand loading per project connection is cleaner than global registration.

### 3.3 Content Assembly for Rebuild

The `rebuild_all_embeddings` function (`EMBEDDINGS-DESIGN.md` lines 564–606) reads file content via `get_file_blocks` + `block_store.read_blocks`. This reassembles file content from CDC chunks for every file during a full reindex. For 10K files, this means reading and reassembling potentially millions of blocks. The design doesn't account for:
- Block read amplification (one file → many blocks)
- Memory pressure from holding reassembled content
- Cancellation (a full rebuild could take 5+ minutes for 10K files)

**Mitigation:** Since embeddings are stored per-file with BLAKE3 hash as cache key, a full rebuild should only process files whose hash has changed. The design already does this (`line 580: "if existing.file_hash == entry.tree_hash { continue; }"`), which is correct.

### 3.4 Tree-Sitter Chunking Complexity

The design adds **3 new Rust crates** for chunking (`tree-sitter`, `tree-sitter-rust`, `tree-sitter-python`, `tree-sitter-typescript`, `tree-sitter-go`, `tree-sitter-javascript`) plus grammar parsers. This is significant scope for an MVP.

**Concerns:**
- Tree-sitter grammars are `.so`/`.dylib` files that must be built or bundled — adds deployment complexity
- The chunking code handles 5+ languages at launch, but ProxyGit's primary language is Rust
- Grammar loading at runtime adds ~50-100ms startup time per language
- The line-count fallback (50 lines, 10 overlap) works adequately for non-code files

**Recommendation:** Defer tree-sitter to Phase 2. Use whole-file embeddings + simple line-count chunking for the MVP. The whole-file embedding already answers "which file is about X" — function-level precision is a refinement.

### 3.5 Tree-Sitter Grammar Deployment

The design doesn't discuss how tree-sitter grammar files are deployed. On server-host (server side), the grammars need to be compiled `.so` files available at runtime. This is straightforward on the build machine but creates a dependency on host tooling (a C compiler + `tree-sitter` CLI). The deployment Dockerfile would need these build-time dependencies.

### 3.6 Mean-Pooling Implementation Error

The mean-pooling code in the design doc (`EMBEDDINGS-DESIGN.md` lines 207–219) has a bug:

```rust
let mask = attention_mask;
let mask_sum: f32 = mask.iter().sum::<f32>().max(1.0);
let pooled: Vec<f32> = embedding
    .rows().into_iter().next().unwrap()
    .iter().enumerate()
    .map(|(i, &v)| v * mask[i % len] as f32 / mask_sum)
    .collect();
```

This `enumerate().map()` iterates over the embedding dimensions (384), not over tokens. The attention mask should be applied per-token during mean pooling, not per-dimension. The correct BGE-family mean-pooling approach is:

```rust
// Token-wise mean pooling with attention mask
let token_embeddings: ArrayView2<f32> = outputs["last_hidden_state"]
    .try_extract_tensor::<f32>()?; // shape: (1, seq_len, dims)
let seq_len = token_embeddings.shape()[1];
let dims = token_embeddings.shape()[2];

let mut pooled = vec![0.0f32; dims];
let mut token_count = 0;

for t in 0..seq_len {
    if attention_mask[t] > 0 {
        for d in 0..dims {
            pooled[d] += token_embeddings[(0, t, d)];
        }
        token_count += 1;
    }
}

if token_count > 0 {
    for d in 0..dims {
        pooled[d] /= token_count as f32;
    }
}
```

The current code would produce incorrect embeddings, silently degrading search quality.

### 3.7 BGE Context Window vs. File Size Mismatch

`EMBEDDINGS-DESIGN.md` says (`line 255`) that the truncation threshold is "First 8,000 tokens" for whole-file, then says BGE's max context is 512 tokens. Later code uses `truncate_to_max_tokens(content, 512)` (line 301). This inconsistency needs resolution. The actual BGE-small context window is 512 tokens — anything beyond that is simply truncated.

For files >512 tokens, the design mentions mean-pooling multiple 512-token windows but doesn't implement it. This should be called out: BGE-small cannot encode a 10,000-token Rust file in one pass.

### 3.8 Hybrid Search Weighting with No Evaluation

The hybrid search (`EMBEDDINGS-DESIGN.md` lines 968–987) uses a fixed 70/30 ANN/FTS5 weighting:
```rust
* scores.entry(path).or_insert(0.0) += score * 0.7;
* scores.entry(path).or_insert(0.0) += score.min(1.0) * 0.3;
```

These weights are arbitrary and should be configurable or determined through evaluation. The FTS5 BM25 score range is [0, ∞) (it's not bounded like cosine similarity), so `score.min(1.0)` may distort results significantly. This needs clarification or a proper normalization step.

---

## 4. Is the ~25-30 Hour Effort Estimate Realistic?

### Verdict: Reasonable for the core path, optimistic with tree-sitter

The design doc estimates **~25.5 hours** (`EMBEDDINGS-DESIGN.md` line 956). The research doc estimates **~29h** (`EMBEDDINGS-RESEARCH.md` line 429). Both are in the same ballpark.

**Breakdown with reality check:**

| Phase | Estimated | Realistic | Notes |
|-------|-----------|-----------|-------|
| P1: Embedding Engine | 6.5h | 8h | Auto-download + ORT init + first-time setup always takes longer than estimated; cross-compilation for the server host (x86_64) from macOS (ARM) may add friction |
| P2: Vector Index | 9.5h | **14h** | The estimate includes tree-sitter + chunking + FTS5 + multi-language grammar support. Deferred chunking (drop tree-sitter for MVP) brings this to ~8h. |
| P3: API Integration | 5h | 4h | Standard QUIC/MCP pattern — well understood, fast to implement if familiar with existing code |
| P4: Write-Time Sync | 4.5h | 5h | Wiring into `handle_write_blocks` is straightforward; E2E tests always take longer |
| **Total** | **25.5h** | **31h** (full), **25h** (MVP without tree-sitter/chunking) |

**Risks that could inflate the estimate:**
- **ONNX Runtime build time** — `ort` crate compiles/builds ONNX Runtime as a native dependency. First build can take 10-15 minutes. CI setup may need a pre-built artifact cache.
- **sqlite-vec platform dylib** — The `.so`/`.dylib` needs to be available on the server host (x86_64). If the build machine is macOS ARM, cross-compilation or Docker build is needed.
- **Tree-sitter grammar build** — Each grammar needs a C compiler at build time. This adds complexity to the Dockerfile.
- **Model download integration** — HuggingFace downloads via the `ort` crate's built-in download (if any) or via a custom `reqwest` implementation. Token-based auth may be needed for gated models.

**Bottom line:** 25-30 hours is achievable for the core MVP (whole-file embeddings, no tree-sitter, 384-d BGE-small via ONNX). Full scope with tree-sitter chunking is ~35h.

---

## 5. Integration with Existing ProxyGit Patterns

### 5.1 MCP Tools ✅ — Well designed

The `semantic_search` MCP tool (`EMBEDDINGS-DESIGN.md` lines 640–676) follows the existing MCP pattern. The tool name `semantic_search` is consistent with the existing tool naming convention in `.proxygit` (which lists `read_file`, `write_file`, `list_directory`, etc.).

**Minor issue:** The `.proxygit` manifest (`AGENTS.md` line 101) has a hardcoded `tools` list:
```toml
tools = ["read_file", "write_file", "list_directory", "stat", "get_project_map"]
```

This list needs to include `"semantic_search"` after implementation. Consider making the tool list dynamic (server-advertised) rather than hardcoded in `.proxygit`.

### 5.2 QUIC Protocol ⚠️ — Message type collision (see 3.1)

The QUIC message pattern (new types, request-response pairing, payload format) follows existing patterns correctly. The payload structure (`[project_id: 16 bytes][query_len: 2 bytes][query: UTF-8][limit: 1 byte][mode: 1 byte]`) matches the existing frame layout in `crates/proxygit-common/src/protocol.rs`.

**Required fix:** Change message types from `0x0E`/`0x0F` to `0x10`/`0x11` to avoid collision with `MSG_GET_PROJECT_MAP`.

### 5.3 `.proxygit` Manifest ⚠️ — Missing embedding configuration

The `.proxygit` manifest (`AGENTS.md` lines 91–105) has no `[embeddings]` section. Per-project embedding model selection, dimensions, and API keys need a configuration path.

**Recommendation:** Add an optional `[embeddings]` section:
```toml
[embeddings]
enabled = true
model = "bge-small-en-v1.5"    # default
# model = "openai-text-embedding-3-small"  # alternative
# api_key = "${OPENAI_API_KEY}"
```

### 5.4 Bidirectional Sync ✅ — Well integrated

The write-time embedding update (`EMBEDDINGS-DESIGN.md` section 6) hooks into `handle_write_blocks` after content ingestion, following the same "inline update on mutation" pattern used for the file index. This is the correct integration point.

The sync daemon integration (`EMBEDDINGS-RESEARCH.md` lines 416–419) adds a re-index call after sync pass, which is the right approach for the Python sync daemon.

### 5.5 Server-Side Architecture ✅ — Sound

The decision to run embeddings on the server host (the server, `EMBEDDINGS-DESIGN.md` section 8) rather than on macOS is correct. The reasoning about data locality, index deduplication, and avoiding QUIC round-trips for file content is solid. The thin-client model (macOS forwards MCP calls to server) aligns with the existing architecture.

---

## 6. What's the Simplest MVP That Delivers Value?

### MVP Scope: ~20 hours, 3 engineer-days

Cut scope aggressively to deliver working semantic search:

#### What to build
1. **Whole-file embeddings only** (no chunking, no tree-sitter)
2. **BGE-small-en-v1.5 via ONNX Runtime** (384-d, 33 MB, proven path)
3. **`vec_embeddings` table + `vec0` ANN index** (per-project SQLite)
4. **MCP `semantic_search` tool** (file-level only, hybrid mode)
5. **QUIC message types 0x10/0x11** (avoiding protocol collision)
6. **Write-time embedding on `WRITE_BLOCKS`** (inline update)
7. **`proxygit-client search` CLI** (basic)
8. **FTS5 `vec_fts` table for keyword-only fallback** (no hybrid fusion — keep it simple)

#### What to defer
| Feature | Defer To | Rationale |
|---------|----------|-----------|
| Tree-sitter chunking | Phase 2 | 3+ crates, grammar build, marginal benefit for MVP |
| ColBERT / multi-vector search | Phase 2 | Requires different search architecture |
| Hybrid ANN+FTS5 reranking | Phase 2 | No evaluation data for weight tuning |
| LiquidAI model support | Phase 2 | Requires GGUF/ONNX bridge, 1024-d schema change |
| OpenAI API backend | Phase 2 | Needs `EmbeddingBackend` trait + API key management |
| Rebuild progress streaming | Phase 2 | Nice-to-have for large projects |
| Binary file name embedding | Phase 2 | Trivial but unnecessary for MVP |
| `.proxygit` embedding config | Phase 2 | Hardcode model for MVP |
| Streaming full-rebuild progress | Phase 2 | SSE/WebSocket, nice-to-have |

#### Why this MVP works
- **Works immediately** for the core use case: "find the Rust file about X"
- **No dependency on tree-sitter** — avoids grammar build issues on the server host
- **BGE-small ONNX is the simplest path** — `ort` crate + model file, proven in production
- **384-d vectors** fit sqlite-vec well without dimension abstraction complexity
- **Write-time sync** means search results are never stale
- **MCP tool fits the existing agent pattern** — Hermes agents can call it immediately

#### What the MVP cannot do
- Cannot find specific functions within a file — only "which file is about X"
- Cannot search across line-number granularity
- Cannot use user's natural language terms that are not semantically similar to code

**This is acceptable** — the whole-file approach answers the most common query pattern: "Where is the code that handles X?"

---

## Summary of Required Fixes Before Implementation

| Priority | Issue | Location | Fix |
|----------|-------|----------|-----|
| 🔴 **Blocking** | QUIC msg type collision 0x0E/0x0F | DESIGN.md §7.3 | Change to 0x10/0x11 |
| 🟡 **High** | Mean-pooling implementation bug | DESIGN.md §3.3 | Rewrite token-wise mean pooling |
| 🟡 **High** | ORT session thread safety | DESIGN.md §3.3 | Add `Arc<Mutex<Session>>` or pool |
| 🟡 **High** | BGE context window (512 vs 8000) | DESIGN.md §4.1 | Clarify, cap at 512 tokens |
| 🟡 **Medium** | No `EmbeddingBackend` trait | DESIGN.md §3 | Define trait for pluggability |
| 🟡 **Medium** | Schema hardcoded to 384-d | DESIGN.md §5 | Abstract dimensions in schema |
| 🟡 **Medium** | No `.proxygit` embedding config | AGENTS.md | Add `[embeddings]` section |
| 🟢 **Low** | FTS5 score normalization in hybrid | DESIGN.md §11.1 | Normalize BM25 to [0,1] before weighting |
| 🟢 **Low** | sqlite-vec side-table undefined | DESIGN.md §5 | Specify rowid↔path mapping |
| 🟢 **Low** | Tree-sitter grammar deployment | DESIGN.md §4.3 | Document in Dockerfile |
| 🟢 **Low** | No model download SHA256 verification | DESIGN.md §3 | Add integrity check |

---

## Notes on LiquidAI Models

The user asked to incorporate **LiquidAI LFM2.5-Encoder and LFM2 ColBERT** models, which run on CPU. Research findings:

### Current Model Inventory (July 2026)

| HuggingFace ID | Status | Params | Dims | Format | Type |
|---------------|--------|--------|------|--------|------|
| `LiquidAI/LFM2.5-Embedding-350M` | ✅ Current | 354M | **1024** | PyTorch, GGUF | Dense bi-encoder (retrieval) |
| `LiquidAI/LFM2.5-ColBERT-350M` | ✅ Current | 354M | Per-token | PyTorch, GGUF | ColBERT late-interaction |
| `LiquidAI/LFM2.5-Encoder-350M` | ✅ Current | 354M | Hidden states | PyTorch | General bidirectional encoder |
| `LiquidAI/LFM2.5-Encoder-230M` | ✅ Current | 230M | Hidden states | PyTorch | General bidirectional encoder |
| `LiquidAI/LFM2-ColBERT-350M` | ❌ **Deprecated** | 350M | Per-token | PyTorch | Replaced by LFM2.5 version |

### Key Changes from Design Assumptions

1. **Dimensions:** LiquidAI dense embeddings are **1024-d** (not 384-d). The sqlite-vec schema and vec0 table definitions must account for this.

2. **Format:** LiquidAI models ship as PyTorch (`sentence-transformers`) and GGUF (`llama.cpp`), **NOT as ONNX**. Running them in Rust via the `ort` crate would require:
   - Manually exporting to ONNX via `torch.onnx.export()` (requires Python + PyTorch, may fail on custom LFM2 ops like multiplicative gates)
   - Or using a GGUF inference path via the `llama-cpp-2` crate instead of `ort`

3. **Performance:** The LFM2.5-Embedding-350M has very fast CPU inference. The benchmarks show:
   - Query embedding: **7.3ms p50** (llama.cpp, M4 Max)
   - Query + Doc embedding + MaxSim: **34.3ms p50** (for ColBERT, uncached)
   - These are competitive with or faster than BGE-small via ONNX

4. **Recommendation for LiquidAI:** Best used in Phase 2 as an alternative backend, once the `EmbeddingBackend` trait is in place. The 1024-d output is better quality than 384-d BGE-small, and the GGUF format is well-supported by llama.cpp's Rust bindings.

### Citation
Liquid AI, "LFM2.5 Retrievers: Bi-directional LFMs for Fast Multilingual Search", Jun 2026.  
Liquid AI, "LFM2.5-Encoders: Fast at Long Context, Even on CPU", Jul 2026.

---

*Review generated by Hermes Agent subagent. All line references are to the July 2026 versions of the cited documents and codebase.*
