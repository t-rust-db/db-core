//! The `Vfs` / `VfsFile` abstraction: engines (sqlite-rs, column-rs) code
//! against these traits instead of `std::fs` directly, so a different
//! backend (in-memory for tests, later S3 or similar) can be swapped in
//! without touching engine code.

use std::io;
use std::ops::Deref;
use std::path::Path;

/// Opens files. Implementations decide where bytes actually live
/// (local disk, memory, network).
pub trait Vfs: Send + Sync {
    /// The handle [`Vfs::open`] returns.
    type File: VfsFile;

    /// Open the file at `path` for reading.
    fn open(&self, path: &Path) -> io::Result<Self::File>;
}

/// A single open file. Supports both random-access reads and, where the
/// backend allows it, memory-mapping the whole file for zero-copy access.
pub trait VfsFile: Send + Sync {
    /// The file's length in bytes.
    fn size(&self) -> io::Result<u64>;

    /// Read up to `buf.len()` bytes starting at `offset`, returning how
    /// many were read: fewer near the end of the file, 0 at or past it.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    /// Map the whole file read-only. Backends that cannot mmap (e.g. an
    /// in-memory VFS) return a region that simply wraps the buffer they
    /// already hold.
    fn mmap(&self) -> io::Result<MmapRegion>;
}

/// A read-only view over a whole file's bytes, however the backend
/// produced them (an OS mmap, or a plain in-memory buffer). Deref's to
/// `&[u8]` so callers don't need to care which.
pub enum MmapRegion {
    /// An OS memory map of the file.
    Mapped(memmap2::Mmap),
    /// Bytes the backend already holds in memory.
    Owned(std::sync::Arc<[u8]>),
}

impl Deref for MmapRegion {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            MmapRegion::Mapped(m) => m,
            MmapRegion::Owned(b) => b,
        }
    }
}
