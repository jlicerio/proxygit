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

/// Maximum payload size per frame (64 MB) to prevent OOM DoS
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;

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
}
