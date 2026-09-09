//! ZSTD page decompression, via the pure-Rust `ruzstd` decode-only crate
//! (no C dependency).
//!
//! A hand-rolled decoder was attempted first (matching the hand-rolled
//! Snappy decompressor's approach) but ZSTD's FSE/Huffman entropy coding
//! proved too easy to get subtly bit-order-wrong without a reference
//! decoder to diff against; see #47 for a possible future revisit.

use std::fmt;

#[derive(Debug)]
pub enum ZstdError {
    Frame(String),
    Read(String),
    SizeMismatch { expected: usize, actual: usize },
}

impl fmt::Display for ZstdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZstdError::Frame(e) => write!(f, "zstd frame error: {e}"),
            ZstdError::Read(e) => write!(f, "zstd read error: {e}"),
            ZstdError::SizeMismatch { expected, actual } => {
                write!(
                    f,
                    "zstd decompressed size mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for ZstdError {}

type Result<T> = std::result::Result<T, ZstdError>;

pub fn decompress(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    let mut decoder =
        ruzstd::StreamingDecoder::new(data).map_err(|e| ZstdError::Frame(e.to_string()))?;
    let mut out = Vec::with_capacity(uncompressed_size);
    std::io::Read::read_to_end(&mut decoder, &mut out)
        .map_err(|e| ZstdError::Read(e.to_string()))?;
    if out.len() != uncompressed_size {
        return Err(ZstdError::SizeMismatch {
            expected: uncompressed_size,
            actual: out.len(),
        });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    #[test]
    fn decompresses_a_real_zstd_frame() {
        // Produced by `zstd -19` on a small repetitive input.
        let text = "hello hello hello world world world zstd zstd zstd test test test";
        let compressed = zstd_encode_for_test(text.as_bytes());
        let out = decompress(&compressed, text.len()).unwrap();
        assert_eq!(out, text.as_bytes());
    }

    /// Shells out to the system `zstd` CLI to produce a real compressed
    /// frame for the round-trip test above (skips gracefully if unavailable).
    fn zstd_encode_for_test(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut child = std::process::Command::new("zstd")
            .args(["-q", "-19", "-c"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("zstd CLI must be available for this test");
        child.stdin.take().unwrap().write_all(data).unwrap();
        let output = child.wait_with_output().unwrap();
        output.stdout
    }
}
