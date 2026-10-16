//! Record ingestion into column tables (ADR 0027-0029, epic #570).
//!
//! This first part is durability (#571): [`Wal`] assigns LSNs and
//! group-commits framed records to `<table>/wal/`, tracking the
//! `visible`/`durable`/`flushed` watermarks; [`TableLock`] keeps a table to
//! one writer; replay on open recovers everything up to a torn tail and
//! refuses to drop data past real corruption. #572 adds pages, the flush
//! to Parquet under an atomically replaced manifest ([`TableWriter`]), and
//! [`Snapshot`], the read view over both; the `Table` API is #574.

// Read side (`ingest-read`): what a reader of a table directory needs.
mod crc32c;
pub mod error;
mod fields;
mod frame;
pub mod layout;
pub mod manifest;
pub mod page;
pub mod record;
mod recover;
pub mod snapshot;
pub mod wal;

// Write side (`ingest`): the Parquet flush, tiers, the lock and the writer.
#[cfg(feature = "ingest")]
mod flush;
#[cfg(feature = "ingest")]
pub mod lock;
#[cfg(feature = "ingest")]
pub mod table;
#[cfg(feature = "ingest")]
mod tier;

pub use error::IngestError;
pub use layout::{Area, Layout, LayoutKind, TierOptions};
#[cfg(feature = "ingest")]
pub use lock::{LockError, TableLock, LOCK_FILE};
pub use manifest::{FileEntry, Manifest, ManifestError, MANIFEST_FILE};
pub use page::{Page, PageBody};
pub use record::{RecordValue, RowPage};
pub use snapshot::Snapshot;
#[cfg(feature = "ingest")]
pub use table::{TableOptions, TableWriter, DATA_DIR, TMP_DIR};
#[cfg(feature = "ingest")]
pub use wal::Wal;
pub use wal::{Ack, Record, WalError, WalOptions, Watermarks, WAL_DIR};
