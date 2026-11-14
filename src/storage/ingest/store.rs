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
use std::path::Path;

use self::lease::Lease;
use super::snapshot::HeldFile;
use crate::storage::column::posix::PosixFile;
use crate::storage::column::{MmapRegion, VfsFile};

#[cfg(test)]
pub(crate) mod conformance;
pub mod lease;
mod local;
#[cfg(test)]
pub(crate) mod memory;

#[cfg(feature = "storage-s3")]
pub mod s3;

pub use local::LocalFs;

/// Rejects a key that is absolute or has a `..`, `.` or empty component:
/// an object must stay under the table root whatever the backend.
fn check_key(key: &str) -> io::Result<()> {
    let ok = !key.is_empty()
        && key
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid object key {key:?}"),
        ))
    }
}

/// Keeps a table's objects. Keys are `/`-separated and relative to the
/// table root; an absolute key or one with a `..` component is
/// [`io::ErrorKind::InvalidInput`].
pub trait TableStore: Send + Sync {
    /// An open object, for positioned reads.
    type File: VfsFile;

    /// Opens the object for reading; a missing one is
    /// [`io::ErrorKind::NotFound`].
    fn open(&self, key: &str) -> io::Result<Self::File>;

    /// The whole object, or `None` if there is none.
    fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>>;

    /// The whole object as a read-only region. A missing object is
    /// [`io::ErrorKind::NotFound`], which readers take as "replaced, retry".
    fn map(&self, key: &str) -> io::Result<MmapRegion>;

    /// The listed object `key`, of `size` bytes (the manifest's), for a
    /// snapshot to hold while it reads it (ADR 0045). By default the whole
    /// object, [`map`](Self::map)ped; an object store reads it by range
    /// instead. A missing object is [`io::ErrorKind::NotFound`].
    fn hold(&self, key: &str, size: u64) -> io::Result<HeldFile> {
        let _ = size;
        Ok(HeldFile::mapped(self.map(key)?))
    }

    /// Atomically replaces the object with `bytes`, durably: after `Ok`, a
    /// crash leaves either the old or the new object, never a mix, and
    /// never loses the new one. This is the manifest's commit point.
    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()>;

    /// Moves the finished local file `local` into the store as `key`,
    /// durably. The file is gone from `local` after `Ok`. A crash leaves the
    /// object either absent or whole. Publishing the same bytes again is
    /// `Ok`. Different bytes under a name already taken replace the object,
    /// except in a store with conditional puts, where that name was taken by
    /// another writer and the publish fails with
    /// [`io::ErrorKind::AlreadyExists`], leaving the object as it was.
    fn publish(&self, local: &Path, key: &str) -> io::Result<()>;

    /// Removes the object. Idempotent: a missing one is `Ok`, as an object
    /// store's delete is.
    fn delete(&self, key: &str) -> io::Result<()>;

    /// Removes directories under `prefix` that hold no objects. A store
    /// with no directories (an object store) has nothing to do.
    fn remove_empty_dirs(&self, prefix: &str) -> io::Result<()>;

    /// The writer lease this store grants `owner`, if it can (it has
    /// conditional puts). `None`: a local directory protects a table with
    /// its lock file instead, and a store without conditional puts relies on
    /// the writer's local lock. [`io::ErrorKind::AlreadyExists`]: another
    /// writer holds the table.
    fn lease(&self, owner: &str) -> io::Result<Option<Lease>> {
        let _ = owner;
        Ok(None)
    }

    /// The keys under `prefix` (a directory-like prefix such as `data/`,
    /// or empty for all), sorted.
    fn list(&self, prefix: &str) -> io::Result<Vec<String>>;
}

/// Where a table's objects live: the local filesystem, or a remote store.
/// A concrete enum rather than `dyn TableStore`, so the ingest code stays
/// monomorphic.
#[derive(Debug, Clone)]
pub enum Store {
    /// A directory on the local filesystem.
    Local(LocalFs),
    /// A bucket on an S3-compatible object store.
    #[cfg(feature = "storage-s3")]
    S3(s3::S3Store),
    /// In memory, for tests.
    #[cfg(test)]
    Memory(std::sync::Arc<memory::MemoryStore>),
}

/// An object opened from a [`Store`].
pub enum StoreFile {
    /// From the local filesystem.
    Local(PosixFile),
    /// From an S3-compatible object store.
    #[cfg(feature = "storage-s3")]
    S3(s3::S3File),
    /// From memory, for tests.
    #[cfg(test)]
    Memory(crate::storage::column::memory::MemoryFile),
}

impl VfsFile for StoreFile {
    fn size(&self) -> io::Result<u64> {
        match self {
            StoreFile::Local(f) => f.size(),
            #[cfg(feature = "storage-s3")]
            StoreFile::S3(f) => f.size(),
            #[cfg(test)]
            StoreFile::Memory(f) => f.size(),
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            StoreFile::Local(f) => f.read_at(offset, buf),
            #[cfg(feature = "storage-s3")]
            StoreFile::S3(f) => f.read_at(offset, buf),
            #[cfg(test)]
            StoreFile::Memory(f) => f.read_at(offset, buf),
        }
    }

    fn mmap(&self) -> io::Result<MmapRegion> {
        match self {
            StoreFile::Local(f) => f.mmap(),
            #[cfg(feature = "storage-s3")]
            StoreFile::S3(f) => f.mmap(),
            #[cfg(test)]
            StoreFile::Memory(f) => f.mmap(),
        }
    }

    fn prefetch(&self, ranges: &[(u64, u64)]) {
        match self {
            StoreFile::Local(f) => f.prefetch(ranges),
            #[cfg(feature = "storage-s3")]
            StoreFile::S3(f) => f.prefetch(ranges),
            #[cfg(test)]
            StoreFile::Memory(f) => f.prefetch(ranges),
        }
    }
}

impl Store {
    /// The local directory `dir` as a store.
    pub fn local(dir: impl Into<std::path::PathBuf>) -> Store {
        Store::Local(LocalFs::new(dir))
    }

    /// The store at `location`: an `s3://bucket/prefix[?options]` URI (the
    /// `storage-s3` feature; see `S3Config::from_uri` for the options), or
    /// else a local directory.
    ///
    /// ```ignore
    /// let store = Store::at("s3://bucket/events?endpoint=http://garage:3900")?;
    /// let writer = TableWriter::open_in(Path::new("/var/lib/app/events"), store, options)?;
    /// ```
    pub fn at(location: &str) -> io::Result<Store> {
        if location.starts_with("s3://") {
            #[cfg(feature = "storage-s3")]
            return s3::S3Store::open_uri(location).map(Store::S3);
            #[cfg(not(feature = "storage-s3"))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "an s3:// table needs the `storage-s3` feature",
            ));
        }
        Ok(Store::local(location))
    }
}

impl TableStore for Store {
    type File = StoreFile;

    fn open(&self, key: &str) -> io::Result<StoreFile> {
        match self {
            Store::Local(s) => s.open(key).map(StoreFile::Local),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.open(key).map(StoreFile::S3),
            #[cfg(test)]
            Store::Memory(s) => s.open(key).map(StoreFile::Memory),
        }
    }

    fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match self {
            Store::Local(s) => s.get(key),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.get(key),
            #[cfg(test)]
            Store::Memory(s) => s.get(key),
        }
    }

    fn map(&self, key: &str) -> io::Result<MmapRegion> {
        match self {
            Store::Local(s) => s.map(key),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.map(key),
            #[cfg(test)]
            Store::Memory(s) => s.map(key),
        }
    }

    fn hold(&self, key: &str, size: u64) -> io::Result<HeldFile> {
        match self {
            Store::Local(s) => s.hold(key, size),
            // Read by range: only the footers and the column chunks a query
            // needs, through the store's read cache (#1052).
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s
                .open_held(key, size)
                .map(|f| HeldFile::ranged(StoreFile::S3(f))),
            // The same path in tests, over memory.
            #[cfg(test)]
            Store::Memory(s) => s.open(key).map(|f| HeldFile::ranged(StoreFile::Memory(f))),
        }
    }

    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        match self {
            Store::Local(s) => s.put(key, bytes),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.put(key, bytes),
            #[cfg(test)]
            Store::Memory(s) => s.put(key, bytes),
        }
    }

    fn publish(&self, local: &Path, key: &str) -> io::Result<()> {
        match self {
            Store::Local(s) => s.publish(local, key),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.publish(local, key),
            #[cfg(test)]
            Store::Memory(s) => s.publish(local, key),
        }
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        match self {
            Store::Local(s) => s.delete(key),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.delete(key),
            #[cfg(test)]
            Store::Memory(s) => s.delete(key),
        }
    }

    fn lease(&self, owner: &str) -> io::Result<Option<Lease>> {
        // Only the test store and the S3 store grant leases.
        let _ = owner;
        match self {
            Store::Local(_) => Ok(None),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.lease(owner),
            #[cfg(test)]
            Store::Memory(s) => s.lease(owner),
        }
    }

    fn remove_empty_dirs(&self, prefix: &str) -> io::Result<()> {
        match self {
            Store::Local(s) => s.remove_empty_dirs(prefix),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.remove_empty_dirs(prefix),
            #[cfg(test)]
            Store::Memory(s) => s.remove_empty_dirs(prefix),
        }
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
        match self {
            Store::Local(s) => s.list(prefix),
            #[cfg(feature = "storage-s3")]
            Store::S3(s) => s.list(prefix),
            #[cfg(test)]
            Store::Memory(s) => s.list(prefix),
        }
    }
}
