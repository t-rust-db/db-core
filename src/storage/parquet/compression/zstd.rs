//! ZSTD page decompression (#700) and compression (#692): a decoder and
//! an encoder of our own, from RFC 8878, for the frames Parquet pages hold.
//!
//! A page is one frame whose decompressed size the page header gives, so
//! the decoder writes straight into an output buffer of that size (plus a
//! little slack for 16-byte wild copies) instead of streaming through a
//! window, as `ruzstd`'s decoder did. Spike #697 measured it at 1.8-2.5x
//! `ruzstd`'s speed on real pages, about libzstd's. The writer's encoder
//! (#692) is `encoder.rs`; `ruzstd` is only a test reference now.
//!
//! The first hand-written decoder (#47) foundered on FSE/Huffman bit order
//! with nothing to diff against; this one is checked byte for byte against
//! `ruzstd`'s decoder, and pages written by DuckDB (libzstd) and pyarrow.
//!
//! Supported: raw, RLE and compressed blocks; Huffman literals (1 or 4
//! streams, direct or FSE-coded weights, treeless); predefined, RLE,
//! FSE-coded and repeated sequence tables; repeat offsets. A frame with a
//! dictionary ID is refused: Parquet pages carry none.

mod bits;
#[cfg(test)]
mod decoder_tests;
#[cfg(any(feature = "storage-column-write", test))]
mod encoder;
mod frame;
mod fse;
mod huffman;
mod xxhash;

#[cfg(any(feature = "storage-column-write", test))]
pub(crate) use encoder::compress;

use std::fmt;

/// A ZSTD page that failed to decompress to its declared size.
#[derive(Debug)]
pub enum ZstdError {
    /// The frame header is not a valid ZSTD frame.
    Frame(String),
    /// The frame body failed to decode.
    Read(String),
    /// The frame decoded to a different length than the page declares.
    SizeMismatch {
        /// The page header's declared uncompressed size.
        expected: usize,
        /// The decoded length, read at most one byte past `expected`.
        actual: usize,
    },
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

/// The largest legitimate expansion of a frame: an RLE block, 4 bytes
/// with its header, yields up to a 128 KiB block. A declared size past
/// this many times the frame's length cannot be real, and is refused
/// before the output buffer is sized from it.
const MAX_EXPANSION: usize = 32 * 1024;

/// A corrupt frame body.
fn corrupt(why: &str) -> ZstdError {
    ZstdError::Read(why.to_string())
}

/// Decompress one ZSTD frame, which must decode to exactly
/// `uncompressed_size` bytes.
pub fn decompress(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    if uncompressed_size > data.len().saturating_mul(MAX_EXPANSION) {
        return Err(corrupt("declared size past any frame's expansion"));
    }
    frame::decode(data, uncompressed_size)
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

    #[test]
    fn garbage_bytes_are_not_a_zstd_frame() {
        let err = decompress(&[0, 1, 2, 3, 4, 5, 6, 7], 8).unwrap_err();
        assert!(matches!(err, ZstdError::Frame(_)), "{err:?}");
        assert!(err.to_string().starts_with("zstd frame error:"), "{err}");
    }

    #[test]
    fn a_truncated_frame_is_a_read_error() {
        let text = "hello hello hello world world world zstd zstd zstd test test test";
        let compressed = zstd_encode_for_test(text.as_bytes());
        // A valid frame header but a body cut short mid-block: `new()`
        // accepts the header, `read_to_end` fails partway through.
        let truncated = &compressed[..compressed.len() - 2];
        let err = decompress(truncated, text.len()).unwrap_err();
        assert!(matches!(err, ZstdError::Read(_)), "{err:?}");
        assert!(err.to_string().starts_with("zstd read error:"), "{err}");
    }

    #[test]
    fn a_wrong_declared_size_is_a_size_mismatch() {
        let text = "hello hello hello world world world zstd zstd zstd test test test";
        let compressed = zstd_encode_for_test(text.as_bytes());
        let err = decompress(&compressed, text.len() + 1).unwrap_err();
        assert!(
            matches!(err, ZstdError::SizeMismatch { expected, actual } if expected == text.len() + 1 && actual == text.len()),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("zstd decompressed size mismatch:"),
            "{err}"
        );
    }

    #[test]
    fn an_oversized_frame_stops_one_byte_past_the_declared_size() {
        let text = "hello hello hello world world world zstd zstd zstd test test test";
        let compressed = zstd_encode_for_test(text.as_bytes());
        let err = decompress(&compressed, 4).unwrap_err();
        assert!(
            matches!(
                err,
                ZstdError::SizeMismatch {
                    expected: 4,
                    actual: 5
                }
            ),
            "{err:?}"
        );
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
