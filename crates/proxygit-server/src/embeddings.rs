//! Server-side embedding index for semantic search over stored files.
//!
//! MVP uses a hash-based mock embedding (no ML dependencies). The embedding
//! index is persisted to a JSON file at `data_dir/embeddings.json`.
//! Production will replace `compute_embedding` with ONNX BGE-small.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Deterministic mock embedding computed from file content.
///
/// Uses BLAKE3 hash of the content to seed a deterministic expansion into 384
/// floats in [-1, 1]. Content-addressable — same content always produces the
/// same embedding; different content (almost certainly) produces a different one.
///
/// The real ONNX BGE-small model will replace this function later.
pub fn compute_embedding(content: &[u8]) -> Vec<f32> {
    let hash = blake3::hash(content);
    let bytes = hash.as_bytes();

    // Deterministic expansion: repeatedly hash to produce 384 floats
    let mut result = Vec::with_capacity(384);
    let mut seed = bytes.to_vec();

    for _ in 0..384 {
        let h = blake3::hash(&seed);
        let hb = h.as_bytes();
        // First two bytes → u16 → normalize to [-1, 1]
        let val = u16::from_le_bytes([hb[0], hb[1]]) as f32 / 32768.0 - 1.0;
        result.push(val);
        seed = hb.to_vec();
    }

    result
}

/// In-memory embedding index backed by a JSON file on disk.
pub struct EmbeddingIndex {
    /// Path to the embeddings JSON file.
    path: PathBuf,
    /// Path → embedding vector mapping.
    embeddings: HashMap<String, Vec<f32>>,
}

impl EmbeddingIndex {
    /// Create a new (empty) embedding index backed by `data_dir/embeddings.json`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("embeddings.json"),
            embeddings: HashMap::new(),
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
        let a = compute_embedding(b"hello world");
        let b = compute_embedding(b"hello world");
        assert_eq!(a.len(), 384);
        assert_eq!(a, b);
    }

    #[test]
    fn test_compute_embedding_changes_with_content() {
        let a = compute_embedding(b"hello world");
        let b = compute_embedding(b"hello world!");
        assert_ne!(a, b);
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let v = compute_embedding(b"test");
        let sim = EmbeddingIndex::cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        // Zero vector should give 0.0 similarity to anything
        let a = vec![0.0f32; 384];
        let b = compute_embedding(b"anything");
        let sim = EmbeddingIndex::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_search_returns_top_results() {
        let mut idx = EmbeddingIndex::new(std::path::Path::new("/tmp"));
        idx.set("a", compute_embedding(b"alpha"));
        idx.set("b", compute_embedding(b"beta"));
        idx.set("c", compute_embedding(b"gamma"));

        let query = compute_embedding(b"alpha");
        let results = idx.search(&query, 2);
        assert_eq!(results.len(), 2);
        // "a" should be first (closest match to "alpha")
        assert_eq!(results[0].0, "a");
    }

    #[test]
    fn test_save_and_load_roundtrip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut idx = EmbeddingIndex::new(dir.path());
        idx.set("hello.txt", compute_embedding(b"hello"));
        idx.set("world.txt", compute_embedding(b"world"));
        idx.save()?;

        let mut loaded = EmbeddingIndex::new(dir.path());
        loaded.load()?;
        assert!(loaded.get("hello.txt").is_some());
        assert!(loaded.get("world.txt").is_some());
        assert_eq!(loaded.get("hello.txt").unwrap().len(), 384);
        assert_eq!(loaded.embeddings.len(), 2);
        Ok(())
    }

    #[test]
    fn test_remove_embedding() {
        let mut idx = EmbeddingIndex::new(std::path::Path::new("/tmp"));
        idx.set("gone.txt", compute_embedding(b"gone"));
        assert!(idx.get("gone.txt").is_some());
        idx.remove("gone.txt");
        assert!(idx.get("gone.txt").is_none());
    }
}
