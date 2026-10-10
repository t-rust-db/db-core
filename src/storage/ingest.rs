//! Record ingestion into column tables (ADR 0027-0029, epic #570).
//!
//! This first part is durability (#571): [`Wal`] assigns LSNs and
//! group-commits framed records to `<table>/wal/`, tracking the
//! `visible`/`durable`/`flushed` watermarks; [`TableLock`] keeps a table to
//! one writer; replay on open recovers everything up to a torn tail and
//! refuses to drop data past real corruption. Pages, flush to Parquet and
//! the manifest are #572; the `Table` API is #574.

mod crc32c;
mod frame;
pub mod lock;
mod recover;
pub mod wal;

pub use lock::{LockError, TableLock, LOCK_FILE};
pub use wal::{Ack, Record, Wal, WalError, WalOptions, Watermarks, WAL_DIR};
