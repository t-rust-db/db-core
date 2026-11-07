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

use crate::storage::column::posix::PosixFile;
use crate::storage::column::{MmapRegion, VfsFile};

#[cfg(test)]
mod conformance;
mod local;
#[cfg(test)]
pub(crate) mod memory;

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

    /// Atomically replaces the object with `bytes`, durably: after `Ok`, a
    /// crash leaves either the old or the new object, never a mix, and
    /// never loses the new one. This is the manifest's commit point.
    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()>;

    /// Moves the finished local file `local` into the store as `key`,
    /// durably, and replaces any object already there. The file is gone from
    /// `local` after `Ok`. A crash leaves the object either absent or whole.
    fn publish(&self, local: &Path, key: &str) -> io::Result<()>;

    /// Removes the object; a missing one is [`io::ErrorKind::NotFound`].
    fn delete(&self, key: &str) -> io::Result<()>;

    /// Removes directories under `prefix` that hold no objects. A store
    /// with no directories (an object store) has nothing to do.
    fn remove_empty_dirs(&self, prefix: &str) -> io::Result<()>;

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
    /// In memory, for tests.
    #[cfg(test)]
    Memory(std::sync::Arc<memory::MemoryStore>),
}

/// An object opened from a [`Store`].
pub enum StoreFile {
    /// From the local filesystem.
    Local(PosixFile),
    /// From memory, for tests.
    #[cfg(test)]
    Memory(crate::storage::column::memory::MemoryFile),
}

impl VfsFile for StoreFile {
    fn size(&self) -> io::Result<u64> {
        match self {
            StoreFile::Local(f) => f.size(),
            #[cfg(test)]
            StoreFile::Memory(f) => f.size(),
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            StoreFile::Local(f) => f.read_at(offset, buf),
            #[cfg(test)]
            StoreFile::Memory(f) => f.read_at(offset, buf),
        }
    }

    fn mmap(&self) -> io::Result<MmapRegion> {
        match self {
            StoreFile::Local(f) => f.mmap(),
            #[cfg(test)]
            StoreFile::Memory(f) => f.mmap(),
        }
    }
}

impl Store {
    /// The local directory `dir` as a store.
    pub fn local(dir: impl Into<std::path::PathBuf>) -> Store {
        Store::Local(LocalFs::new(dir))
    }
}

impl TableStore for Store {
    type File = StoreFile;

    fn open(&self, key: &str) -> io::Result<StoreFile> {
        match self {
            Store::Local(s) => s.open(key).map(StoreFile::Local),
            #[cfg(test)]
            Store::Memory(s) => s.open(key).map(StoreFile::Memory),
        }
    }

    fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match self {
            Store::Local(s) => s.get(key),
            #[cfg(test)]
            Store::Memory(s) => s.get(key),
        }
    }

    fn map(&self, key: &str) -> io::Result<MmapRegion> {
        match self {
            Store::Local(s) => s.map(key),
            #[cfg(test)]
            Store::Memory(s) => s.map(key),
        }
    }

    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        match self {
            Store::Local(s) => s.put(key, bytes),
            #[cfg(test)]
            Store::Memory(s) => s.put(key, bytes),
        }
    }

    fn publish(&self, local: &Path, key: &str) -> io::Result<()> {
        match self {
            Store::Local(s) => s.publish(local, key),
            #[cfg(test)]
            Store::Memory(s) => s.publish(local, key),
        }
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        match self {
            Store::Local(s) => s.delete(key),
            #[cfg(test)]
            Store::Memory(s) => s.delete(key),
        }
    }

    fn remove_empty_dirs(&self, prefix: &str) -> io::Result<()> {
        match self {
            Store::Local(s) => s.remove_empty_dirs(prefix),
            #[cfg(test)]
            Store::Memory(s) => s.remove_empty_dirs(prefix),
        }
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
        match self {
            Store::Local(s) => s.list(prefix),
            #[cfg(test)]
            Store::Memory(s) => s.list(prefix),
        }
    }
}
