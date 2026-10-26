//! Record ingestion into column tables (ADR 0027-0029, epic #570).
//!
//! Durability (#571): the WAL assigns LSNs and group-commits framed
//! records to `<table>/wal/`, tracking the `visible`/`durable`/`flushed`
//! watermarks; a `LOCK` file keeps a table to one writer; replay on open
//! recovers everything up to a torn tail and refuses to drop data past real
//! corruption. #572 adds pages, the flush to Parquet under an atomically
//! replaced manifest (`TableWriter`), and [`Snapshot`], the read view over
//! both; #573 the time-tiered layout; #574 the public `Table` handle and
//! its line sources.
//!
//! ADR 0029 splits this module in two: the `ingest-read` feature is what a
//! reader of a table directory needs (manifest, pages, the WAL tail,
//! [`Snapshot`]), with no Parquet or WAL writer; `ingest` adds the write
//! side.

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
pub mod handle;
#[cfg(feature = "ingest")]
pub mod lock;
#[cfg(feature = "ingest")]
pub mod table;
#[cfg(feature = "ingest")]
mod tier;

pub use error::IngestError;
#[cfg(feature = "ingest")]
pub use handle::{Table, FOLLOW_DIR};
pub use layout::{Area, Layout, LayoutKind, SessionKey, TierOptions};
#[cfg(feature = "ingest")]
pub use lock::{LockError, TableLock, LOCK_FILE};
pub use manifest::{FileEntry, KeyBound, KeyRange, Manifest, ManifestError, MANIFEST_FILE};
pub use page::{Page, PageBody};
pub use record::{RecordValue, RowPage};
pub use snapshot::{ReaderStats, Snapshot, TableReader};
#[cfg(feature = "ingest")]
pub use table::{TableOptions, TableWriter, DATA_DIR, TMP_DIR};
#[cfg(feature = "ingest")]
pub use wal::Wal;
pub use wal::{Ack, Record, WalError, WalOptions, Watermarks, WAL_DIR};
