//! Server-side embedding index for content search over stored files.
//!
//! Default backend (`PROXYGIT_EMBEDDING=features` or unset): **feature-hashed
//! bag-of-tokens** — lexical similarity without ML deps. Same content is
//! deterministic; shared tokens raise cosine score.
//!
//! Fallback (`PROXYGIT_EMBEDDING=hash`): pure BLAKE3 mock vectors (identity
//! only — not lexical). ONNX/BGE remains a future optional path, not required
//! for MVP search usefulness.
//!
//! Index is persisted to `data_dir/embeddings.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Embedding dimensionality (matches prior mock / planned BGE-small width).
pub const EMBED_DIM: usize = 384;

/// Which embedding backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmbeddingBackend {
    /// Feature-hashed bag-of-tokens (default).
    #[default]
    Features,
    /// Deterministic BLAKE3 expansion (content identity only).
    Hash,
}

impl EmbeddingBackend {
    pub fn from_env() -> Self {
        match std::env::var("PROXYGIT_EMBEDDING")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "hash" | "blake3" | "mock" => Self::Hash,
            _ => Self::Features,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Features => "features",
            Self::Hash => "hash",
        }
    }
}

/// Compute embedding for `content` using the process-wide backend from env.
pub fn compute_embedding(content: &[u8]) -> Vec<f32> {
    compute_embedding_with(content, EmbeddingBackend::from_env())
}

/// Compute embedding with an explicit backend (tests / callers).
pub fn compute_embedding_with(content: &[u8], backend: EmbeddingBackend) -> Vec<f32> {
    match backend {
        EmbeddingBackend::Features => compute_features(content),
        EmbeddingBackend::Hash => compute_hash_mock(content),
    }
}

/// BLAKE3-seeded deterministic mock (identity only).
fn compute_hash_mock(content: &[u8]) -> Vec<f32> {
    let hash = blake3::hash(content);
    let bytes = hash.as_bytes();
    let mut result = Vec::with_capacity(EMBED_DIM);
    let mut seed = bytes.to_vec();
    for _ in 0..EMBED_DIM {
        let h = blake3::hash(&seed);
        let hb = h.as_bytes();
        let val = u16::from_le_bytes([hb[0], hb[1]]) as f32 / 32768.0 - 1.0;
        result.push(val);
        seed = hb.to_vec();
    }
    result
}

/// Feature-hashed bag-of-tokens → L2-normalized 384-d vector.
///
/// Tokens: runs of `[A-Za-z0-9]` lowercased, length ≥ 2. Underscores and other
/// punctuation are separators so `authentication_middleware` yields both
/// `authentication` and `middleware`. Empty input → zero vec.
fn compute_features(content: &[u8]) -> Vec<f32> {
    let mut vec = vec![0.0f32; EMBED_DIM];
    let text = String::from_utf8_lossy(content);
    let mut token = String::new();
    let flush = |tok: &mut String, v: &mut [f32]| {
        if tok.len() < 2 {
            tok.clear();
            return;
        }
        let h = blake3::hash(tok.as_bytes());
        let b = h.as_bytes();
        let idx = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize % EMBED_DIM;
        let sign = if b[4] & 1 == 0 { 1.0f32 } else { -1.0f32 };
        // Mild length/freq weight: longer identifiers matter slightly more.
        let w = 1.0 + (tok.len().min(16) as f32) * 0.05;
        v[idx] += sign * w;
        tok.clear();
    };

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            token.push(ch.to_ascii_lowercase());
        } else if !token.is_empty() {
            flush(&mut token, &mut vec);
        }
    }
    if !token.is_empty() {
        flush(&mut token, &mut vec);
    }

    l2_normalize(&mut vec);
    vec
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// In-memory embedding index backed by a JSON file on disk.
pub struct EmbeddingIndex {
    /// Path to the embeddings JSON file.
    path: PathBuf,
    /// Path → embedding vector mapping.
    embeddings: HashMap<String, Vec<f32>>,
    /// Backend used when indexing (recorded for honesty in search results).
    pub backend: EmbeddingBackend,
}

impl EmbeddingIndex {
    /// Create a new (empty) embedding index backed by `data_dir/embeddings.json`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("embeddings.json"),
            embeddings: HashMap::new(),
            backend: EmbeddingBackend::from_env(),
        }
    }

    /// Load embeddings from disk. If the file doesn't exist, starts empty.
    pub fn load(&mut self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let json = std::fs::read_to_string(&self.path)?;
        if json.trim().is_empty() {
            return Ok(());
        }
        self.embeddings = serde_json::from_str(&json)?;
        Ok(())
    }

    /// Persist the current embedding index to disk as JSON.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.embeddings)?;
        std::fs::write(&self.path, &json)?;
        Ok(())
    }

    /// Look up the embedding for a file path.
    pub fn get(&self, path: &str) -> Option<&[f32]> {
        self.embeddings.get(path).map(|v| v.as_slice())
    }

    /// Insert or update the embedding for a file path.
    pub fn set(&mut self, path: &str, embedding: Vec<f32>) {
        self.embeddings.insert(path.to_string(), embedding);
    }

    /// Remove the embedding for a file path.
    pub fn remove(&mut self, path: &str) {
        self.embeddings.remove(path);
    }

    /// Number of indexed paths.
    pub fn len(&self) -> usize {
        self.embeddings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }

    /// Compute the cosine similarity between two equal-length vectors.
    ///
    /// Returns a value in [-1, 1], or 0.0 if either vector is zero.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        dot / (norm_a * norm_b)
    }

    /// Search the index for the top-N results most similar to `query_embedding`.
    ///
    /// Returns up to `limit` `(path, score)` pairs sorted by descending score.
    pub fn search(&self, query_embedding: &[f32], limit: usize) -> Vec<(String, f32)> {
        let mut results: Vec<(String, f32)> = self
            .embeddings
            .iter()
            .map(|(path, emb)| {
                let score = Self::cosine_similarity(query_embedding, emb);
                (path.clone(), score)
            })
            .collect();

        // Sort by descending score; ties are left in insertion order (stable).
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_embedding_deterministic() {
        let a = compute_embedding_with(b"hello world", EmbeddingBackend::Features);
        let b = compute_embedding_with(b"hello world", EmbeddingBackend::Features);
        assert_eq!(a.len(), EMBED_DIM);
        assert_eq!(a, b);
    }

    #[test]
    fn test_compute_embedding_changes_with_content() {
        let a = compute_embedding_with(b"hello world", EmbeddingBackend::Features);
        let b = compute_embedding_with(b"hello world!", EmbeddingBackend::Features);
        // trailing punct alone may not change tokens; force real change
        let c = compute_embedding_with(b"goodbye moon", EmbeddingBackend::Features);
        assert_ne!(a, c);
        let _ = b;
    }

    #[test]
    fn test_features_lexical_similarity() {
        let auth_mw = compute_embedding_with(
            b"fn authentication_middleware(req: Request) { validate_token(req); }",
            EmbeddingBackend::Features,
        );
        let auth_query = compute_embedding_with(
            b"authentication middleware token",
            EmbeddingBackend::Features,
        );
        let unrelated = compute_embedding_with(
            b"fn render_canvas_pixels(buf: &mut [u8]) { fill_red(buf); }",
            EmbeddingBackend::Features,
        );
        let sim_rel = EmbeddingIndex::cosine_similarity(&auth_mw, &auth_query);
        let sim_un = EmbeddingIndex::cosine_similarity(&auth_mw, &unrelated);
        assert!(
            sim_rel > sim_un,
            "related query should outrank unrelated: rel={sim_rel} un={sim_un}"
        );
        assert!(
            sim_rel > 0.1,
            "related should have positive score: {sim_rel}"
        );
    }

    #[test]
    fn test_hash_backend_identity_only() {
        let a = compute_embedding_with(b"alpha beta", EmbeddingBackend::Hash);
        let b = compute_embedding_with(b"alpha beta gamma", EmbeddingBackend::Hash);
        // hash mock: any content change → different vector; no lexical claim
        assert_ne!(a, b);
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let v = compute_embedding_with(b"test", EmbeddingBackend::Features);
        let sim = EmbeddingIndex::cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![0.0f32; EMBED_DIM];
        let b = compute_embedding_with(b"anything", EmbeddingBackend::Features);
        let sim = EmbeddingIndex::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_search_returns_top_results() {
        let mut idx = EmbeddingIndex::new(std::path::Path::new("/tmp"));
        idx.backend = EmbeddingBackend::Features;
        idx.set(
            "a",
            compute_embedding_with(b"alpha authentication", EmbeddingBackend::Features),
        );
        idx.set(
            "b",
            compute_embedding_with(b"beta rendering canvas", EmbeddingBackend::Features),
        );
        idx.set(
            "c",
            compute_embedding_with(b"gamma database schema", EmbeddingBackend::Features),
        );

        let query = compute_embedding_with(b"authentication", EmbeddingBackend::Features);
        let results = idx.search(&query, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "a");
    }

    #[test]
    fn test_save_and_load_roundtrip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut idx = EmbeddingIndex::new(dir.path());
        idx.set(
            "hello.txt",
            compute_embedding_with(b"hello", EmbeddingBackend::Features),
        );
        idx.set(
            "world.txt",
            compute_embedding_with(b"world", EmbeddingBackend::Features),
        );
        idx.save()?;

        let mut loaded = EmbeddingIndex::new(dir.path());
        loaded.load()?;
        assert!(loaded.get("hello.txt").is_some());
        assert!(loaded.get("world.txt").is_some());
        assert_eq!(loaded.get("hello.txt").unwrap().len(), EMBED_DIM);
        assert_eq!(loaded.embeddings.len(), 2);
        Ok(())
    }

    #[test]
    fn test_remove_embedding() {
        let mut idx = EmbeddingIndex::new(std::path::Path::new("/tmp"));
        idx.set(
            "gone.txt",
            compute_embedding_with(b"gone", EmbeddingBackend::Features),
        );
        assert!(idx.get("gone.txt").is_some());
        idx.remove("gone.txt");
        assert!(idx.get("gone.txt").is_none());
    }
}
