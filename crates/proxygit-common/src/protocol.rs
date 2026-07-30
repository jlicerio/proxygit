use anyhow::{bail, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ── QUIC Message Types ──────────────────────────────────────────────

pub const MSG_LIST_PROJECT: u8 = 0x01;
pub const MSG_LIST_PROJECT_RESP: u8 = 0x02;
pub const MSG_READ_FILE: u8 = 0x03;
pub const MSG_READ_FILE_RESP: u8 = 0x04;
pub const MSG_WRITE_BLOCKS: u8 = 0x05;
pub const MSG_WRITE_ACK: u8 = 0x06;
pub const MSG_STAT_FILE: u8 = 0x07;
pub const MSG_STAT_FILE_RESP: u8 = 0x08;
pub const MSG_EVENT_SUBSCRIBE: u8 = 0x09;
pub const MSG_FILE_CHANGED: u8 = 0x0A;
pub const MSG_BLOCK_REQUEST: u8 = 0x0B;
pub const MSG_BLOCK_RESP: u8 = 0x0C;
pub const MSG_ERROR: u8 = 0x0D;
pub const MSG_GET_PROJECT_MAP: u8 = 0x0E;
pub const MSG_GET_PROJECT_MAP_RESP: u8 = 0x0F;
pub const MSG_SEMANTIC_SEARCH: u8 = 0x10;
pub const MSG_SEMANTIC_SEARCH_RESP: u8 = 0x11;
pub const MSG_CREATE_BACKUP: u8 = 0x12;
pub const MSG_LIST_BACKUPS: u8 = 0x13;
pub const MSG_WRITE_BLOCKS_SPARSE: u8 = 0x14;
pub const MSG_HAS_BLOCKS: u8 = 0x15;
pub const MSG_HAS_BLOCKS_RESP: u8 = 0x16;

/// Maximum payload size per frame (64 MB) to prevent OOM DoS
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;

/// Log wire bytes for bench instrumentation.
/// Greppable as `[wire] direction type=0xNN payload=N wire=N`.
pub fn log_wire_bytes(direction: &str, msg_type: u8, msg_name: &str, payload_len: usize) {
    let wire_bytes = 53 + payload_len;
    tracing::info!(
        target: "proxygit::wire",
        direction, msg_type = format_args!("0x{msg_type:02x}"), msg_name,
        payload_len, wire = wire_bytes,
        "[wire] {direction} type={msg_type:#04x} ({msg_name}) payload={payload_len} wire={wire_bytes}",
    );
}

/// Human-readable name for a protocol message type.
pub fn msg_type_name(msg_type: u8) -> &'static str {
    match msg_type {
        MSG_LIST_PROJECT => "LIST_PROJECT",
        MSG_LIST_PROJECT_RESP => "LIST_PROJECT_RESP",
        MSG_READ_FILE => "READ_FILE",
        MSG_READ_FILE_RESP => "READ_FILE_RESP",
        MSG_WRITE_BLOCKS => "WRITE_BLOCKS",
        MSG_WRITE_ACK => "WRITE_ACK",
        MSG_STAT_FILE => "STAT_FILE",
        MSG_STAT_FILE_RESP => "STAT_FILE_RESP",
        MSG_BLOCK_REQUEST => "BLOCK_REQUEST",
        MSG_BLOCK_RESP => "BLOCK_RESP",
        MSG_ERROR => "ERROR",
        MSG_GET_PROJECT_MAP => "GET_PROJECT_MAP",
        MSG_GET_PROJECT_MAP_RESP => "GET_PROJECT_MAP_RESP",
        MSG_SEMANTIC_SEARCH => "SEMANTIC_SEARCH",
        MSG_SEMANTIC_SEARCH_RESP => "SEMANTIC_SEARCH_RESP",
        MSG_CREATE_BACKUP => "CREATE_BACKUP",
        MSG_LIST_BACKUPS => "LIST_BACKUPS",
        MSG_WRITE_BLOCKS_SPARSE => "WRITE_BLOCKS_SPARSE",
        MSG_HAS_BLOCKS => "HAS_BLOCKS",
        MSG_HAS_BLOCKS_RESP => "HAS_BLOCKS_RESP",
        _ => "UNKNOWN",
    }
}

/// A decoded QUIC protocol frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub msg_type: u8,
    pub project_id: u128,
    pub payload_len: u32,
    pub hash: [u8; 32],
    pub payload: Vec<u8>,
}

/// Encode a QUIC protocol frame into bytes.
///
/// Frame layout (53B header + variable payload):
///   [0]      msg_type      (u8)
///   [1..17]  project_id    (u128, big-endian)
///   [17..21] payload_len   (u32, big-endian)
///   [21..53] blake3_hash   (32 bytes)
///   [53..]   payload       (payload_len bytes)
pub fn encode_frame(msg_type: u8, project_id: u128, hash: &[u8; 32], payload: &[u8]) -> Vec<u8> {
    let payload_len = payload.len() as u32;
    let mut buf = Vec::with_capacity(53 + payload.len());
    buf.push(msg_type);
    buf.extend_from_slice(&project_id.to_be_bytes());
    buf.extend_from_slice(&payload_len.to_be_bytes());
    buf.extend_from_slice(hash);
    buf.extend_from_slice(payload);
    buf
}

/// Decode a QUIC protocol frame from raw bytes.
pub fn decode_frame(data: &[u8]) -> Result<Frame> {
    if data.len() < 53 {
        bail!("Frame too short: {} bytes (need at least 53)", data.len());
    }
    let msg_type = data[0];
    let project_id = u128::from_be_bytes(data[1..17].try_into()?);
    let payload_len = u32::from_be_bytes(data[17..21].try_into()?);
    let hash: [u8; 32] = data[21..53].try_into()?;
    let payload_len_us = payload_len as usize;

    if payload_len_us > MAX_PAYLOAD_SIZE {
        bail!(
            "Frame payload too large: {} bytes (max {})",
            payload_len_us,
            MAX_PAYLOAD_SIZE
        );
    }
    if data.len() < 53 + payload_len_us {
        bail!(
            "Frame payload truncated: header says {} bytes, got {}",
            payload_len_us,
            data.len().saturating_sub(53)
        );
    }
    let payload = data[53..53 + payload_len_us].to_vec();
    Ok(Frame {
        msg_type,
        project_id,
        payload_len,
        hash,
        payload,
    })
}

/// Read one complete frame from an async QUIC stream.
pub async fn recv_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame> {
    // Read header (53 bytes)
    let mut header = [0u8; 53];
    reader.read_exact(&mut header).await?;

    let msg_type = header[0];
    let project_id = u128::from_be_bytes(header[1..17].try_into()?);
    let payload_len = u32::from_be_bytes(header[17..21].try_into()?);
    let hash: [u8; 32] = header[21..53].try_into()?;
    let payload_len_us = payload_len as usize;

    if payload_len_us > MAX_PAYLOAD_SIZE {
        bail!(
            "Frame payload too large: {} bytes (max {})",
            payload_len_us,
            MAX_PAYLOAD_SIZE
        );
    }

    let mut payload = vec![0u8; payload_len_us];
    reader.read_exact(&mut payload).await?;

    log_wire_bytes("recv", msg_type, msg_type_name(msg_type), payload_len_us);

    Ok(Frame {
        msg_type,
        project_id,
        payload_len,
        hash,
        payload,
    })
}

/// Send a complete frame to an async QUIC stream.
pub async fn send_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: u8,
    project_id: u128,
    hash: &[u8; 32],
    payload: &[u8],
) -> Result<()> {
    let bytes = encode_frame(msg_type, project_id, hash, payload);
    log_wire_bytes("send", msg_type, msg_type_name(msg_type), payload.len());
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Encode a binary-safe write payload: [path_len: u16][path][content]
pub fn encode_write_payload(path: &str, content: &[u8]) -> Vec<u8> {
    let path_bytes = path.as_bytes();
    let mut buf = Vec::with_capacity(2 + path_bytes.len() + content.len());
    buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(path_bytes);
    buf.extend_from_slice(content);
    buf
}

/// Decode a binary-safe write payload: returns (path, content)
pub fn decode_write_payload(payload: &[u8]) -> Result<(String, &[u8])> {
    if payload.len() < 2 {
        bail!("Write payload too short");
    }
    let path_len = u16::from_le_bytes(payload[0..2].try_into()?) as usize;
    if payload.len() < 2 + path_len {
        bail!(
            "Write payload truncated: path_len={path_len}, payload_len={}",
            payload.len()
        );
    }
    let path = String::from_utf8(payload[2..2 + path_len].to_vec())?;
    let content = &payload[2 + path_len..];
    Ok((path, content))
}

// ── Sparse Write (MSG_WRITE_BLOCKS_SPARSE) ────────────────────────────

/// A single chunk in a sparse write payload.
///
/// If `data` is empty, the chunk is a hash-only reference — the server
/// is expected to already have this block in its store. If `data` is
/// non-empty, it carries the raw bytes for this chunk.
#[derive(Debug, Clone)]
pub struct SparseChunk {
    pub hash: [u8; 32],
    pub data: Vec<u8>,
}

/// Encode a sparse write payload.
///
/// Wire format:
///   [path_len:u16][path][chunk_count:u32][(hash:32B)(data_len:u32)(data)...]
pub fn encode_sparse_write(path: &str, chunks: &[SparseChunk]) -> Vec<u8> {
    let path_bytes = path.as_bytes();
    let chunks_data_len: usize = chunks.iter().map(|c| 32 + 4 + c.data.len()).sum();
    let mut buf = Vec::with_capacity(2 + path_bytes.len() + 4 + chunks_data_len);
    buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(path_bytes);
    buf.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for chunk in chunks {
        buf.extend_from_slice(&chunk.hash);
        buf.extend_from_slice(&(chunk.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&chunk.data);
    }
    buf
}

/// Decode a sparse write payload: returns (path, chunks)
pub fn decode_sparse_write(payload: &[u8]) -> Result<(String, Vec<SparseChunk>)> {
    if payload.len() < 2 {
        bail!("Sparse write payload too short");
    }
    let path_len = u16::from_le_bytes(payload[0..2].try_into()?) as usize;
    if payload.len() < 2 + path_len + 4 {
        bail!("Sparse write payload too short for path + chunk count");
    }
    let path = String::from_utf8(payload[2..2 + path_len].to_vec())?;
    let mut offset = 2 + path_len;
    let chunk_count = u32::from_le_bytes(payload[offset..offset + 4].try_into()?) as usize;
    offset += 4;

    let mut chunks = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        if offset + 32 + 4 > payload.len() {
            bail!("Sparse write payload truncated at chunk {}", chunks.len());
        }
        let hash: [u8; 32] = payload[offset..offset + 32].try_into()?;
        offset += 32;
        let data_len = u32::from_le_bytes(payload[offset..offset + 4].try_into()?) as usize;
        offset += 4;
        if data_len > MAX_PAYLOAD_SIZE {
            bail!("Chunk data too large: {data_len} bytes");
        }
        if offset + data_len > payload.len() {
            bail!("Sparse write payload truncated at chunk {}", chunks.len());
        }
        let data = payload[offset..offset + data_len].to_vec();
        offset += data_len;
        chunks.push(SparseChunk { hash, data });
    }

    Ok((path, chunks))
}

// ── Hash List (HAS_BLOCKS handshake) ─────────────────────────────────

/// Encode a list of hashes for HAS_BLOCKS query: [count:u32][hash:32B...]
pub fn encode_hash_list(hashes: &[[u8; 32]]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + hashes.len() * 32);
    buf.extend_from_slice(&(hashes.len() as u32).to_le_bytes());
    for h in hashes {
        buf.extend_from_slice(h);
    }
    buf
}

/// Decode a hash list. Returns (count, list_of_hashes)
pub fn decode_hash_list(data: &[u8]) -> Result<(u32, Vec<[u8; 32]>)> {
    if data.len() < 4 {
        bail!("Hash list too short: {} bytes", data.len());
    }
    let count = u32::from_le_bytes(data[0..4].try_into()?);
    let count_us = count as usize;
    let expected = 4 + count_us * 32;
    if data.len() < expected {
        bail!(
            "Hash list truncated: expected {expected} bytes, got {}",
            data.len()
        );
    }
    let mut hashes = Vec::with_capacity(count_us);
    for i in 0..count_us {
        let start = 4 + i * 32;
        let end = start + 32;
        let hash: [u8; 32] = data[start..end].try_into()?;
        hashes.push(hash);
    }
    Ok((count, hashes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_encode_decode_roundtrip() {
        let project_id: u128 = 0x1234567890abcdef;
        let hash = blake3::hash(b"test").into();
        let payload = b"hello proxygit".to_vec();

        let encoded = encode_frame(MSG_READ_FILE, project_id, &hash, &payload);
        let frame = decode_frame(&encoded).unwrap();

        assert_eq!(frame.msg_type, MSG_READ_FILE);
        assert_eq!(frame.project_id, project_id);
        assert_eq!(frame.hash, hash);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn test_decode_short_frame_fails() {
        let result = decode_frame(&[0x01, 0x02]);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_recv_roundtrip() {
        use tokio::io::duplex;

        let (mut a, mut b) = duplex(65536);

        let project_id: u128 = 42;
        let hash = [0u8; 32];
        let payload = b"hello from test".to_vec();
        let payload_clone = payload.clone();

        let send_task = tokio::spawn(async move {
            send_frame(&mut a, MSG_READ_FILE, project_id, &hash, &payload_clone)
                .await
                .unwrap();
        });

        let recv_task = tokio::spawn(async move { recv_frame(&mut b).await.unwrap() });

        send_task.await.unwrap();
        let frame = recv_task.await.unwrap();

        assert_eq!(frame.msg_type, MSG_READ_FILE);
        assert_eq!(frame.project_id, project_id);
        assert_eq!(frame.payload, payload);
    }
    #[test]
    fn test_binary_write_payload_roundtrip() {
        let path = "assets/icon.png";
        // Binary data with non-UTF8 bytes (0xFF, 0xFE, 0x00, 0x80) and newlines (\n)
        let binary_content: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF, 0xFE, 0x80,
        ];

        let encoded = encode_write_payload(path, &binary_content);
        let (decoded_path, decoded_content) = decode_write_payload(&encoded).unwrap();

        assert_eq!(decoded_path, path);
        assert_eq!(decoded_content, binary_content.as_slice());
    }

    #[test]
    fn test_hash_list_roundtrip() {
        let hashes: Vec<[u8; 32]> = (0..5)
            .map(|i| {
                let mut h = [0u8; 32];
                h[0] = i;
                h
            })
            .collect();

        let encoded = encode_hash_list(&hashes);
        let (count, decoded) = decode_hash_list(&encoded).unwrap();

        assert_eq!(count, 5);
        assert_eq!(decoded.len(), 5);
        for i in 0..5 {
            assert_eq!(decoded[i], hashes[i]);
        }
    }

    #[test]
    fn test_decode_hash_list_too_short_fails() {
        let result = decode_hash_list(&[0u8; 3]);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_hash_list_truncated_fails() {
        // count=2 but only 1 hash provided
        let mut data = vec![2u8, 0, 0, 0];
        data.extend_from_slice(&[0u8; 32]); // only one hash
        let result = decode_hash_list(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_hash_list_empty() {
        let encoded = encode_hash_list(&[]);
        let (count, decoded) = decode_hash_list(&encoded).unwrap();
        assert_eq!(count, 0);
        assert!(decoded.is_empty());
    }
}
