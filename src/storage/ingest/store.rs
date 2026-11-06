//! Where a table's objects live (#1015).
//!
//! A table is a set of objects addressed by `/`-separated keys relative to
//! the table root (`manifest.json`, `data/<file>.parquet`, ...).
//! [`TableStore`] is the narrow surface the ingest code needs from the
//! place those objects are kept, so a backend without rename, append,
//! fsync or advisory locks (S3) can stand in for the local filesystem.
//! [`LocalFs`] is the default and behaves exactly as the direct `std::fs`
//! calls it replaces.

use std::io;

use crate::storage::column::MmapRegion;

mod local;

pub use local::LocalFs;

/// Keeps a table's objects. Keys are `/`-separated and relative to the
/// table root; an absolute key or one with a `..` component is
/// [`io::ErrorKind::InvalidInput`].
pub trait TableStore: Send + Sync {
    /// The whole object, or `None` if there is none.
    fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>>;

    /// The whole object as a read-only region. A missing object is
    /// [`io::ErrorKind::NotFound`], which readers take as "replaced, retry".
    fn map(&self, key: &str) -> io::Result<MmapRegion>;

    /// Atomically replaces the object with `bytes`, durably: after `Ok`, a
    /// crash leaves either the old or the new object, never a mix, and
    /// never loses the new one. This is the manifest's commit point.
    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()>;

    /// Removes the object; a missing one is [`io::ErrorKind::NotFound`].
    fn delete(&self, key: &str) -> io::Result<()>;

    /// The keys under `prefix` (a directory-like prefix such as `data/`,
    /// or empty for all), sorted.
    fn list(&self, prefix: &str) -> io::Result<Vec<String>>;
}
