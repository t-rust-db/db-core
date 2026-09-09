//! Stream-oriented storage: log parsing and columnar batching for push-driven
//! sources (files, Docker, journald, K8s). Counterpart to `column` (Parquet,
//! bounded) and `row` (SQLite B-tree, transactional).
//!
//! # Architecture
//!
//! ```text
//! Source (mmap/stdin/docker)
//!     │
//!     ▼
//! LineIterator ──► Parser (syslog/json/logfmt)
//!                     │
//!                     ▼
//!               LogBatch<'a>  ──► vm::stream::StreamFilter
//!                     │                   │
//!                     ▼                   ▼
//!              fields as columns      BitVec (matches)
//! ```
//!
//! # Design
//!
//! - **Tier 1 (raw):** zero-copy `&[u8]` slices into mmap
//! - **Tier 2 (predefined):** typed columns for universal fields
//!   (timestamp, severity, message, trace_id, span_id)
//! - **Tier 3 (open):** schema-on-read `FieldStore` with dictionary
//!   encoding for low-cardinality fields
//!
//! Batch-level `Source` and `Resource` avoid per-row duplication for
//! metadata constant across a file/container.

pub mod batch;
pub mod syslog;

pub use batch::{
    Facility, FieldColumn, FieldStore, LogBatch, Resource, Severity, Source, SourceKind,
};
pub use syslog::SyslogParser;
