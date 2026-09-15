//! Page-level decompression for the codecs Parquet writers actually use:
//! a hand-rolled Snappy decompressor and ZSTD via the pure-Rust `ruzstd`
//! crate (see `zstd.rs` for why ZSTD isn't hand-rolled too).
//! `Codec::Uncompressed` is a zero-copy passthrough.

mod snappy;
mod zstd;

pub use snappy::SnappyError;
pub use zstd::ZstdError;

use crate::storage::column::parquet::footer::Codec;
use std::borrow::Cow;
use std::fmt;

#[derive(Debug)]
pub enum CompressionError {
    UnsupportedCodec(i32),
    Snappy(SnappyError),
    Zstd(ZstdError),
}

impl fmt::Display for CompressionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompressionError::UnsupportedCodec(c) => {
                write!(f, "unsupported compression codec: {c}")
            }
            CompressionError::Snappy(e) => write!(f, "{e}"),
            CompressionError::Zstd(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CompressionError {}

impl From<SnappyError> for CompressionError {
    fn from(e: SnappyError) -> Self {
        CompressionError::Snappy(e)
    }
}

impl From<ZstdError> for CompressionError {
    fn from(e: ZstdError) -> Self {
        CompressionError::Zstd(e)
    }
}

pub type Result<T> = std::result::Result<T, CompressionError>;

/// Decompress one page body. `uncompressed_size` is the page header's
/// declared uncompressed size, used to size the output buffer and validated
/// against the actual decompressed length.
pub fn decompress<'a>(
    codec: Codec,
    compressed: &'a [u8],
    uncompressed_size: usize,
) -> Result<Cow<'a, [u8]>> {
    match codec {
        Codec::Uncompressed => Ok(Cow::Borrowed(compressed)),
        Codec::Snappy => Ok(Cow::Owned(snappy::decompress(
            compressed,
            uncompressed_size,
        )?)),
        Codec::Zstd => Ok(Cow::Owned(zstd::decompress(compressed, uncompressed_size)?)),
        Codec::Other(c) => Err(CompressionError::UnsupportedCodec(c)),
    }
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
    fn uncompressed_is_zero_copy_passthrough() {
        let data = [1u8, 2, 3];
        let out = decompress(Codec::Uncompressed, &data, 3).unwrap();
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(&*out, &data);
    }

    #[test]
    fn other_codec_is_unsupported() {
        let err = decompress(Codec::Other(7), &[], 0).unwrap_err();
        assert!(matches!(err, CompressionError::UnsupportedCodec(7)));
        assert_eq!(err.to_string(), "unsupported compression codec: 7");
    }

    #[test]
    fn snappy_codec_dispatches_to_the_snappy_decoder() {
        // A raw Snappy block: declared length 5, one literal "hello".
        let mut compressed = vec![5u8];
        compressed.push(4u8 << 2);
        compressed.extend_from_slice(b"hello");
        let out = decompress(Codec::Snappy, &compressed, 5).unwrap();
        assert_eq!(&*out, b"hello");
    }

    #[test]
    fn a_snappy_error_wraps_into_a_compression_error() {
        // Offset-0 copy: `SnappyError::InvalidCopyOffset`, converted via
        // `From<SnappyError>` and displayed through `CompressionError`'s
        // own `Snappy` arm.
        let compressed = vec![4u8, 0x01u8, 0];
        let err = decompress(Codec::Snappy, &compressed, 4).unwrap_err();
        assert!(matches!(err, CompressionError::Snappy(_)));
        assert_eq!(
            err.to_string(),
            "snappy copy references data before the start of the buffer"
        );
    }

    #[test]
    fn zstd_codec_dispatches_to_the_zstd_decoder() {
        let text = b"zstd zstd zstd dispatch dispatch dispatch";
        let compressed = zstd_encode_for_test(text);
        let out = decompress(Codec::Zstd, &compressed, text.len()).unwrap();
        assert_eq!(&*out, text);
    }

    #[test]
    fn a_zstd_error_wraps_into_a_compression_error() {
        // Not a valid ZSTD frame at all: `ZstdError::Frame`, converted
        // via `From<ZstdError>` and displayed through `CompressionError`'s
        // own `Zstd` arm.
        let err = decompress(Codec::Zstd, &[0, 1, 2, 3], 10).unwrap_err();
        assert!(matches!(err, CompressionError::Zstd(_)));
        assert!(err.to_string().starts_with("zstd frame error:"), "{err}");
    }

    /// Shells out to the system `zstd` CLI to produce a real compressed
    /// frame, mirroring `zstd.rs`'s own test helper.
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
