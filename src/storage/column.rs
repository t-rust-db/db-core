//! Column-oriented storage: the mmap-based `Vfs`/`VfsFile` pair plus the
//! Parquet reader that consumes it. Folded together into one module
//! (`db-storage#4`) so `column-rs` depends on a single crate instead of
//! two separately-versioned ones.

#[cfg(feature = "vm-batch")]
pub mod decode;
pub mod memory;
pub mod mmap;
pub mod posix;
pub mod vfs;

#[cfg(feature = "vm-batch")]
pub use decode::{
    annotation, decode_column_at, decode_column_full, decode_column_full_bit_pattern, Annotation,
    Decoded,
};
pub use memory::MemoryVfs;
pub use posix::PosixVfs;
pub use vfs::{MmapRegion, Vfs, VfsFile};

/// The Parquet reader's former home, kept so `storage::column::parquet::*`
/// paths still resolve: it moved to [`crate::storage::parquet`] (#566).
pub use crate::storage::parquet;
pub use crate::storage::parquet::{DictionaryIndices, FileError, ParquetFile, RowGroupReader};
