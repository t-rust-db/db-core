//! Zero-dependency-on-Arrow Parquet reader: parses the Thrift-encoded
//! footer and reads column chunks directly from bytes handed to it by a
//! [`crate::storage::column::vfs::Vfs`]. Folded in from the standalone `db-parquet`
//! repo (`db-storage#4`); moved out of `storage::column` to sit beside its
//! writer (#566).

#![forbid(unsafe_code)]

// The reader predates the crate-wide lint bar (#9): these three lints are
// allowed per reader module (moved from `storage::column`, where they were
// module-wide) until the burn-down lands. Per module, not here at the
// top, so new modules in `parquet` (the writer, #566) get the full bar.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod compression;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod decimal;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod encoding;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod footer;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod nested;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod page;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod parquet_file;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod reader;
pub mod schema;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod schema_tree;
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    missing_docs,
    reason = "reader lint burn-down, see storage::parquet"
)]
pub mod thrift;
#[cfg(feature = "storage-column-write")]
pub mod writer;

pub use parquet_file::{DictionaryIndices, Field, FileError, ParquetFile, RowGroupReader};
