//! Record ingestion into column tables (ADR 0027-0029, epic #570).
//!
//! This first part is durability (#571): [`Wal`] assigns LSNs and
//! group-commits framed records to `<table>/wal/`, tracking the
//! `visible`/`durable`/`flushed` watermarks; [`TableLock`] keeps a table to
//! one writer; replay on open recovers everything up to a torn tail and
//! refuses to drop data past real corruption. #572 adds pages, the flush
//! to Parquet under an atomically replaced manifest ([`TableWriter`]), and
//! [`Snapshot`], the read view over both; the `Table` API is #574.

mod crc32c;
mod flush;
mod frame;
pub mod layout;
pub mod lock;
pub mod manifest;
pub mod page;
pub mod record;
mod recover;
pub mod snapshot;
pub mod table;
mod tier;
pub mod wal;

pub use layout::{Area, Layout, LayoutKind, TierOptions};
pub use lock::{LockError, TableLock, LOCK_FILE};
pub use manifest::{FileEntry, Manifest, ManifestError, MANIFEST_FILE};
pub use page::{Page, PageBody};
pub use record::{RecordValue, RowPage};
pub use snapshot::Snapshot;
pub use table::{IngestError, TableOptions, TableWriter, DATA_DIR, TMP_DIR};
pub use wal::{Ack, Record, Wal, WalError, WalOptions, Watermarks, WAL_DIR};
