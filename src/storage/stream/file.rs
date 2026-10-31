//! `LogFile`: a live, append-only file read in line-aligned blocks from
//! both ends.
//!
//! Two cursors bound what has been loaded: `tail_off` (earliest byte
//! loaded) and `head_off` (byte after the last complete line indexed).
//! Opening positions both at the last newline, so a client can read
//! backwards for tail-first display and forwards for live follow without
//! ever scanning the whole file. Only complete lines leave this module: a
//! trailing partial line stays unconsumed until its newline arrives.
//!
//! Memory is bounded (#883): one [`LogFile::refresh`] reads at most
//! [`REFRESH_BATCH`] bytes of complete lines, and a line longer than the
//! file's maximum ([`DEFAULT_MAX_LINE`] unless set with
//! [`LogFile::with_max_line`]) is an `InvalidData` error naming its offset
//! -- never skipped and never buffered without limit.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bytes per read. One block becomes one [`Block`] (minus the partial
/// line at its boundary) and, downstream, one or more segments.
pub const BLOCK_SIZE: usize = 256 * 1024;

/// The longest line a [`LogFile`] reads unless told otherwise (#883).
pub const DEFAULT_MAX_LINE: usize = 1024 * 1024;

/// Bytes of complete lines one [`LogFile::refresh`] reads before it
/// returns them; the caller refreshes again for the rest (#883).
pub const REFRESH_BATCH: u64 = 8 * 1024 * 1024;

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

/// Which file a path pointed to when opened: device and inode. A log
/// rotated by rename-and-recreate keeps its path but gets a new identity,
/// which is how logfile-mode ingestion (ADR 0027, #571) tells "the file I
/// was reading" from "a new file at the same path".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// Device id.
    pub dev: u64,
    /// Inode number.
    pub ino: u64,
}

impl FileIdentity {
    #[cfg(unix)]
    fn from_metadata(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(_meta: &std::fs::Metadata) -> Self {
        FileIdentity { dev: 0, ino: 0 }
    }

    /// The identity of the file `path` names now.
    pub fn of(path: &Path) -> io::Result<Self> {
        Ok(Self::from_metadata(&std::fs::metadata(path)?))
    }
}

/// A log file with two line-aligned cursors.
#[derive(Debug)]
pub struct LogFile {
    path: PathBuf,
    file: File,
    identity: FileIdentity,
    /// Earliest loaded byte. Everything in `[tail_off, head_off)` has been
    /// handed out as blocks.
    tail_off: u64,
    /// One past the last complete line handed out.
    head_off: u64,
    /// Total bytes read from disk, for tests and stats.
    bytes_read: u64,
    /// The longest line this reads; a longer one is an error (#883).
    max_line: usize,
}

impl LogFile {
    /// Open read-only and position both cursors after the last complete
    /// line. Reads at most one block.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let identity = FileIdentity::from_metadata(&file.metadata()?);
        let mut lf = Self {
            path: path.to_path_buf(),
            file,
            identity,
            tail_off: 0,
            head_off: 0,
            bytes_read: 0,
            max_line: DEFAULT_MAX_LINE,
        };
        lf.reposition_at_end()?;
        Ok(lf)
    }

    /// Open read-only with both cursors at `offset`, so the next
    /// [`LogFile::refresh`] returns the complete lines from there on: how
    /// logfile-mode ingestion resumes, `offset` being the end of the last
    /// line already ingested (a block's [`Block::end_off`]). Fails with
    /// `InvalidInput` if the file is shorter than `offset` (it was truncated
    /// or replaced); compare [`LogFile::identity`] with the recorded one to
    /// tell a rotated file of at least that length.
    pub fn open_at(path: &Path, offset: u64) -> io::Result<Self> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if meta.len() < offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is {} bytes, shorter than the resume offset {offset}: truncated or rotated",
                    path.display(),
                    meta.len()
                ),
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            identity: FileIdentity::from_metadata(&meta),
            file,
            tail_off: offset,
            head_off: offset,
            bytes_read: 0,
            max_line: DEFAULT_MAX_LINE,
        })
    }

    /// Reads lines up to `bytes` long (at least one [`BLOCK_SIZE`]); a
    /// longer one is an error (#883). The default is [`DEFAULT_MAX_LINE`].
    #[must_use]
    pub fn with_max_line(mut self, bytes: usize) -> Self {
        self.max_line = bytes.max(BLOCK_SIZE);
        self
    }

    /// The identity of the file this was opened on.
    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Whether the path now names a different file than the one open (a
    /// rename-and-recreate rotation), or nothing at all.
    pub fn rotated(&self) -> io::Result<bool> {
        match FileIdentity::of(&self.path) {
            Ok(now) => Ok(now != self.identity),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        }
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
    /// A line longer than a block is read whole, from further back; one
    /// longer than the maximum is an error (#883), where it used to be
    /// skipped.
    pub fn read_tail(&mut self, min_bytes: u64) -> io::Result<Vec<Block>> {
        let mut blocks: Vec<Block> = Vec::new();
        let mut loaded: u64 = 0;
        while loaded < min_bytes && self.tail_off > 0 {
            let end = self.tail_off;
            let start = end.saturating_sub(BLOCK_SIZE as u64);
            let buf = self.read_range(start, end)?;
            // Align to a line start: drop the partial first line unless we
            // are at the file start. The dropped bytes are re-read as the
            // end of the next (older) block. A block with no newline is
            // inside one long line, which starts further back. The block's
            // last byte is the newline ending the line at `end`, so it is
            // not a line start inside the block: counting it would cut the
            // whole block and never move `tail_off` (#883).
            let first_line = if start == 0 {
                Some(0)
            } else {
                buf.split_last()
                    .and_then(|(_, before)| before.iter().position(|&b| b == b'\n'))
                    .map(|i| i.saturating_add(1))
            };
            let (block_off, bytes) = match first_line {
                Some(cut) => (
                    start.saturating_add(cut as u64),
                    buf.get(cut..).unwrap_or_default().to_vec(),
                ),
                None => {
                    let line = self.line_start(start, end)?;
                    (line, self.read_range(line, end)?)
                }
            };
            self.tail_off = block_off;
            if !bytes.is_empty() {
                loaded = loaded.saturating_add(bytes.len() as u64);
                blocks.push(Block {
                    file_off: block_off,
                    bytes: Arc::from(bytes),
                });
            }
        }
        blocks.reverse();
        Ok(blocks)
    }

    /// Where the line that holds byte `before - 1` starts: one past the
    /// last newline before `before`, or 0. Reads backwards a block at a
    /// time; the line runs to `line_end`, and one longer than the maximum
    /// is an error (#883).
    fn line_start(&mut self, before: u64, line_end: u64) -> io::Result<u64> {
        let mut hi = before;
        while hi > 0 {
            let lo = hi.saturating_sub(BLOCK_SIZE as u64);
            let buf = self.read_range(lo, hi)?;
            if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
                return Ok(lo.saturating_add(i as u64).saturating_add(1));
            }
            if line_end.saturating_sub(lo) > self.max_line as u64 {
                return Err(self.line_too_long(lo, line_end.saturating_sub(lo)));
            }
            hi = lo;
        }
        Ok(0)
    }

    /// The error for a line longer than the maximum: at least `len` bytes,
    /// reaching byte `at` (#883).
    fn line_too_long(&self, at: u64, len: u64) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: the line through byte {at} is at least {len} bytes, longer than the {}-byte maximum",
                self.path.display(),
                self.max_line
            ),
        )
    }

    /// Read forward from `head_off`, up to [`REFRESH_BATCH`] bytes of
    /// complete lines (#883: the caller refreshes again for more). Handles
    /// the three outcomes: nothing new, new complete lines, or truncation.
    /// A line longer than the maximum is an `InvalidData` error.
    pub fn refresh(&mut self) -> io::Result<Refresh> {
        let len = self.len()?;
        if len < self.head_off {
            self.reposition_at_end()?;
            return Ok(Refresh::Truncated);
        }
        if len == self.head_off {
            return Ok(Refresh::NoNew);
        }
        let started = self.head_off;
        let mut blocks = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut pending_off = self.head_off;
        let mut pos = self.head_off;
        while pos < len {
            let end = pos.saturating_add(BLOCK_SIZE as u64).min(len);
            let chunk = self.read_range(pos, end)?;
            pos = end;
            // `pending` holds no newline, so only the new chunk is searched
            // (#883: rescanning all of `pending` made a long line O(n^2)).
            let carried = pending.len();
            pending.extend_from_slice(&chunk);
            match chunk.iter().rposition(|&b| b == b'\n') {
                Some(i) => {
                    // The carried partial line ends at the chunk's first
                    // newline; lines wholly inside a chunk are shorter than
                    // the maximum, which is at least a block.
                    let first = chunk.iter().position(|&b| b == b'\n').unwrap_or(i);
                    let line = carried.saturating_add(first) as u64;
                    if line > self.max_line as u64 {
                        return Err(self.line_too_long(pending_off.saturating_add(line), line));
                    }
                    let cut = carried.saturating_add(i).saturating_add(1);
                    let rest = pending.split_off(cut);
                    let complete = std::mem::replace(&mut pending, rest);
                    let block_len = complete.len() as u64;
                    blocks.push(Block {
                        file_off: pending_off,
                        bytes: Arc::from(complete),
                    });
                    pending_off = pending_off.saturating_add(block_len);
                    self.head_off = pending_off;
                    if pending_off.saturating_sub(started) >= REFRESH_BATCH {
                        break;
                    }
                }
                // No newline yet: a long line or a partial tail. It stays
                // pending until its newline arrives, up to the maximum.
                None if pending.len() > self.max_line => {
                    return Err(self.line_too_long(pos, pending.len() as u64));
                }
                None => {}
            }
        }
        if blocks.is_empty() {
            Ok(Refresh::NoNew)
        } else {
            Ok(Refresh::New(blocks))
        }
    }

    /// Position both cursors after the last complete line in the file,
    /// reading one block from the end -- more only when the last line is
    /// longer than a block, which used to be skipped; one longer than the
    /// maximum is an error (#883).
    fn reposition_at_end(&mut self) -> io::Result<()> {
        let len = self.len()?;
        let head = self.line_start(len, len)?;
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
    use super::{Block, LogFile, Refresh, BLOCK_SIZE, DEFAULT_MAX_LINE, REFRESH_BATCH};
    use std::io;

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

    /// `n` bytes of `x` and a newline.
    fn long_line(n: usize) -> Vec<u8> {
        let mut line = vec![b'x'; n];
        line.push(b'\n');
        line
    }

    fn refreshed(lf: &mut LogFile) -> Vec<Block> {
        match lf.refresh().unwrap() {
            Refresh::New(blocks) => blocks,
            other => panic!("expected lines, got {other:?}"),
        }
    }

    /// #883: a line longer than a block but within the maximum comes out
    /// whole, from either end; it used to be skipped by `read_tail`.
    #[test]
    fn a_line_longer_than_a_block_is_read_whole_from_both_ends() {
        let long = 600 * 1024;
        let mut contents = b"a\n".to_vec();
        contents.extend(long_line(long));
        contents.extend(b"b\n");
        let t = tmp("long-both", &contents);
        let mut lf = LogFile::open_at(&t.0, 0).unwrap();
        let blocks = refreshed(&mut lf);
        let got = lines(&blocks);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].len(), long);
        let mut tail = LogFile::open(&t.0).unwrap();
        let blocks = tail.read_tail(u64::MAX).unwrap();
        assert_eq!(
            lines(&blocks).iter().map(String::len).collect::<Vec<_>>(),
            [1, long, 1]
        );
    }

    /// #883: a line longer than the maximum is an `InvalidData` error from
    /// `refresh`, `read_tail` and `open`, complete or still being written.
    #[test]
    fn a_line_longer_than_the_maximum_is_an_error() {
        let over = DEFAULT_MAX_LINE + BLOCK_SIZE;
        let mut contents = b"a\n".to_vec();
        contents.extend(long_line(over));
        let t = tmp("long-over", &contents);
        let mut lf = LogFile::open_at(&t.0, 0).unwrap();
        let err = lf.refresh().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("maximum"), "{err}");
        // Raising the maximum reads it.
        let mut lf = LogFile::open_at(&t.0, 0).unwrap().with_max_line(2 * over);
        assert_eq!(lines(&refreshed(&mut lf))[1].len(), over);
        // From the end, a complete last line is never read: the cursors go
        // after its newline.
        assert_eq!(
            LogFile::open(&t.0).unwrap().head_off(),
            contents.len() as u64
        );
        // Still being written (no newline yet): an error too, not a wait --
        // and `open` cannot place the cursors before it.
        let partial = tmp("long-partial", &vec![b'x'; over]);
        let mut lf = LogFile::open_at(&partial.0, 0).unwrap();
        assert_eq!(lf.refresh().unwrap_err().kind(), io::ErrorKind::InvalidData);
        let err = LogFile::open(&partial.0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        // `read_tail` reaching back into an over-long line.
        let mut contents = long_line(over);
        contents.extend(b"b\n");
        let t = tmp("long-tail", &contents);
        let mut lf = LogFile::open(&t.0).unwrap();
        assert_eq!(
            lf.read_tail(u64::MAX).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // The maximum is never below a block.
        let lf = LogFile::open_at(&t.0, 0).unwrap().with_max_line(1);
        assert_eq!(lf.max_line, BLOCK_SIZE);
    }

    /// #883: one refresh reads a bounded batch, not the whole new file;
    /// refreshing again continues where it stopped.
    #[test]
    fn refresh_reads_a_bounded_batch_and_continues() {
        let line = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde\n";
        let count = (3 * REFRESH_BATCH as usize) / line.len();
        let t = tmp("batches", &line.repeat(count));
        let mut lf = LogFile::open_at(&t.0, 0).unwrap();
        let mut total = 0;
        let mut refreshes = 0;
        loop {
            match lf.refresh().unwrap() {
                Refresh::New(blocks) => {
                    let bytes: usize = blocks.iter().map(|b| b.bytes.len()).sum();
                    assert!(bytes as u64 <= REFRESH_BATCH + BLOCK_SIZE as u64);
                    total += lines(&blocks).len();
                    refreshes += 1;
                }
                Refresh::NoNew => break,
                Refresh::Truncated => panic!("not truncated"),
            }
        }
        assert_eq!(total, count);
        assert!(refreshes >= 3, "{refreshes} refreshes");
    }

    #[test]
    fn open_at_resumes_after_the_ingested_lines() {
        let t = tmp("open-at", b"one\ntwo\n");
        let first = LogFile::open_at(&t.0, 0).unwrap().refresh().unwrap();
        let crate::storage::stream::Refresh::New(blocks) = first else {
            panic!("expected lines")
        };
        let resume = blocks.last().unwrap().end_off();
        assert_eq!(resume, 8);
        let mut f = std::fs::OpenOptions::new().append(true).open(&t.0).unwrap();
        std::io::Write::write_all(&mut f, b"three\npart").unwrap();
        let mut lf = LogFile::open_at(&t.0, resume).unwrap();
        assert_eq!((lf.tail_off(), lf.head_off()), (8, 8));
        let crate::storage::stream::Refresh::New(blocks) = lf.refresh().unwrap() else {
            panic!("expected lines")
        };
        assert_eq!(lines(&blocks), vec!["three"]);
        assert_eq!(blocks[0].file_off, 8);
    }

    #[test]
    fn open_at_past_the_end_is_refused() {
        let t = tmp("open-at-short", b"one\n");
        let err = LogFile::open_at(&t.0, 5).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("shorter than the resume offset 5"));
    }

    #[test]
    fn rename_and_recreate_is_a_rotation() {
        let t = tmp("rotate", b"a\n");
        let lf = LogFile::open_at(&t.0, 0).unwrap();
        assert_eq!(
            lf.identity(),
            crate::storage::stream::FileIdentity::of(&t.0).unwrap()
        );
        assert!(!lf.rotated().unwrap());
        let mut f = std::fs::OpenOptions::new().append(true).open(&t.0).unwrap();
        std::io::Write::write_all(&mut f, b"b\n").unwrap();
        assert!(!lf.rotated().unwrap(), "appending is not rotating");
        let old = t.0.with_extension("1");
        std::fs::rename(&t.0, &old).unwrap();
        assert!(lf.rotated().unwrap(), "gone");
        std::fs::write(&t.0, b"new\n").unwrap();
        assert!(lf.rotated().unwrap(), "recreated");
        std::fs::remove_file(&old).unwrap();
    }
}
