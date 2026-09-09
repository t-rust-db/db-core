//! Zero-dependency-on-Arrow Parquet reader: parses the Thrift-encoded
//! footer and reads column chunks directly from bytes handed to it by a
//! [`crate::column::vfs::Vfs`]. Folded in from the standalone `db-parquet`
//! repo (`db-storage#4`) as part of `column` module consolidation.

#![forbid(unsafe_code)]

pub mod compression;
pub mod decimal;
pub mod encoding;
pub mod footer;
pub mod nested;
pub mod page;
pub mod parquet_file;
pub mod reader;
pub mod schema_tree;
pub mod thrift;

pub use parquet_file::{DictionaryIndices, FileError, ParquetFile, RowGroupReader};
