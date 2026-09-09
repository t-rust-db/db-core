//! Page-level decompression for the codecs Parquet writers actually use:
//! a hand-rolled Snappy decompressor and ZSTD via the pure-Rust `ruzstd`
//! crate (see `zstd.rs` for why ZSTD isn't hand-rolled too).
//! `Codec::Uncompressed` is a zero-copy passthrough.

mod snappy;
mod zstd;

pub use snappy::SnappyError;
pub use zstd::ZstdError;

use crate::column::parquet::footer::Codec;
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
}
