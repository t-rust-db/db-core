//! Record ingestion into column tables (ADR 0027-0029, epic #570).
//!
//! This first part is durability (#571): [`Wal`] assigns LSNs and
//! group-commits framed records to `<table>/wal/`, tracking the
//! `visible`/`durable`/`flushed` watermarks; [`TableLock`] keeps a table to
//! one writer; replay on open recovers everything up to a torn tail and
//! refuses to drop data past real corruption. Pages, flush to Parquet and
//! the manifest are #572; the `Table` API is #574.

mod crc32c;
mod flush;
mod frame;
pub mod lock;
pub mod manifest;
pub mod page;
mod recover;
pub mod table;
pub mod wal;

pub use lock::{LockError, TableLock, LOCK_FILE};
pub use manifest::{FileEntry, Manifest, ManifestError, MANIFEST_FILE};
pub use page::Page;
pub use table::{IngestError, TableOptions, TableWriter, DATA_DIR, TMP_DIR};
pub use wal::{Ack, Record, Wal, WalError, WalOptions, Watermarks, WAL_DIR};
