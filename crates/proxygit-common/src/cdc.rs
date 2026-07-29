use anyhow::Result;
use blake3::Hash;

/// Result of chunking a single block from a file
#[derive(Debug, Clone)]
pub struct ChunkResult {
    pub offset: u64,
    pub length: usize,
    pub hash: Hash,
    pub data: Vec<u8>,
}

/// Content-Defined Chunking using FastCDC algorithm.
///
/// Splits data at content-defined boundaries (not fixed offsets),
/// so inserting a byte in the middle only shifts boundaries locally.
pub struct CdcChunker {
    min_size: usize,
    avg_size: usize,
    max_size: usize,
}

impl CdcChunker {
    pub fn default_config() -> Self {
        Self {
            min_size: 4096,  // 4 KB
            avg_size: 16384, // 16 KB
            max_size: 65536, // 64 KB
        }
    }

    /// Create a chunker with custom size bounds
    pub fn new(min: usize, avg: usize, max: usize) -> Self {
        Self {
            min_size: min,
            avg_size: avg,
            max_size: max,
        }
    }

    /// Chunk input data using FastCDC algorithm and calculate BLAKE3 hashes.
    ///
    /// The algorithm scans for a boundary by looking at the lowest 4 bits
    /// of each byte. When they equal zero, a chunk boundary is declared
    /// (normalized to be at least `min_size` and at most `max_size`).
    pub fn process_buffer(&self, data: &[u8]) -> Result<Vec<ChunkResult>> {
        let mut chunks = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            let remaining = data.len() - offset;
            let chunk_len = if remaining <= self.min_size {
                remaining
            } else {
                let limit = remaining.min(self.max_size);
                let mut cut = self.min_size;

                // FastCDC boundary scan: find a byte whose low nibble is zero
                for i in self.min_size..limit {
                    if (data[offset + i] & 0x0F) == 0 {
                        cut = i;
                        break;
                    }
                }

                // If no boundary found in the window, use the max
                if cut == self.min_size {
                    limit
                } else {
                    cut
                }
            };

            let chunk_bytes = &data[offset..offset + chunk_len];
            let hash = blake3::hash(chunk_bytes);

            chunks.push(ChunkResult {
                offset: offset as u64,
                length: chunk_len,
                hash,
                data: chunk_bytes.to_vec(),
            });

            offset += chunk_len;
        }

        Ok(chunks)
    }

    /// Convenience: chunk from a byte slice, return just the hashes + offsets (no data copy)
    pub fn fingerprint(&self, data: &[u8]) -> Result<Vec<(u64, usize, Hash)>> {
        let chunks = self.process_buffer(data)?;
        Ok(chunks
            .into_iter()
            .map(|c| (c.offset, c.length, c.hash))
            .collect())
    }

    /// Reconstruct original data from chunks (must be in order, no gaps)
    pub fn reconstruct(chunks: &[ChunkResult]) -> Vec<u8> {
        let total: usize = chunks.iter().map(|c| c.length).sum();
        let mut out = Vec::with_capacity(total);
        for c in chunks {
            out.extend_from_slice(&c.data);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunker_basic() {
        let chunker = CdcChunker::default_config();
        let dummy_data = vec![0u8; 50000];
        let results = chunker.process_buffer(&dummy_data).unwrap();

        assert!(!results.is_empty());
        let total_bytes: usize = results.iter().map(|c| c.length).sum();
        assert_eq!(total_bytes, 50000);
    }

    #[test]
    fn test_chunker_small_data() {
        let chunker = CdcChunker::default_config();
        let data = b"hello world";
        let results = chunker.process_buffer(data).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].data, data);
    }

    #[test]
    fn test_chunker_roundtrip() {
        let chunker = CdcChunker::default_config();
        // Create data with varying bytes so CDC finds natural boundaries
        let data: Vec<u8> = (0..20000).map(|i| (i % 256) as u8).collect();
        let results = chunker.process_buffer(&data).unwrap();
        let reconstructed = CdcChunker::reconstruct(&results);
        assert_eq!(data, reconstructed);
    }

    #[test]
    fn test_fingerprint_no_data() {
        let chunker = CdcChunker::default_config();
        let data = vec![0u8; 100000];
        let fps = chunker.fingerprint(&data).unwrap();
        assert!(!fps.is_empty());
        let total_len: usize = fps.iter().map(|(_, l, _)| l).sum();
        assert_eq!(total_len, 100000);
    }
    #[test]
    fn test_chunker_empty_buffer() {
        let chunker = CdcChunker::default_config();
        let empty_data: Vec<u8> = Vec::new();
        let results = chunker.process_buffer(&empty_data).unwrap();
        assert!(results.is_empty());
        let reconstructed = CdcChunker::reconstruct(&results);
        assert!(reconstructed.is_empty());
    }
}
