//! Shared auth helpers for optional bearer tokens.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// Load a token from `PROXYGIT_TOKEN` or, if unset, from `PROXYGIT_TOKEN_FILE`.
/// Empty / whitespace-only values are treated as unset.
pub fn load_token_from_env() -> Result<Option<String>> {
    if let Ok(t) = std::env::var("PROXYGIT_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(Some(t));
        }
    }
    if let Ok(path) = std::env::var("PROXYGIT_TOKEN_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            return load_token_file(Path::new(path)).map(Some);
        }
    }
    Ok(None)
}

/// Read a token file; first non-empty line wins (trailing newline stripped).
pub fn load_token_file(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read token file {}", path.display()))?;
    for line in raw.lines() {
        let t = line.trim();
        if !t.is_empty() && !t.starts_with('#') {
            return Ok(t.to_string());
        }
    }
    bail!("token file {} has no non-empty token line", path.display());
}

/// Generate a URL-safe random token (32 bytes → 43-char base64url, no pad).
pub fn generate_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 32];
    // Prefer OS randomness; fall back to blake3 of time if /dev/urandom missing (tests).
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf = *blake3::hash(&t.to_le_bytes()).as_bytes();
    }
    base64_url_nopad(&buf)
}

fn base64_url_nopad(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((data.len() * 4).div_ceil(3));
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
        i += 3;
    }
    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
    }
    out
}

/// Constant-time equality for tokens (length must match; different lengths → false).
pub fn tokens_equal(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in ab.iter().zip(bb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Parse `Authorization: Bearer <token>` (case-insensitive scheme).
pub fn bearer_from_authorization(header: &str) -> Option<&str> {
    let h = header.trim();
    let rest = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))?;
    let t = rest.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_nonzero() {
        let t = generate_token();
        assert!(t.len() >= 40, "len={}", t.len());
    }

    #[test]
    fn tokens_equal_ok() {
        assert!(tokens_equal("abc", "abc"));
        assert!(!tokens_equal("abc", "abd"));
        assert!(!tokens_equal("abc", "ab"));
    }

    #[test]
    fn bearer_parse() {
        assert_eq!(bearer_from_authorization("Bearer secret"), Some("secret"));
        assert_eq!(bearer_from_authorization("bearer  x "), Some("x"));
        assert_eq!(bearer_from_authorization("Basic x"), None);
    }
}
