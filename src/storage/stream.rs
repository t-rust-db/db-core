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

// db-core#225's `cast_*` tier, scoped off for `stream` only while its
// storage layer is being built (#304/#305 own this module): two sites today
// (`file.rs`, `segment.rs`). Lift with #305, not as a drive-by from #289.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

#[cfg(feature = "vm-batch")]
pub mod adapter;
pub mod alias;
pub mod batch;
pub mod clf;
pub mod detect;
pub mod file;
pub mod json;
pub mod jsonl;
pub mod logfmt;
pub mod ring;
pub mod segment;
pub mod syslog;
pub mod timestamp;

#[cfg(feature = "vm-batch")]
pub use adapter::{ColumnRequest, StreamSegment, TailSource, PREDEFINED_COLUMNS};

pub use batch::{
    Facility, FieldColumn, FieldStore, LogBatch, Resource, Severity, Source, SourceKind,
};
pub use clf::ClfParser;
pub use detect::{detect, parse_detected, DetectedParser, Detection, Format};
pub use file::{Block, LogFile, Refresh, BLOCK_SIZE};
pub use jsonl::JsonlParser;
pub use logfmt::LogfmtParser;
pub use ring::{EvictedSummary, Ring, DEFAULT_SUMMARY_HORIZON_NS};
pub use segment::{
    ColumnSummary, MinMax, OwnedColumn, Segment, SegmentSummary, Span, SEGMENT_MAX_ROWS,
};
pub use syslog::SyslogParser;
