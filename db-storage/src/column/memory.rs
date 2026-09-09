//! In-memory `Vfs` implementation for tests: a named set of byte buffers,
//! no filesystem involved.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::column::vfs::{MmapRegion, Vfs, VfsFile};

/// A `Vfs` backed by an in-memory map of path -> bytes. Populate it with
/// [`MemoryVfs::insert`] before opening files.
#[derive(Default)]
pub struct MemoryVfs {
    files: Mutex<HashMap<PathBuf, Arc<[u8]>>>,
}

impl MemoryVfs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace the contents of a virtual file.
    pub fn insert(&self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) {
        self.files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.into(), contents.into().into());
    }
}

impl Vfs for MemoryVfs {
    type File = MemoryFile;

    fn open(&self, path: &Path) -> io::Result<Self::File> {
        let contents = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))?;
        Ok(MemoryFile { contents })
    }
}

/// A file opened from a [`MemoryVfs`].
pub struct MemoryFile {
    contents: Arc<[u8]>,
}

impl VfsFile for MemoryFile {
    fn size(&self) -> io::Result<u64> {
        Ok(self.contents.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let offset = offset as usize;
        if offset >= self.contents.len() {
            return Ok(0);
        }
        let available = &self.contents[offset..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        Ok(n)
    }

    fn mmap(&self) -> io::Result<MmapRegion> {
        Ok(MmapRegion::Owned(self.contents.clone()))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_inserted_contents() {
        let vfs = MemoryVfs::new();
        vfs.insert("hello.txt", b"hello mmap world".to_vec());

        let file = vfs.open(Path::new("hello.txt")).unwrap();
        assert_eq!(file.size().unwrap(), 16);

        let mapped = file.mmap().unwrap();
        assert_eq!(&*mapped, b"hello mmap world");
    }

    #[test]
    fn read_at_respects_offset_and_short_buffers() {
        let vfs = MemoryVfs::new();
        vfs.insert("data.bin", b"0123456789".to_vec());
        let file = vfs.open(Path::new("data.bin")).unwrap();

        let mut buf = [0u8; 4];
        let n = file.read_at(3, &mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf, b"3456");

        // Reading past the end returns 0, not an error.
        let n = file.read_at(100, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn missing_file_errors_not_panics() {
        let vfs = MemoryVfs::new();
        let result = vfs.open(Path::new("does-not-exist.bin"));
        assert!(result.is_err());
    }

    #[test]
    fn maps_empty_file() {
        let vfs = MemoryVfs::new();
        vfs.insert("empty.bin", Vec::new());
        let file = vfs.open(Path::new("empty.bin")).unwrap();
        assert_eq!(file.size().unwrap(), 0);
        assert!(file.mmap().unwrap().is_empty());
    }
}
