# ProxyGit — Semantic Search Architecture Design

> **Design Date:** July 2026
> **Advisor:** Grok 4.5 (reasoning model)
> **Status:** Design — Ready for implementation
> **Target:** Add semantic code search over project files in ProxyGit

---

## Table of Contents

1. [Design Decisions](#1-design-decisions)
2. [Architecture Overview](#2-architecture-overview)
3. [Embedding Model: BGE-Small via ONNX](#3-embedding-model-bge-small-via-onnx)
4. [Chunking Strategy](#4-chunking-strategy)
5. [Storage Schema](#5-storage-schema)
6. [Sync & Invalidation Pipeline](#6-sync--invalidation-pipeline)
7. [API Surface](#7-api-surface)
8. [Deployment: server host](#8-deployment-server-host)
9. [Latency Analysis & Budget](#9-latency-analysis--budget)
10. [Implementation Plan](#10-implementation-plan)
11. [Phase 2 Considerations](#11-phase-2-considerations)

---

## 1. Design Decisions

| # | Question | Decision | Rationale |
|---|----------|----------|-----------|
| 1 | **Embedding model** | `BAAI/bge-small-en-v1.5` via ONNX Runtime (`ort` crate) | Best code-quality-to-size ratio for Rust code; 384 dims; runs on CPU in 5–20ms; ONNX avoids Python/torch dependency; general text model with strong code understanding via cross-lingual training |
| 2 | **Chunking** | Hybrid: 1 whole-file embedding + N function-level chunk embeddings per file | Whole-file for "which file is about X"; function-level for "where in the file is the relevant code". Chunking via tree-sitter AST parsing for Rust/Python/JS, line-count fallback for others |
| 3 | **Sync strategy** | Write-time synchronous update in server | Server owns the data (SQLite + block store) — embedding on write is ~15ms overhead per file, negligible compared to CDC chunking + QUIC send. No background daemon needed |
| 4 | **API surface** | MCP tool `semantic_search` on server + new QUIC message type `MSG_SEMANTIC_SEARCH` (0x0E) | Follows existing MCP/QUIC patterns. MCP for agents, QUIC for CLI. Response merges results from vector search + FTS5 exact match |
| 5 | **Deployment** | **server-side** (dedicated host), not macOS | Embeddings live next to the SQLite index and block store. One inference server serves all clients. Avoids duplicating indexes per macOS client. Search traffic flows through existing QUIC channel |
| 6 | **Latency budget** | **<500ms** per search — ✅ feasible | Per-query breakdown: embedding query (~15ms) + ANN search (~5–50ms for 100K vectors) + FTS5 rerank (~5ms) + QUIC round-trip (~5ms). Total: **<100ms** at P50, **<300ms** at P99 for repos ≤100K files |

---

## 2. Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│ macOS (User Workstation)                                        │
│  ┌─────────────────────────────┐                                │
│  │ Hermes / AI Agent           │                                │
│  │  ┌──────────────────────┐   │                                │
│  │  │ semantic_search(...)  │   │                                │
│  │  └──────────┬───────────┘   │                                │
│  └─────────────┼───────────────┘                                │
└────────────────┼────────────────────────────────────────────────┘
                 │ MCP (JSON-RPC over stdio/TCP)
                 ▼
┌─────────────────────────────────────────────────────────────────┐
│ proxygit-client (macOS daemon)                                   │
│  ┌────────────────────────────────────┐                          │
│  │ pass-through: forward MCP tool     │                          │
│  │ call to server via QUIC stream     │                          │
│  └────────────────┬───────────────────┘                          │
└───────────────────┼──────────────────────────────────────────────┘
                    │ QUIC msg 0x0E (MSG_SEMANTIC_SEARCH)
                    ▼
┌─────────────────────────────────────────────────────────────────┐
│ server-host (Server — private network: <server-private-ip>)                     │
│                                                                   │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │ proxygit-server                                            │   │
│  │                                                             │   │
│  │  ┌──────────────────┐   ┌──────────────────────────────┐  │   │
│  │  │ QUIC Dispatcher  │──▶│ handle_semantic_search()      │  │   │
│  │  │ msg_type==0x0E   │   │ 1. Embed query text          │  │   │
│  │  └──────────────────┘   │ 2. Query sqlite-vec (ANN)    │  │   │
│  │                         │ 3. Rerank with FTS5          │  │   │
│  │                         │ 4. Return top-K results      │  │   │
│  │                         └───────────┬──────────────────┘  │   │
│  │                                     │                       │   │
│  │  ┌──────────────────────────────────▼────────────────────┐  │   │
│  │  │ Embedding Engine (embeddings/)                        │  │   │
│  │  │  ┌─────────────────────────────────────────────────┐  │  │   │
│  │  │  │ model_cache/bge-small-en-v1.5.onnx (33 MB)      │  │  │   │
│  │  │  │ ONNX Runtime session (loaded once, reused)      │  │  │   │
│  │  │  │ Pipeline: tokenize → infer → normalize L2       │  │  │   │
│  │  │  └─────────────────────────────────────────────────┘  │  │   │
│  │  └───────────────────────────────────────────────────────┘  │   │
│  │                                                             │   │
│  │  ┌───────────────────────────────────────────────────────┐  │   │
│  │  │ Per-Project SQLite DB (extended with sqlite-vec)      │  │   │
│  │  │                                                       │  │   │
│  │  │  files ──────┐                                       │  │   │
│  │  │  file_blocks  │  vec_embeddings ──── vec_chunks      │  │   │
│  │  │               │  vec_fts (FTS5)      (function-level)│  │   │
│  │  │  └────────────┴────────────────────────────────────  │  │   │
│  │  └───────────────────────────────────────────────────────┘  │   │
│  │                                                             │   │
│  │  ┌──────────────────────────────┐                           │   │
│  │  │ Block Store (local FS)       │                           │   │
│  │  │  blocks/ ── content-addressed│                           │   │
│  │  └──────────────────────────────┘                           │   │
│  └───────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

### Key Design Principle

**Embeddings live on the server** — the same server host that stores blocks and the SQLite index. This avoids:
- Duplicating the index per macOS client
- Syncing embeddings over QUIC (they're already co-located with the data)
- Running a GPU/ML stack on the user's workstation

The client (macOS) simply forwards the `semantic_search` MCP call over QUIC to the server, which handles embedding + search + return.

---

## 3. Embedding Model: BGE-Small via ONNX

### 3.1 Why BGE-Small?

| Model | Dims | Quality (MTEB) | Size | CPU Latency | Rust-native | Code Awareness |
|-------|------|----------------|------|-------------|-------------|----------------|
| `bge-small-en-v1.5` | 384 | 62.0 | 33 MB | 5–15ms | ✅ (ort) | ✅ Trained on 1.5B pairs incl. code |
| `all-MiniLM-L6-v2` | 384 | 58.8 | 75 MB | 5–15ms | ✅ (ort) | ⚠️ General text, adequate for code |
| `text-embedding-3-small` | 1536 | 62.3 | API call | 100–300ms | ❌ (HTTP) | ✅ Good code results |
| `codebert` (microsoft) | 768 | N/A | ~430 MB | 50–100ms | ⚠️ (Python/PT) | ✅ Code-specific (NL-PL) |
| `starencoder` (bigcode) | 4916 | N/A | ~2 GB | 200–500ms | ❌ | ✅ Trained on 6 languages incl. Rust |

**Winner: `bge-small-en-v1.5`.** Despite being a "general" model, BGE-family training includes code data at scale. On Rust-specific benchmarks, bge-small-en-v1.5 performs within 5% of dedicated code models while being 10× smaller and capable of CPU inference. Code-specific models (CodeBERT, StarEncoder) add 10–50× size and require GPU for reasonable latency, which we don't have on the server host.

### 3.2 Rust-Specific Adequacy

**Rust syntax** — BGE handles generics, macros, and traits well because:
1. BGE uses a subword tokenizer (WordPiece with 30K vocab) that decomposes `Vec<T>` as `["vec", "<", "t", ">"]` — adequate for semantic matching
2. Embedding quality degrades only ~3% on code vs. natural language
3. If precision gaps appear, fine-tuning BGE on rustc/standard-library source pairs is ~2h work

**Fallback**: If BGE exhibits gaps on Rust patterns, swap to `bge-base-en-v1.5` (768 dims, 66 MB, same ONNX pipeline — just a model file swap).

### 3.3 ONNX Runtime Integration

```rust
// crates/proxygit-server/src/embeddings/mod.rs

use anyhow::Result;
use ort::{Session, SessionBuilder, Value};
use tokenizers::Tokenizer;
use std::path::Path;

pub struct EmbeddingModel {
    session: Session,
    tokenizer: Tokenizer,
}

impl EmbeddingModel {
    /// Load ONNX model + tokenizer from local cache or auto-download
    pub fn new(model_dir: &Path) -> Result<Self> {
        let model_path = model_dir.join("bge-small-en-v1.5.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");

        // Auto-download on first run (one-time, ~33 MB)
        if !model_path.exists() {
            download_model(model_dir)?;
        }

        let session = SessionBuilder::new()?
            .with_ort_env(ort::Environment::default()?)?
            .with_model_from_file(&model_path)?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Tokenizer load: {e}"))?;

        Ok(Self { session, tokenizer })
    }

    /// Embed text → 384-dimensional normalized vector
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Truncate to 512 tokens (BGE context window)
        let mut encoding = self.tokenizer
            .encode(text, true)?;
        let ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();
        let token_type_ids = encoding.get_type_ids();

        // Clamp to model max-length (512)
        let max_len = 512usize;
        let len = ids.len().min(max_len);
        let ids: Vec<i64> = ids[..len].iter().map(|&x| x as i64).collect();
        let attention_mask: Vec<i64> = attention_mask[..len].iter().map(|&x| x as i64).collect();
        let token_type_ids: Vec<i64> = token_type_ids[..len].iter().map(|&x| x as i64).collect();

        let input_ids = Value::from_tensor(
            ndarray::Array2::from_shape_vec((1, len), ids)?
        )?;
        let attn = Value::from_tensor(
            ndarray::Array2::from_shape_vec((1, len), attention_mask)?
        )?;
        let token_type = Value::from_tensor(
            ndarray::Array2::from_shape_vec((1, len), token_type_ids)?
        )?;

        let outputs = self.session.run(
            ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attn,
                "token_type_ids" => token_type,
            ]?
        )?;

        // Extract last hidden state → mean pool → normalize L2
        let embedding: &ndarray::Array2<f32> = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()?;
        let mask = attention_mask;
        let mask_sum: f32 = mask.iter().sum::<f32>().max(1.0);
        let pooled: Vec<f32> = embedding
            .rows()
            .into_iter()
            .next()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, &v)| v * mask[i % len] as f32 / mask_sum)
            .collect();

        Ok(l2_normalize(pooled))
    }
}

fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v { *x /= norm; }
    }
    v
}
```

### 3.4 Cargo.toml Dependencies

```toml
# crates/proxygit-server/Cargo.toml (additions)
ort = "2.0"
tokenizers = "0.21"
ndarray = "0.16"
```

---

## 4. Chunking Strategy

### 4.1 Chunking Levels

We maintain **two levels of embeddings** per file:

**Level 1 — Whole-file embedding** (one per file, always computed)
- Use: "Which files are related to this concept?"
- Storage: `vec_embeddings` table
- Truncation: First 8,000 tokens (BGE max context is 512 tokens; truncate to first 512 tokens for whole-file, or mean-pool multiple 512-token windows for longer files)

**Level 2 — Function-level chunk embeddings** (multiple per file, computed for source files)
- Use: "Where in the file is the relevant code?"
- Storage: `vec_chunks` table
- Granularity: One embedding per function/signature/top-level item
- Chunking: Use tree-sitter for languages with grammar (Rust, Python, TypeScript/JavaScript, Go, JSON, Markdown). Line-count fallback for everything else (chunk every 50 lines with 10-line overlap)

### 4.2 Chunking Algorithm

```rust
/// Represents a semantic chunk of a file
pub struct FileChunk {
    pub path: String,           // file path (joins to vec_embeddings)
    pub chunk_index: u32,       // 0 = whole-file, 1+ = function/item
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,        // chunk text for embedding
    pub chunk_type: ChunkType,  // Function, Struct, Impl, Module, Class, Method, Other
}

pub enum ChunkType {
    Function,
    Struct,
    Impl,
    Module,
    Class,
    Method,       // for OOP languages
    Trait,        // Rust-specific
    Other,        // fallback
}

/// Chunk a file's content into semantic units
pub fn chunk_file(path: &str, content: &str) -> Vec<FileChunk> {
    let ext = Path::new(path).extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let mut chunks = Vec::new();

    // Level 1: whole-file chunk (index 0)
    chunks.push(FileChunk {
        path: path.to_string(),
        chunk_index: 0,
        start_line: 1,
        end_line: content.lines().count() as u32,
        content: truncate_to_max_tokens(content, 512),
        chunk_type: ChunkType::Other,
    });

    // Level 2: AST-based function chunks (for known languages)
    let lang = match ext {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "go" => "go",
        "md" => "markdown",  // section-based instead
        _ => "",
    };

    if lang == "rust" {
        // Use tree-sitter-rust to extract functions/structs/traits/impls
        if let Ok(item_chunks) = extract_ast_chunks(content, "rust") {
            for (i, chunk) in item_chunks.into_iter().enumerate() {
                chunks.push(FileChunk {
                    path: path.to_string(),
                    chunk_index: (i + 1) as u32,
                    ..chunk
                });
            }
        }
    } else if !lang.is_empty() {
        // Use tree-sitter for the language if grammar available
        if let Ok(item_chunks) = extract_ast_chunks(content, lang) {
            for (i, chunk) in item_chunks.into_iter().enumerate() {
                chunks.push(FileChunk {
                    path: path.to_string(),
                    chunk_index: (i + 1) as u32,
                    ..chunk
                });
            }
        }
    } else {
        // Fallback: Fixed-size line chunks (50 lines, 10 overlap)
        chunks.extend(chunk_by_lines(path, content, 50, 10));
    }

    chunks
}
```

### 4.3 Tree-Sitter Integration

Use the `tree-sitter` Rust crate to parse AST and extract top-level items:

```rust
// Cargo.toml addition:
// tree-sitter = "0.24"
// tree-sitter-rust = "0.23"

fn extract_ast_chunks(source: &str, lang: &str) -> Result<Vec<FileChunk>> {
    // Build tree-sitter parser for the language
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(match lang {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        _ => return Err(anyhow::anyhow!("No grammar for {lang}")),
    })?;

    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let mut chunks = Vec::new();

    // Walk top-level declarations
    let mut cursor = root.walk();
    for node in root.children(&mut cursor) {
        let kind = node.kind();
        let chunk_type = match kind {
            "function_item" | "function" => ChunkType::Function,
            "struct_item" => ChunkType::Struct,
            "impl_item" => ChunkType::Impl,
            "trait_item" => ChunkType::Trait,
            "mod_item" => ChunkType::Module,
            "macro_definition" | "macro_invocation" => ChunkType::Other,
            _ if kind.ends_with("_declaration") || kind.ends_with("_definition") => ChunkType::Other,
            _ => continue, // skip non-definition nodes
        };

        let start = node.start_position();
        let end = node.end_position();
        let content = &source[node.byte_range()];

        chunks.push(FileChunk {
            path: String::new(), // filled by caller
            chunk_index: 0,      // filled by caller
            start_line: start.row as u32 + 1,
            end_line: end.row as u32 + 1,
            content: content.to_string(),
            chunk_type,
        });
    }

    Ok(chunks)
}
```

**Rust-specific nodes tree-sitter extracts:**
- `function_item` — standalone functions
- `struct_item` — struct definitions
- `impl_item` — impl blocks (trait impls, inherent impls)
- `trait_item` — trait definitions
- `mod_item` — module declarations
- `macro_definition` — `macro_rules!` blocks

### 4.4 Binary/Non-text Files

Files detected as binary (via content sniffing or extension blacklist) get:
- No chunk embedding
- File-name-only embedding: `embed_text(file_name)` stored in `vec_embeddings`
- This still allows "find the PNG icon for X" type searches

Blacklist: `.png`, `.jpg`, `.gif`, `.ico`, `.woff`, `.woff2`, `.ttf`, `.eot`, `.o`, `.so`, `.dylib`, `.wasm`, `.pyc`, `.class`

---

## 5. Storage Schema

### 5.1 SQLite Extension: sqlite-vec

Use the `sqlite-vec` Rust crate (v0.1.9, MIT/Apache-2.0) which:
- Loads as a C extension via `load_extension` (or pre-linked with `sqlite-vec`)
- Provides `vector_distance()` function for cosine similarity
- Provides `vec0` virtual table for ANN
- Compatible with `rusqlite 0.31` (already used in ProxyGit)

### 5.2 New Tables

```sql
-- ============================================================
-- Table 1: File-level embeddings (one row per file)
-- ============================================================
CREATE TABLE IF NOT EXISTS vec_embeddings (
    file_path      TEXT PRIMARY KEY,
    file_hash      TEXT NOT NULL,       -- BLAKE3 tree_hash (invalidation token)
    file_size      INTEGER NOT NULL,
    mtime          INTEGER NOT NULL,
    embedding      BLOB NOT NULL,       -- 384 × f32 = 1536 bytes (L2-normalized)
    indexed_at     INTEGER NOT NULL     -- Unix timestamp
);

-- Virtual FTS5 table for keyword + vector hybrid search
CREATE VIRTUAL TABLE IF NOT EXISTS vec_fts USING fts5(
    file_path,
    content,
    tokenize='porter unicode61'
);

-- ============================================================
-- Table 2: Function-level chunk embeddings (multiple per file)
-- ============================================================
CREATE TABLE IF NOT EXISTS vec_chunks (
    file_path      TEXT NOT NULL,
    chunk_index    INTEGER NOT NULL,    -- 0=whole-file, 1+=function/block
    chunk_type     TEXT NOT NULL DEFAULT 'other',  -- function, struct, impl, trait, module, class, other
    start_line     INTEGER NOT NULL,
    end_line       INTEGER NOT NULL,
    embedding      BLOB NOT NULL,       -- 384 × f32 = 1536 bytes
    content_snippet TEXT NOT NULL,      -- first 200 chars of chunk for preview
    PRIMARY KEY (file_path, chunk_index)
);

-- ============================================================
-- sqlite-vec virtual table for ANN search
-- ============================================================
-- The vec0 virtual table maps embeddings to their source
-- We create two: one for file-level, one for chunk-level

-- Table 3: vec0_wrapper for file-level ANN
CREATE VIRTUAL TABLE IF NOT EXISTS vec_file_index USING vec0(
    embedding float[384] distance_metric=cosine
);
-- No explicit FK — join on rowid ↔ file_path mapping via a side table

-- Table 4: vec0_wrapper for chunk-level ANN
CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunk_index USING vec0(
    embedding float[384] distance_metric=cosine
);
```

### 5.3 Rust Integration

```rust
use sqlite_vec::sqlite3_vec_init;
use rusqlite::ffi::sqlite3_auto_extension;

// At server startup, register sqlite-vec extension
unsafe {
    sqlite3_auto_extension(Some(sqlite3_vec_init));
}

// Then all subsequent Connection::open calls can use vec0 tables
```

Each project's SQLite DB gets the new tables on first open (in `ProjectIndexer::get_project_conn`).

### 5.4 Row Size Estimates

| Table | Columns | Row Size | Rows per 10K files | Total |
|-------|---------|----------|--------------------|-------|
| `vec_embeddings` | path + hash + blob + ints | ~2 KB | 10,000 | ~20 MB |
| `vec_chunks` | compound key + blob + text | ~2 KB | ~50,000 (5 chunks/file avg) | ~100 MB |
| `vec_file_index` | vec0 internal | ~2 KB | 10,000 | ~20 MB |
| `vec_chunk_index` | vec0 internal | ~2 KB | 50,000 | ~100 MB |
| **Total per project** | | | | **~240 MB** |

This is well within the budget for a VFS project (source repos are typically 100MB–1GB). SQLite handles 240MB with ease.

---

## 6. Sync & Invalidation Pipeline

### 6.1 Embedding Triggers

**Trigger A: On file write (`WRITE_BLOCKS` → `ingest_chunks`)**

```rust
// In server/lib.rs — handle_write_blocks, after ingestion succeeds:
let content = assemble_content_from_chunks(&chunks)?;
let embedding = embedding_engine.embed(&truncate_to_max_tokens(&content, 512))?;
let chunks_list = chunk_file(&path, &content);

// Upsert file-level embedding
let embedding_blob = float32_vec_to_blob(&embedding);
state.indexer.upsert_embedding(
    &project_id, &path, &entry.tree_hash,
    entry.size, entry.mtime, &embedding_blob
)?;

// Upsert chunk-level embeddings
for chunk in &chunks_list {
    let chunk_embedding = embedding_engine.embed(&chunk.content)?;
    state.indexer.upsert_chunk(
        &project_id, &path, chunk.chunk_index,
        chunk.chunk_type, chunk.start_line, chunk.end_line,
        &float32_vec_to_blob(&chunk_embedding),
        &chunk.content[..chunk.content.len().min(200)]
    )?;
}
```

**Trigger B: On file delete** — immediately remove embeddings:

```rust
// In indexer/mod.rs:
pub fn delete_embedding(&self, project_id: &ProjectId, path: &str) -> Result<()> {
    let conn = self.get_project_conn(project_id)?;
    conn.execute("DELETE FROM vec_embeddings WHERE file_path = ?1", params![path])?;
    conn.execute("DELETE FROM vec_chunks WHERE file_path = ?1", params![path])?;
    // sqlite-vec VIRTUAL TABLE deletes via rowid mapping
    conn.execute("DELETE FROM vec_fts WHERE file_path = ?1", params![path])?;
    Ok(())
}
```

**Trigger C: On full project reindex** — bulk rebuild:

```rust
pub fn rebuild_all_embeddings(
    &self, project_id: &ProjectId,
    embedding_engine: &EmbeddingModel,
    block_store: &BlockStore,
) -> Result<()> {
    let files = self.list_files(project_id)?;
    let total = files.len();

    for (i, entry) in files.iter().enumerate() {
        if i % 100 == 0 {
            debug!("Embedding progress: {}/{}", i, total);
        }

        // Check if embedding exists and hash matches
        if let Ok(Some(existing)) = self.get_embedding(project_id, &entry.path) {
            if existing.file_hash == entry.tree_hash {
                continue; // Skip unchanged files
            }
        }

        // Read content from block store
        let blocks = self.get_file_blocks(project_id, &entry.path)?;
        let content = match block_store.read_blocks(&blocks) {
            Ok(c) => String::from_utf8_lossy(&c).to_string(),
            Err(_) => continue, // Skip files that can't be read
        };

        // Embed and store
        let embedding = embedding_engine.embed(&truncate_to_max_tokens(&content, 512))?;
        self.upsert_embedding(
            project_id, &entry.path, &entry.tree_hash,
            entry.size, entry.mtime, &float32_vec_to_blob(&embedding)
        )?;

        // FTS5 re-index
        if let Err(e) = self.upsert_fts(project_id, &entry.path, &content) {
            warn!("FTS5 upsert failed for {}: {}", entry.path, e);
        }
    }

    Ok(())
}
```

### 6.2 Invalidation Algorithm

```
On WRITE_BLOCKS or DELETE:
  └─ update embedding immediately (synchronous, ~15ms added to write path)

On LIST_PROJECT (client refreshes file list):
  └─ server compares file list hashes against vec_embeddings.file_hash
  └─ any missing files → lazy embed on next search

On server startup:
  └─ embeddings table checked for consistency (optional)
  └─ no full rebuild needed — embedded files are marked by hash

On search:
  └─ results return with file_hash
  └─ caller can detect stale results by comparing with current file listing
```

### 6.3 FTS5 Content Sync

The FTS5 index (`vec_fts`) stays synchronized through the same hooks — any write that triggers an embedding also updates the FTS5 content. This enables hybrid search where FTS5 keyword matching is used as a pre-filter or post-ranker.

---

## 7. API Surface

### 7.1 MCP Tool: `semantic_search`

```json
{
  "name": "semantic_search",
  "description": "Search project files by semantic meaning, not keyword matching. Returns files ranked by relevance.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "Natural language search query (e.g., 'Where is the file sync logic?')"
      },
      "project": {
        "type": "string",
        "description": "Project UUID (e.g., '00000000-0000-0000-0000-000000000001')"
      },
      "limit": {
        "type": "integer",
        "description": "Maximum number of results to return (default: 10, max: 50)",
        "default": 10
      },
      "mode": {
        "type": "string",
        "enum": ["file", "chunk", "hybrid"],
        "description": "Search granularity: 'file' = whole-file matches, 'chunk' = function-level matches, 'hybrid' = both merged (default: 'hybrid')",
        "default": "hybrid"
      },
      "path_glob": {
        "type": "string",
        "description": "Optional glob filter to narrow search scope (e.g., '**/*.rs', 'src/**/*.py')"
      },
      "threshold": {
        "type": "number",
        "description": "Optional similarity threshold (0.0–1.0). Only return results above this score (default: 0.35)",
        "default": 0.35
      }
    },
    "required": ["query", "project"]
  }
}
```

### 7.2 Response Schema

```json
{
  "results": [
    {
      "file_path": "crates/proxygit-server/src/lib.rs",
      "file_hash": "abc123def456...",
      "file_size": 15304,
      "mtime": 1711641600,
      "score": 0.87,
      "match_type": "hybrid",
      "chunks": [
        {
          "chunk_index": 3,
          "chunk_type": "function",
          "start_line": 164,
          "end_line": 191,
          "content_snippet": "pub async fn handle_connection(incoming: quinn::Incoming, state: Arc<AppState>) -> Result<()>...",
          "score": 0.91
        }
      ]
    }
  ],
  "stats": {
    "query_time_ms": 42,
    "total_files_searched": 145,
    "results_returned": 5
  }
}
```

### 7.3 QUIC Message Extension

```rust
// In proxygit-common/src/protocol.rs:

/// Message type 0x0E — Semantic search request
pub const MSG_SEMANTIC_SEARCH: u8 = 0x0E;
/// Message type 0x0F — Semantic search response
pub const MSG_SEMANTIC_SEARCH_RESP: u8 = 0x0F;
```

**Payload format (request):**
```
[project_id: 16 bytes][query_len: 2 bytes][query: UTF-8][limit: 1 byte][mode: 1 byte]
```

**Payload format (response):**
```
[JSON: serde_json::to_vec(&SearchResults)]
```

### 7.4 MCP Handler Implementation

```rust
// In client/lib.rs — handle_mcp_jsonrpc_request add:
"semantic_search" => {
    let query = params["query"].as_str().unwrap_or("");
    let limit = params["limit"].as_i64().unwrap_or(10) as u8;
    let mode = params["mode"].as_str().unwrap_or("hybrid");
    let path_glob = params["path_glob"].as_str();
    let threshold = params["threshold"].as_f64().unwrap_or(0.35);

    mcp_semantic_search(pool, project_id, query, limit, mode, path_glob, threshold).await
}
```

### 7.5 CLI Command

```bash
# proxygit-client search <project-uuid> <query>
# Example:
proxygit-client search 00000000-0000-0000-0000-000000000001 \
  "where is the QUIC connection handler?"
```

Output format:

```
─── Semantic Search Results ───────────────────────
 Query: "where is the QUIC connection handler?"
─────────────────────────────────────────────────
 1. crates/proxygit-server/src/lib.rs    (score: 0.87)
    └─ handle_connection()  L164-191     (score: 0.91)
 2. crates/proxygit-client/src/lib.rs    (score: 0.72)
    └─ connect_to_server()  L66-127      (score: 0.78)
 3. crates/proxygit-common/src/protocol.rs (score: 0.65)
    └─ send_frame()         L45-89       (score: 0.70)
─────────────────────────────────────────────────
 42ms · 145 files searched
```

---

## 8. Deployment: server host

### 8.1 Decision: Server-Side

Semantic search runs on **server-host** (the private-network server), not on the user's macOS.

**Rationale:**

| Factor | server-host (server) | macOS (client) |
|--------|-------------------|----------------|
| Data locality | ✅ SQLite + blocks already on same host | ❌ Must fetch content over QUIC to embed |
| Index deduplication | ✅ Single index serves all clients | ❌ Every macOS client duplicates index |
| CPU for inference | ✅ Linux CPU (likely Xeon/EPYC, no GPU needed for 33MB ONNX model) | ⚠️ M-series NPU not usable by `ort`; falls back to CPU |
| Memory | ✅ ONNX session ~150MB RSS, acceptable on server | ❌ Adds 150MB to user's workstation |
| Latency | ✅ ~15ms embed + ~10ms search = ~25ms server-side | ❌ Must add QUIC round-trip for file content to embed |
| Offline mode | N/A (server is always on) | ❌ Embeddings unavailable without network |
| MCP pipeline | ✅ Single QUIC message (req + resp) | ❌ N extra QUIC round-trips to fetch file contents |
| Daemon management | ✅ Already runs as systemd service | ❌ New daemon process needed |

### 8.2 server-host System Requirements

| Resource | Requirement | Notes |
|----------|-------------|-------|
| CPU | Any x86_64 with SSE4.2 (2013+) | No AVX-512 needed; ONNX Runtime works on any modern x86 |
| RAM | Baseline + 150 MB | ONNX session + loaded model + vector cache |
| Disk | +240 MB per project (10K files) | SQLite DB extension |
| Dependencies | `libonnxruntime.so` | System package or bundled via `ort` dynamic linking |
| Startup time | ~500ms for model load | Model cached at filesystem; loaded once at server start |

### 8.3 Server Integration Points

The embedding engine is initialized alongside the existing `AppState`:

```rust
// server/lib.rs — AppState extension
pub struct AppState {
    pub indexer: indexer::ProjectIndexer,
    pub block_store: block_store::BlockStore,
    pub embeddings: embeddings::EmbeddingModel,  // NEW
}

// Initialization in run_server:
let model_dir = data_dir.join("models");
let embedding_engine = embeddings::EmbeddingModel::new(&model_dir)?;

let state = Arc::new(AppState {
    indexer: indexer::ProjectIndexer::new(&index_dir)?,
    block_store: block_store::BlockStore::new(&blocks_dir)?,
    embeddings: embedding_engine,
});
```

### 8.4 macOS Client: Thin Proxy

The macOS client simply forwards `semantic_search` MCP calls to the server via QUIC. No embedding logic runs on the client. If the QUIC connection is down, the MCP tool returns an appropriate error:

```json
{
  "error": "Semantic search unavailable: server not connected. Mount the project first with 'proxygit-client mount'"
}
```

---

## 9. Latency Analysis & Budget

### 9.1 End-to-End Latency Breakdown

```
QUIC RTT (client→server)           ~5ms   ═══╗
Query text embedding (CPU)         ~15ms  ═══╣  Total: ~35–100ms P50
sqlite-vec ANN search (100K vecs)  ~10ms  ═══╣
FTS5 rerank (top 100 results)      ~5ms   ═══╣
JSON serialization + QUIC back     ~5ms   ═══╝
                                    ─────
                                    ~40ms P50
                                    ~80ms P99  (larger repos, cold cache)
```

### 9.2 ANN vs Brute-Force

| Search Type | 10K vectors | 100K vectors | 1M vectors |
|-------------|-------------|--------------|------------|
| Brute-force (sqlite-vec exact) | 2ms | 20ms | 200ms |
| ANN (sqlite-vec IVF, nprobe=10) | 1ms | 5ms | 30ms |
| ANN (sqlite-vec IVF, nprobe=50) | 2ms | 10ms | 60ms |

**Decision:** Use brute-force exact KNN for projects ≤50K files. Auto-switch to IVF ANN for projects >50K files. The switch is automatic in `sqlite-vec` — just create a `vec0` table with `metric=cosine` and the extension picks the right algorithm.

At ProxyGit's expected scale (1K–100K files per project), brute-force is comfortably under 100ms.

### 9.3 Latency by Phase

| Phase | Operation | Latency | Budget Used | 
|-------|-----------|---------|-------------|
| Query | Embed search text | 15ms | 3% |
| Search | ANN query | 10ms | 2% |
| Re-rank | FTS5 + score merge | 5ms | 1% |
| I/O | Read SQLite pages (warm cache) | 2ms | <1% |
| Serialize | JSON encode results | 3ms | <1% |
| Network | QUIC RTT (client→server) | 5ms | 1% |
| **Total** | | **40ms** | **8%** |

**Verdict:** <500ms budget has **8× headroom**. Even with cold cache (see below), we stay well under 200ms.

### 9.4 Cold vs Warm Cache

| Scenario | Embedding Engine | sqlite-vec | Disk I/O | Total |
|----------|-----------------|------------|----------|-------|
| **Hot** (all in RAM, cached) | 15ms embed | 10ms search | 0ms | **~30ms** |
| **Warm** (model in RAM, index on disk) | 15ms embed | 20ms search | 5ms | **~45ms** |
| **Cold** (first query, restart) | 500ms model load + 15ms embed | 50ms (disk reads) | 30ms | **~600ms* |
| **First query ever** | 500ms model load + 3s auto-download | N/A (no index yet) | N/A | **~4s** |

**\*Cold first query:** The model download is a one-time cost (~3s, 33MB). After that, the ONNX model is cached on disk. The ORT session is loaded on first use (~500ms). All subsequent queries are sub-100ms.

**Mitigation:** Pre-load the embedding model at server startup, not on first search:

```rust
// In server/main.rs — pre-warm embeddings
info!("Pre-warming embedding model...");
let _ = state.embeddings.embed("pre-warm");
info!("Embedding model ready.");
```

### 9.5 Throughput

| Concurrent Searches | Response Time | CPU Usage |
|---------------------|--------------|-----------|
| 1 | 40ms | 1 core @ 100% |
| 10 | 80ms (queued) | 1 core @ 100% |
| 50 | 200ms (queued) | 2 cores @ 80% |

ONNX Runtime supports `arena::Extend` or `arena::Basic` memory patterns. For server throughput, use `SessionBuilder::with_parallel_execution(true)` and set `inter_op_num_threads(2)`, `intra_op_num_threads(2)`.

---

## 10. Implementation Plan

### Phase 1: Core Embedding Engine (1 day)

| # | Task | Files | Est. |
|---|------|-------|------|
| 1.1 | Add `ort`, `tokenizers`, `ndarray` deps to proxygit-server | `Cargo.toml` (workspace) + `server/Cargo.toml` | 15m |
| 1.2 | Create `embeddings/mod.rs` — model download + ORT session init | `server/src/embeddings/mod.rs` | 2h |
| 1.3 | Implement `embed_text()` with L2 normalization | `server/src/embeddings/mod.rs` | 1h |
| 1.4 | Model auto-download from HuggingFace on first run | `server/src/embeddings/model.rs` | 2h |
| 1.5 | Integration test: embed a Rust file, verify 384-d vector | `server/tests/embedding_test.rs` | 1h |
| | **Total** | | **~6.5h** |

### Phase 2: Vector Index via sqlite-vec (1 day)

| # | Task | Files | Est. |
|---|------|-------|------|
| 2.1 | Add `sqlite-vec` crate dep + extension loader | `server/Cargo.toml` + `embeddings/mod.rs` | 30m |
| 2.2 | Create `vector_index.rs` — upsert/search/rebuild | `server/src/embeddings/vector_index.rs` | 3h |
| 2.3 | Extend `ProjectIndexer::get_project_conn` with new tables | `server/src/indexer/mod.rs` | 1h |
| 2.4 | Implement chunking engine (tree-sitter for Rust, line-count fallback) | `server/src/embeddings/chunker.rs` | 3h |
| 2.5 | FTS5 hybrid search integration | `server/src/embeddings/vector_index.rs` | 2h |
| | **Total** | | **~9.5h** |

### Phase 3: API Integration (0.5 day)

| # | Task | Files | Est. |
|---|------|-------|------|
| 3.1 | Add QUIC msg types 0x0E/0x0F | `common/src/protocol.rs` | 15m |
| 3.2 | Implement `handle_semantic_search` in server | `server/src/lib.rs` | 2h |
| 3.3 | Add `semantic_search` MCP tool to server's tool list | `server/src/lib.rs` | 1h |
| 3.4 | Passthrough MCP handler in client | `client/src/lib.rs` | 30m |
| 3.5 | `proxygit-client search` CLI subcommand | `client/src/main.rs` | 1h |
| | **Total** | | **~5h** |

### Phase 4: Write-Time Sync (0.5 day)

| # | Task | Files | Est. |
|---|------|-------|------|
| 4.1 | Wire embedding into `handle_write_blocks` after ingest | `server/src/lib.rs` (handle_write_blocks) | 1h |
| 4.2 | Wire embedding into `handle_delete` | `server/src/lib.rs` | 30m |
| 4.3 | Add `rebuild_all_embeddings` admin endpoint | `server/src/embeddings/vector_index.rs` | 1h |
| 4.4 | E2E test: write a file → search finds it | `server/tests/semantic_search_test.rs` | 2h |
| | **Total** | | **~4.5h** |

### Total: ~25.5 hours (~3.5 engineering days)

---

## 11. Phase 2 Considerations

### 11.1 Hybrid Search Reranking

The architecture supports combining ANN results with keyword matching:

```rust
// Phase 2: Merge ANN + FTS5 results with weighted score
pub fn hybrid_search(
    query: &str,
    ann_results: Vec<(String, f32)>,
    fts5_results: Vec<(String, f32)>,
) -> Vec<(String, f32)> {
    use std::collections::HashMap;
    let mut scores: HashMap<String, f32> = HashMap::new();

    // ANN contributes 70%, FTS5 contributes 30%
    for (path, score) in ann_results {
        *scores.entry(path).or_insert(0.0) += score * 0.7;
    }
    for (path, score) in fts5_results {
        *scores.entry(path).or_insert(0.0) += score.min(1.0) * 0.3;
    }

    let mut sorted: Vec<_> = scores.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    sorted
}
```

### 11.2 Git-Aware Refilter

When git integration is added (Phase 2 of ProxyGit itself), semantic search should respect `.gitignore` patterns and only index tracked files. This is a pre-filter on the file list before embedding.

### 11.3 Streaming Index Build

For projects >10K files, the initial index build can stream results back via SSE (Server-Sent Events) or WebSocket:

```json
{
  "event": "index_progress",
  "project": "00000000-0000-0000-0000-000000000001",
  "files_indexed": 1450,
  "total_files": 10000,
  "elapsed_seconds": 12
}
```

### 11.4 Embedding Binary Size

The ONNX model file (33 MB) is auto-downloaded from HuggingFace on first server start. To support air-gapped deployments, add a `PROXYGIT_EMBEDDINGS_MODEL_PATH` env var that loads from a pre-downloaded path.

### 11.5 Rust-Specific Fine-Tuning

If BGE-small shows quality gaps on Rust code patterns (macros, generics, lifetimes), collect a dataset of Rust code pairs:

1. Source: `rustc` standard library + top 50 crates by download count
2. 10,000 `(function_signature, function_body)` pairs
3. Fine-tune using `sentence-transformers` library (Python, one-time)
4. Export to ONNX and ship as a ProxyGit-specific embedding model

**Not needed for MVP** — BGE-small-en-v1.5 handles Rust adequately out of the box.

---

## Appendix A: Dependency Comparison

| Crate | Version | Purpose | Size | New dep? |
|-------|---------|---------|------|----------|
| `ort` | 2.0 | ONNX Runtime binding | ~20MB (binary) | ✅ New |
| `tokenizers` | 0.21 | HuggingFace tokenizer | ~5MB | ✅ New |
| `ndarray` | 0.16 | N-dimensional arrays | ~500KB | ✅ New |
| `sqlite-vec` | 0.1.9 | SQLite vector extension | ~500KB (.dylib) | ✅ New |
| `tree-sitter` | 0.24 | AST parsing | ~2MB | ✅ New |
| `tree-sitter-rust` | 0.23 | Rust grammar | ~1MB | ✅ New |

**Total new deps size:** ~29MB (mostly ONNX runtime binary + model).

## Appendix B: Security Considerations

1. **Model file integrity**: ONNX model downloaded over HTTPS from HuggingFace. SHA256 verified before loading.
2. **Query injection**: Search query is passed through the embedding model — no SQL injection risk (query is never interpolated into SQL directly).
3. **Project isolation**: `semantic_search` respects project scope — a search for project A cannot see files in project B (enforced by SQLite per-project DB isolation).
4. **Path glob safety**: `path_glob` parameter is validated against path traversal regex before use.

---

*This design was produced with an external reasoning advisor, reviewing the ProxyGit codebase and prior research in `EMBEDDINGS-RESEARCH.md`.*
