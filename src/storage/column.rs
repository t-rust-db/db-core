//! Column-oriented storage: the mmap-based `Vfs`/`VfsFile` pair plus the
//! Parquet reader that consumes it. Folded together into one module
//! (`db-storage#4`) so `column-rs` depends on a single crate instead of
//! two separately-versioned ones.

// column-rs/db-parquet-derived code predates the crate-wide lint bar (#9).
// `unwrap`/`expect`/`panic` are held to it; these three are allowed at module
// scope until #15 burns them down site by site.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "column::* lint burn-down tracked in #15"
)]

#[cfg(feature = "vm-batch")]
pub mod decode;
pub mod memory;
pub mod mmap;
pub mod posix;
pub mod vfs;

// Only record tables (ingest) render bytes outside the decoder, when the
// column engine reads them.
#[cfg(all(feature = "ingest-read", feature = "engine-column"))]
pub(crate) use decode::hex;
#[cfg(feature = "vm-batch")]
pub use decode::{annotation, decode_column_at, decode_column_full, Annotation, Decoded};
pub use memory::MemoryVfs;
pub use posix::PosixVfs;
pub use vfs::{MmapRegion, Vfs, VfsFile};

/// The Parquet reader's former home, kept so `storage::column::parquet::*`
/// paths still resolve: it moved to [`crate::storage::parquet`] (#566).
pub use crate::storage::parquet;
pub use crate::storage::parquet::{DictionaryIndices, FileError, ParquetFile, RowGroupReader};
