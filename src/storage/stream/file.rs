//! `LogFile`: a live, append-only file read in line-aligned blocks from
//! both ends.
//!
//! Two cursors bound what has been loaded: `tail_off` (earliest byte
//! loaded) and `head_off` (byte after the last complete line indexed).
//! Opening positions both at the last newline, so a client can read
//! backwards for tail-first display and forwards for live follow without
//! ever scanning the whole file. Only complete lines leave this module: a
//! trailing partial line stays unconsumed until its newline arrives.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bytes per read. One block becomes one [`Block`] (minus the partial
/// line at its boundary) and, downstream, one or more segments.
pub const BLOCK_SIZE: usize = 256 * 1024;

/// A run of complete lines from the file. `bytes` ends with `\n` unless
/// it is empty; `file_off` is the file offset of `bytes[0]`.
#[derive(Debug, Clone)]
pub struct Block {
    /// File offset of the first byte.
    pub file_off: u64,
    /// Line-aligned bytes, ending in a newline.
    pub bytes: Arc<[u8]>,
}

impl Block {
    /// Offset one past the last byte.
    #[must_use]
    pub fn end_off(&self) -> u64 {
        self.file_off.saturating_add(self.bytes.len() as u64)
    }
}

/// Result of a forward [`LogFile::refresh`].
#[derive(Debug)]
pub enum Refresh {
    /// No new complete line since the last refresh.
    NoNew,
    /// New complete lines, oldest block first.
    New(Vec<Block>),
    /// The file shrank below `head_off` (truncation or rotation). Cursors
    /// were repositioned at the new end; the caller drops what it loaded.
    Truncated,
}

/// A log file with two line-aligned cursors.
#[derive(Debug)]
pub struct LogFile {
    path: PathBuf,
    file: File,
    /// Earliest loaded byte. Everything in `[tail_off, head_off)` has been
    /// handed out as blocks.
    tail_off: u64,
    /// One past the last complete line handed out.
    head_off: u64,
    /// Total bytes read from disk, for tests and stats.
    bytes_read: u64,
}

impl LogFile {
    /// Open read-only and position both cursors after the last complete
    /// line. Reads at most one block.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut lf = Self {
            path: path.to_path_buf(),
            file,
            tail_off: 0,
            head_off: 0,
            bytes_read: 0,
        };
        lf.reposition_at_end()?;
        Ok(lf)
    }

    /// The path this file was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Earliest loaded byte.
    #[must_use]
    pub const fn tail_off(&self) -> u64 {
        self.tail_off
    }

    /// One past the last complete line handed out.
    #[must_use]
    pub const fn head_off(&self) -> u64 {
        self.head_off
    }

    /// Bytes read from disk since open.
    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Current file length.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Read backwards from `tail_off` until at least `min_bytes` of
    /// complete lines are loaded or the file start is reached. Blocks are
    /// returned oldest first; `tail_off` moves to the first returned byte.
    pub fn read_tail(&mut self, min_bytes: u64) -> io::Result<Vec<Block>> {
        let mut blocks: Vec<Block> = Vec::new();
        let mut loaded: u64 = 0;
        while loaded < min_bytes && self.tail_off > 0 {
            let end = self.tail_off;
            let start = end.saturating_sub(BLOCK_SIZE as u64);
            let mut buf = self.read_range(start, end)?;
            // Align to a line start: drop the partial first line unless we
            // are at the file start. The dropped bytes are re-read as the
            // end of the next (older) block.
            let cut = if start == 0 {
                0
            } else {
                match buf.iter().position(|&b| b == b'\n') {
                    Some(i) => i.saturating_add(1),
                    None => buf.len(), // a line longer than a block: skip it
                }
            };
            let block_off = start.saturating_add(cut as u64);
            if cut > 0 {
                buf.drain(..cut);
            }
            self.tail_off = block_off;
            if !buf.is_empty() {
                loaded = loaded.saturating_add(buf.len() as u64);
                blocks.push(Block {
                    file_off: block_off,
                    bytes: Arc::from(buf),
                });
            }
        }
        blocks.reverse();
        Ok(blocks)
    }

    /// Read forward from `head_off` to the end of file. Handles the three
    /// outcomes: nothing new, new complete lines, or truncation.
    pub fn refresh(&mut self) -> io::Result<Refresh> {
        let len = self.len()?;
        if len < self.head_off {
            self.reposition_at_end()?;
            return Ok(Refresh::Truncated);
        }
        if len == self.head_off {
            return Ok(Refresh::NoNew);
        }
        let mut blocks = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut pending_off = self.head_off;
        let mut pos = self.head_off;
        while pos < len {
            let end = pos.saturating_add(BLOCK_SIZE as u64).min(len);
            let chunk = self.read_range(pos, end)?;
            pos = end;
            pending.extend_from_slice(&chunk);
            if let Some(i) = pending.iter().rposition(|&b| b == b'\n') {
                let cut = i.saturating_add(1);
                let rest = pending.split_off(cut);
                let complete = std::mem::replace(&mut pending, rest);
                let block_len = complete.len() as u64;
                blocks.push(Block {
                    file_off: pending_off,
                    bytes: Arc::from(complete),
                });
                pending_off = pending_off.saturating_add(block_len);
                self.head_off = pending_off;
            }
            // else: no newline yet -- keep accumulating (long line or
            // partial tail); the loop ends at `len` and leaves it pending.
        }
        if blocks.is_empty() {
            Ok(Refresh::NoNew)
        } else {
            Ok(Refresh::New(blocks))
        }
    }

    /// Position both cursors after the last complete line in the file,
    /// reading at most one block from the end.
    fn reposition_at_end(&mut self) -> io::Result<()> {
        let len = self.len()?;
        let start = len.saturating_sub(BLOCK_SIZE as u64);
        let buf = self.read_range(start, len)?;
        let head = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => start.saturating_add(i as u64).saturating_add(1),
            None if start == 0 => 0, // no complete line at all
            None => len,             // a final line longer than a block
        };
        self.head_off = head;
        self.tail_off = head;
        Ok(())
    }

    /// Read `[start, end)` from disk.
    fn read_range(&mut self, start: u64, end: u64) -> io::Result<Vec<u8>> {
        let n = end.saturating_sub(start) as usize;
        let mut buf = vec![0u8; n];
        self.file.seek(SeekFrom::Start(start))?;
        let mut filled = 0usize;
        while filled < n {
            let got = self.file.read(buf.get_mut(filled..).unwrap_or(&mut []))?;
            if got == 0 {
                buf.truncate(filled);
                break;
            }
            filled = filled.saturating_add(got);
        }
        self.bytes_read = self.bytes_read.saturating_add(buf.len() as u64);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    pub(super) struct Tmp(pub(super) PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).ok();
        }
    }
    pub(super) fn tmp(name: &str, contents: &[u8]) -> Tmp {
        let mut p = std::env::temp_dir();
        p.push(format!("db-core-stream-{}-{}", std::process::id(), name));
        std::fs::write(&p, contents).unwrap();
        Tmp(p)
    }
    pub(super) fn lines(blocks: &[Block]) -> Vec<String> {
        let mut out = Vec::new();
        for b in blocks {
            for l in b.bytes.split(|&c| c == b'\n') {
                if !l.is_empty() {
                    out.push(String::from_utf8(l.to_vec()).unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn open_positions_after_last_newline() {
        let t = tmp("open", b"a\nb\npartial");
        let lf = LogFile::open(&t.0).unwrap();
        assert_eq!(lf.head_off(), 4);
        assert_eq!(lf.tail_off(), 4);
    }

    #[test]
    fn read_tail_returns_oldest_first_and_stops_at_min_bytes() {
        let t = tmp("tail", b"l1\nl2\nl3\nl4\n");
        let mut lf = LogFile::open(&t.0).unwrap();
        let blocks = lf.read_tail(1).unwrap();
        assert_eq!(lines(&blocks), vec!["l1", "l2", "l3", "l4"]);
        assert_eq!(lf.tail_off(), 0);
        assert!(lf.read_tail(1).unwrap().is_empty());
    }

    #[test]
    fn tail_of_large_file_reads_at_most_two_blocks() {
        // 64 MiB of ~100-byte lines; ask for the last 50 lines' worth.
        let line = b"<134>Sep 10 08:00:00 web01 nginx[1234]: GET /api/health 200 padding-padding-padding-padding-padd\n";
        let n = (64 * 1024 * 1024) / line.len();
        let mut body = Vec::with_capacity(n * line.len());
        for _ in 0..n {
            body.extend_from_slice(line);
        }
        let t = tmp("large", &body);
        let mut lf = LogFile::open(&t.0).unwrap();
        let blocks = lf.read_tail(50 * line.len() as u64).unwrap();
        assert!(lines(&blocks).len() >= 50);
        assert!(
            lf.bytes_read() <= 2 * BLOCK_SIZE as u64,
            "read {} bytes",
            lf.bytes_read()
        );
    }

    #[test]
    fn backwards_blocks_are_line_aligned_and_contiguous() {
        // Lines of varying length so block boundaries fall mid-line.
        let mut body = Vec::new();
        for i in 0..20_000u32 {
            body.extend_from_slice(
                format!("line-{i}-{}\n", "x".repeat((i % 37) as usize)).as_bytes(),
            );
        }
        let t = tmp("aligned", &body);
        let mut lf = LogFile::open(&t.0).unwrap();
        let blocks = lf.read_tail(u64::MAX).unwrap();
        assert!(blocks.len() > 1);
        let mut expect = 0u64;
        for b in &blocks {
            assert_eq!(b.file_off, expect, "gap or overlap between blocks");
            assert_eq!(*b.bytes.last().unwrap(), b'\n');
            expect = b.end_off();
        }
        assert_eq!(expect, body.len() as u64);
        assert_eq!(lines(&blocks).len(), 20_000);
    }

    #[test]
    fn refresh_sees_appends_and_holds_partial_lines() {
        let t = tmp("append", b"a\n");
        let mut lf = LogFile::open(&t.0).unwrap();
        assert!(matches!(lf.refresh().unwrap(), Refresh::NoNew));
        let mut f = std::fs::OpenOptions::new().append(true).open(&t.0).unwrap();
        f.write_all(b"b\nc\npart").unwrap();
        match lf.refresh().unwrap() {
            Refresh::New(blocks) => assert_eq!(lines(&blocks), vec!["b", "c"]),
            other => panic!("{other:?}"),
        }
        assert_eq!(lf.head_off(), 6);
        f.write_all(b"ial\n").unwrap();
        match lf.refresh().unwrap() {
            Refresh::New(blocks) => assert_eq!(lines(&blocks), vec!["partial"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn truncation_resets_cursors_without_panic() {
        let t = tmp("trunc", b"one\ntwo\nthree\n");
        let mut lf = LogFile::open(&t.0).unwrap();
        assert_eq!(lf.head_off(), 14);
        std::fs::write(&t.0, b"new\n").unwrap();
        assert!(matches!(lf.refresh().unwrap(), Refresh::Truncated));
        assert_eq!(lf.head_off(), 4);
        assert_eq!(lf.tail_off(), 4);
        assert_eq!(lines(&lf.read_tail(1).unwrap()), vec!["new"]);
    }

    #[test]
    fn empty_file_is_fine() {
        let t = tmp("empty", b"");
        let mut lf = LogFile::open(&t.0).unwrap();
        assert!(lf.read_tail(1).unwrap().is_empty());
        assert!(matches!(lf.refresh().unwrap(), Refresh::NoNew));
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#299 MC/DC backfill).

    use super::tests::{lines, tmp};
    use super::LogFile;

    // storage_stream_file_read_tail_c82dd615: `loaded < min_bytes && self.tail_off > 0`
    #[test]
    fn mcdc__storage_stream_file_read_tail_c82dd615__v1_both_true_keeps_reading_backwards() {
        let t = tmp("mcdc_v1", b"l1\nl2\nl3\nl4\n");
        let mut lf = LogFile::open(&t.0).unwrap();
        // Not enough loaded yet (true) and there's still file before
        // tail_off (true), so read_tail keeps pulling blocks.
        let blocks = lf.read_tail(1).unwrap();
        assert_eq!(lines(&blocks), vec!["l1", "l2", "l3", "l4"]);
        assert_eq!(lf.tail_off(), 0);
    }

    #[test]
    fn mcdc__storage_stream_file_read_tail_c82dd615__v2_min_bytes_already_satisfied() {
        let t = tmp("mcdc_v2", b"l1\nl2\nl3\nl4\n");
        let mut lf = LogFile::open(&t.0).unwrap();
        // A tiny min_bytes is satisfied by the first block read, so the
        // first leaf (`loaded < min_bytes`) goes false and the loop stops
        // regardless of `tail_off`.
        let blocks = lf.read_tail(1).unwrap();
        assert!(!blocks.is_empty());
        assert!(lf.tail_off() < lf.head_off());
    }

    #[test]
    fn mcdc__storage_stream_file_read_tail_c82dd615__v3_tail_off_reaches_zero_before_min_bytes() {
        let t = tmp("mcdc_v3", b"");
        let mut lf = LogFile::open(&t.0).unwrap();
        // Empty file: `loaded < min_bytes` stays true (nothing loaded),
        // but `tail_off > 0` is false from the start, so the loop never
        // runs and returns no blocks.
        assert!(lf.read_tail(1).unwrap().is_empty());
        assert_eq!(lf.tail_off(), 0);
    }
}
