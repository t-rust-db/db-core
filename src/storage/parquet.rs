//! Zero-dependency-on-Arrow Parquet reader: parses the Thrift-encoded
//! footer and reads column chunks directly from bytes handed to it by a
//! [`crate::storage::column::vfs::Vfs`]. Folded in from the standalone `db-parquet`
//! repo (`db-storage#4`); moved out of `storage::column` to sit beside its
//! writer (#566).

#![forbid(unsafe_code)]

// Every module meets the crate-wide lint bar (#9, burned down in #657):
// file bytes are untrusted, so corrupt input returns an error, never a
// panic (`tests/unit/parquet_file_corruption_test.rs`).
pub mod compression;
pub mod decimal;
pub mod encoding;
pub mod footer;
pub mod nested;
pub mod page;
pub mod parquet_file;
pub mod reader;
pub mod schema;
pub mod schema_tree;
pub mod thrift;
#[cfg(feature = "storage-column-write")]
pub mod writer;

pub use parquet_file::{DictionaryIndices, Field, FileError, ParquetFile, RowGroupReader};
