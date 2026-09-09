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

pub mod memory;
pub mod mmap;
pub mod parquet;
pub mod posix;
pub mod vfs;

pub use memory::MemoryVfs;
pub use posix::PosixVfs;
pub use vfs::{MmapRegion, Vfs, VfsFile};

pub use parquet::{DictionaryIndices, FileError, ParquetFile, RowGroupReader};
