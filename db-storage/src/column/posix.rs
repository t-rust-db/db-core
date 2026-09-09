//! Standard-filesystem `Vfs` implementation, backed by `std::fs::File`.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

use crate::column::mmap;
use crate::column::vfs::{MmapRegion, Vfs, VfsFile};

/// Opens files from the local filesystem.
#[derive(Default)]
pub struct PosixVfs;

impl Vfs for PosixVfs {
    type File = PosixFile;

    fn open(&self, path: &Path) -> io::Result<Self::File> {
        let file = File::open(path)?;
        Ok(PosixFile {
            file: Mutex::new(file),
        })
    }
}

/// A file opened from the local filesystem.
///
/// `read_at` needs a cursor seek before reading (std::fs::File has no
/// portable positioned-read that works identically on Unix and Windows),
/// so the underlying `File` is guarded by a `Mutex` to keep `&self` reads
/// safe under concurrent callers.
pub struct PosixFile {
    file: Mutex<File>,
}

impl VfsFile for PosixFile {
    fn size(&self) -> io::Result<u64> {
        self.file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .metadata()
            .map(|m| m.len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.seek(SeekFrom::Start(offset))?;
        file.read(buf)
    }

    fn mmap(&self) -> io::Result<MmapRegion> {
        mmap::map_file(
            &self
                .file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
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
    use std::io::Write;

    fn write_temp_file(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-storage-posix-test-{name}-{}",
            std::process::id()
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        path
    }

    #[test]
    fn open_missing_file_errors() {
        let vfs = PosixVfs;
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-storage-posix-test-missing-{}",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        assert!(vfs.open(&path).is_err());
    }

    #[test]
    fn size_matches_content_length() {
        let path = write_temp_file("size", b"hello world");
        let vfs = PosixVfs;
        let file = vfs.open(&path).unwrap();
        assert_eq!(file.size().unwrap(), 11);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn read_at_offset_reads_expected_bytes() {
        let path = write_temp_file("read-at", b"0123456789");
        let vfs = PosixVfs;
        let file = vfs.open(&path).unwrap();

        let mut buf = [0u8; 4];
        let n = file.read_at(3, &mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf, b"3456");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn read_at_past_eof_returns_zero() {
        let path = write_temp_file("eof", b"short");
        let vfs = PosixVfs;
        let file = vfs.open(&path).unwrap();

        let mut buf = [0u8; 8];
        let n = file.read_at(100, &mut buf).unwrap();
        assert_eq!(n, 0);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn read_at_is_reusable_across_calls_without_advancing_state() {
        let path = write_temp_file("reuse", b"abcdefgh");
        let vfs = PosixVfs;
        let file = vfs.open(&path).unwrap();

        let mut buf = [0u8; 3];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"abc");
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"abc");
        assert_eq!(file.read_at(5, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"fgh");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn mmap_exposes_full_file_contents() {
        let path = write_temp_file("mmap", b"mapped-bytes");
        let vfs = PosixVfs;
        let file = vfs.open(&path).unwrap();

        let region = file.mmap().unwrap();
        assert_eq!(&*region, b"mapped-bytes");

        std::fs::remove_file(&path).unwrap();
    }
}
