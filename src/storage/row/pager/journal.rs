//! Rollback-journal on-disk format: header layout and the per-page
//! checksum, matching stock SQLite's `pager.c` byte-for-byte (#172) so a
//! journal we write is recoverable by a real `sqlite3`, and vice versa.
//!
//! Header layout (28 bytes, `pager.c`'s `writeJournalHdr`), followed by
//! zero padding out to [`JournalHeader::sector_size`] bytes before the
//! first page record:
//!
//! | offset | len | field                                    |
//! |--------|-----|-------------------------------------------|
//! | 0      | 8   | magic (`super::JOURNAL_MAGIC`)      |
//! | 8      | 4   | `n_rec` — number of page records that follow |
//! | 12     | 4   | `nonce` — checksum salt (`cksumInit`)      |
//! | 16     | 4   | `initial_page_count` — db size before the txn |
//! | 20     | 4   | `sector_size`                              |
//! | 24     | 4   | `page_size`                                |
//!
//! Each page record is `4 + page_size + 4` bytes: big-endian page number,
//! the page's original content, then [`page_checksum`] of that content.

use std::path::Path;

use crate::storage::row::vfs::{AnyVfs, AnyVfsFile, VfsError};

/// Fixed size, in bytes, of the rollback-journal header (see the module
/// doc's byte layout table).
pub const JOURNAL_HEADER_LEN: usize = 28;

/// Errors from parsing a rollback journal's header or page records.
#[derive(Debug)]
pub enum JournalError {
    /// The buffer was shorter than [`JOURNAL_HEADER_LEN`].
    HeaderTooShort(usize),

    /// A record the journal's size says is whole read short (#875): the
    /// file shrank under recovery, or a read came up short.
    RecordTruncated {
        /// Zero-based index of the truncated record.
        index: u32,
        /// Expected record length in bytes.
        expected: usize,
        /// Bytes actually available.
        got: usize,
    },

    /// A VFS-level I/O error.
    Vfs(VfsError),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::HeaderTooShort(got) => write!(
                f,
                "journal header too short: expected at least {JOURNAL_HEADER_LEN} bytes, got {got}"
            ),
            JournalError::RecordTruncated {
                index,
                expected,
                got,
            } => write!(
                f,
                "journal record {index} truncated: expected {expected} bytes, got {got}"
            ),
            JournalError::Vfs(source) => std::fmt::Display::fmt(source, f),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<VfsError> for JournalError {
    fn from(source: VfsError) -> Self {
        JournalError::Vfs(source)
    }
}

/// `4 + page_size + 4`: big-endian page number, the page's content, then
/// its [`page_checksum`].
fn record_len(page_size: u32) -> usize {
    4usize.saturating_add(page_size as usize).saturating_add(4)
}

/// A parsed/serialized rollback-journal header. See the module doc for the
/// byte layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalHeader {
    /// Number of page records that follow the header.
    pub n_rec: u32,
    /// Checksum salt (`cksumInit`) used by [`page_checksum`].
    pub nonce: u32,
    /// Database size, in pages, before this transaction started.
    pub initial_page_count: u32,
    /// Sector size the header's zero padding is aligned to.
    pub sector_size: u32,
    /// Page size of the database this journal belongs to.
    pub page_size: u32,
}

impl JournalHeader {
    /// Parses the fixed 28-byte header from `buf`'s start. Does not
    /// validate the magic — callers that already branched on "is this
    /// journal hot" (`Pager::open`) have typically checked it already;
    /// [`super::JOURNAL_MAGIC`] is exposed for callers that haven't.
    pub fn parse(buf: &[u8]) -> Result<Self, JournalError> {
        let bytes: &[u8; JOURNAL_HEADER_LEN] = buf
            .get(..JOURNAL_HEADER_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or(JournalError::HeaderTooShort(buf.len()))?;
        #[allow(
            clippy::indexing_slicing,
            reason = "fixed literal ranges into a 28-byte array, checked by the compiler"
        )]
        let be32 = |range: std::ops::Range<usize>| -> u32 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&bytes[range]);
            u32::from_be_bytes(b)
        };
        Ok(JournalHeader {
            n_rec: be32(8..12),
            nonce: be32(12..16),
            initial_page_count: be32(16..20),
            sector_size: be32(20..24),
            page_size: be32(24..28),
        })
    }

    /// Serializes the 28-byte header proper (magic + the five fields).
    /// Callers pad out to `sector_size` themselves before writing the
    /// first record — see `pager.rs`'s journal writer.
    pub fn serialize(&self, magic: [u8; 8]) -> [u8; JOURNAL_HEADER_LEN] {
        let mut out = [0u8; JOURNAL_HEADER_LEN];
        out[..8].copy_from_slice(&magic);
        out[8..12].copy_from_slice(&self.n_rec.to_be_bytes());
        out[12..16].copy_from_slice(&self.nonce.to_be_bytes());
        out[16..20].copy_from_slice(&self.initial_page_count.to_be_bytes());
        out[20..24].copy_from_slice(&self.sector_size.to_be_bytes());
        out[24..28].copy_from_slice(&self.page_size.to_be_bytes());
        out
    }
}

/// SQLite's `pager_cksum`: samples one byte every 200 bytes, starting at
/// `page.len() - 200` and walking down to (but not past) index 0, summing
/// into `nonce` with wrapping add. Deliberately not a "real" checksum
/// (SQLite's own comment: "it is not a real hashing function... fast to
/// compute and unlikely to collide with a valid page") — replicated
/// exactly so our journal records validate against a stock `sqlite3` and
/// vice versa.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "all subtraction is saturating_sub; index is checked against page.len() before indexing"
)]
pub fn page_checksum(nonce: u32, page: &[u8]) -> u32 {
    let mut cksum = nonce;
    let mut idx = page.len().saturating_sub(200);
    while idx > 0 && idx < page.len() {
        #[allow(
            clippy::indexing_slicing,
            reason = "idx < page.len() is checked by the loop guard"
        )]
        {
            cksum = cksum.wrapping_add(u32::from(page[idx]));
        }
        idx = idx.saturating_sub(200);
    }
    cksum
}

/// Writes a rollback journal's header and page records — the whole
/// commit protocol is: create, write every record, `sync`, then write
/// the dirty pages to the main file, `sync` that, then
/// [`crate::storage::row::vfs::Vfs::delete`] the journal (DELETE mode, #172).
///
/// The exact number of records is known upfront (`Pager::flush` computes
/// the full set of pages needing a pre-image before calling
/// [`JournalWriter::create`]), so unlike SQLite's own incremental writer
/// there's no need to write a placeholder `n_rec` and patch it in later.
pub struct JournalWriter {
    file: AnyVfsFile,
    page_size: u32,
    sector_size: u32,
    nonce: u32,
}

impl JournalWriter {
    /// Creates (or reopens, if a stale journal file is somehow still
    /// there) the `-journal` file at `path` and writes its header,
    /// zero-padded out to `sector_size` bytes.
    pub fn create(
        vfs: &AnyVfs,
        path: &Path,
        page_size: u32,
        sector_size: u32,
        initial_page_count: u32,
        n_rec: u32,
        nonce: u32,
    ) -> Result<Self, JournalError> {
        let file = vfs.create_or_open_write(path)?;
        let header = JournalHeader {
            n_rec,
            nonce,
            initial_page_count,
            sector_size,
            page_size,
        };
        let mut region = vec![0u8; sector_size as usize];
        let header_bytes = header.serialize(super::JOURNAL_MAGIC);
        region
            .get_mut(..JOURNAL_HEADER_LEN)
            .ok_or(JournalError::HeaderTooShort(sector_size as usize))?
            .copy_from_slice(&header_bytes);
        file.write_at(&region, 0)?;
        Ok(JournalWriter {
            file,
            page_size,
            sector_size,
            nonce,
        })
    }

    /// Writes `original`'s content (page `page_num`'s pre-image) as
    /// record `index` (0-based) — callers write records `0..n_rec` in
    /// order, matching the `n_rec` passed to [`JournalWriter::create`].
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "offset arithmetic is all saturating_add/saturating_mul"
    )]
    pub fn write_record(
        &self,
        index: u32,
        page_num: u32,
        original: &[u8],
    ) -> Result<(), JournalError> {
        let offset = (self.sector_size as u64)
            .saturating_add((index as u64).saturating_mul(record_len(self.page_size) as u64));
        let mut buf = Vec::with_capacity(record_len(self.page_size));
        buf.extend_from_slice(&page_num.to_be_bytes());
        buf.extend_from_slice(original);
        buf.extend_from_slice(&page_checksum(self.nonce, original).to_be_bytes());
        self.file.write_at(&buf, offset)?;
        Ok(())
    }

    /// Flushes every write made via [`JournalWriter::write_record`] to
    /// durable storage — must complete before any dirty page is written
    /// to the main file (`Pager::flush`'s ordering).
    pub fn sync(&self) -> Result<(), JournalError> {
        self.file.sync()?;
        Ok(())
    }
}

/// What [`recover`] restored — [`super::recover_hot_journal`] uses
/// this to truncate the main file back to its pre-transaction size.
pub struct RecoveredJournal {
    /// Database size, in pages, before the rolled-back transaction started.
    pub initial_page_count: u32,
    /// Page size of the database this journal belongs to.
    pub page_size: u32,
}

/// Applies every checksum-valid record from `journal_bytes` (the full
/// contents of a `-journal` file already confirmed to start with the
/// magic) to `db_file`, in order — SQLite's hot-journal rollback
/// (`pager.c`'s `pager_playback`). Stops at (and does not apply) the
/// first record whose checksum doesn't match `nonce`, or that runs past
/// the end of `journal_bytes`: leniency for the case where the crash
/// that left this journal hot also interrupted the journal's own last
/// write.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "all offset arithmetic is saturating_add/saturating_mul/saturating_sub"
)]
pub fn recover(
    journal_bytes: &[u8],
    db_file: &AnyVfsFile,
) -> Result<RecoveredJournal, JournalError> {
    let header = JournalHeader::parse(journal_bytes)?;
    let records = whole_records(journal_bytes.len() as u64, &header);
    recover_from(&header, records, db_file, |index, record| {
        let start = usize::try_from(record_offset(&header, index)).unwrap_or(usize::MAX);
        if let Some(source) = journal_bytes.get(start..start.saturating_add(record.len())) {
            record.copy_from_slice(source);
        }
        Ok(())
    })
}

/// [`recover`] reading the journal file `journal` of `size` bytes: its
/// header, then one record at a time (#875), never a buffer sized from the
/// file. A record its size says is whole but that reads short is
/// [`JournalError::RecordTruncated`]; only a partial last record is the
/// torn tail [`recover`] stops at.
pub(crate) fn recover_file(
    journal: &AnyVfsFile,
    size: u64,
    db_file: &AnyVfsFile,
) -> Result<RecoveredJournal, JournalError> {
    let mut head = [0u8; JOURNAL_HEADER_LEN];
    let want = usize::try_from(size)
        .unwrap_or(JOURNAL_HEADER_LEN)
        .min(JOURNAL_HEADER_LEN);
    let buf = head.get_mut(..want).unwrap_or(&mut []);
    let got = journal.read_full(buf, 0)?;
    let header = JournalHeader::parse(head.get(..got).unwrap_or(&[]))?;
    let records = whole_records(size, &header);
    recover_from(&header, records, db_file, |index, record| {
        let got = journal.read_full(record, record_offset(&header, index))?;
        if got == record.len() {
            Ok(())
        } else {
            Err(JournalError::RecordTruncated {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                expected: record.len(),
                got,
            })
        }
    })
}

/// How many whole records follow the header region of a journal of `size`
/// bytes: a partial last one is a torn tail.
fn whole_records(size: u64, header: &JournalHeader) -> usize {
    let records = size
        .saturating_sub(u64::from(header.sector_size))
        .checked_div(record_len(header.page_size) as u64)
        .unwrap_or(0);
    usize::try_from(records).unwrap_or(usize::MAX)
}

/// Where record `index` starts: after the header's sector.
fn record_offset(header: &JournalHeader, index: usize) -> u64 {
    (index as u64)
        .saturating_mul(record_len(header.page_size) as u64)
        .saturating_add(u64::from(header.sector_size))
}

/// Plays back the first `n_rec` of `records` whole records, which
/// `read_record(index, record)` fills one at a time.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "all offset arithmetic is saturating_add/saturating_mul/saturating_sub"
)]
fn recover_from(
    header: &JournalHeader,
    records: usize,
    db_file: &AnyVfsFile,
    mut read_record: impl FnMut(usize, &mut [u8]) -> Result<(), JournalError>,
) -> Result<RecoveredJournal, JournalError> {
    let mut record = vec![0u8; record_len(header.page_size)];
    for index in 0..records.min(header.n_rec as usize) {
        read_record(index, &mut record)?;
        let Some(page_num_bytes) = record.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
            break;
        };
        let page_num = u32::from_be_bytes(page_num_bytes);

        let Some(page_data) = record.get(4..4usize.saturating_add(header.page_size as usize))
        else {
            break;
        };
        let Some(checksum_bytes) = record
            .get(4usize.saturating_add(header.page_size as usize)..)
            .and_then(|s| <[u8; 4]>::try_from(s).ok())
        else {
            break;
        };
        let checksum = u32::from_be_bytes(checksum_bytes);

        if page_checksum(header.nonce, page_data) != checksum {
            break;
        }

        let page_offset = (page_num as u64)
            .saturating_sub(1)
            .saturating_mul(header.page_size as u64);
        db_file.write_at(page_data, page_offset)?;
    }

    Ok(RecoveredJournal {
        initial_page_count: header.initial_page_count,
        page_size: header.page_size,
    })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    const MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

    /// #875: a hot journal is read header first and then one record at a
    /// time; a record its size says is whole but that reads short is an
    /// error, so a journal past one `pread`'s reach is never half played
    /// back (and then deleted) as if its tail were torn.
    #[test]
    fn a_short_read_inside_the_journal_is_an_error_not_a_torn_tail() {
        use crate::storage::row::vfs::test_files::sized_file;
        // With a zero nonce an all-zero record checksums: records 0..3 play
        // back, and record 3 is cut short 10 bytes in.
        let header = JournalHeader {
            n_rec: 1_000_000,
            nonce: 0,
            initial_page_count: 1,
            sector_size: 512,
            page_size: 512,
        };
        let size = 1u64 << 40;
        let readable = 512 + 3 * record_len(512) as u64 + 10;
        let (journal, read) = sized_file(size, readable, header.serialize(MAGIC).to_vec());
        let (db, _) = sized_file(0, 0, Vec::new());
        match recover_file(&journal, size, &db) {
            Err(JournalError::RecordTruncated {
                index: 3, got: 10, ..
            }) => {}
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("a short record read as a torn tail"),
        }
        // A 1 TiB journal cost what was there to read, a record at a time.
        assert_eq!(read.get(), readable - 512 + JOURNAL_HEADER_LEN as u64);
    }

    #[test]
    fn header_roundtrips() {
        let header = JournalHeader {
            n_rec: 3,
            nonce: 0xdead_beef,
            initial_page_count: 7,
            sector_size: 512,
            page_size: 4096,
        };
        let bytes = header.serialize(MAGIC);
        assert_eq!(&bytes[..8], &MAGIC);
        let parsed = JournalHeader::parse(&bytes).unwrap();
        assert_eq!(parsed, header);
    }

    #[test]
    fn header_too_short_is_an_error() {
        let bytes = [0u8; 20];
        assert!(matches!(
            JournalHeader::parse(&bytes),
            Err(JournalError::HeaderTooShort(20))
        ));
    }

    #[test]
    fn checksum_matches_sqlite_pager_cksum_reference_vector() {
        // Hand-computed reference: a 512-byte page of all 0x01 bytes,
        // nonce 0. Sampled indices: 312, 112 (512-200=312, 312-200=112,
        // 112-200=-88 stops). Two samples of 0x01 each -> nonce + 2.
        let page = vec![1u8; 512];
        assert_eq!(page_checksum(0, &page), 2);
    }

    #[test]
    fn checksum_depends_on_nonce() {
        let page = vec![1u8; 512];
        assert_ne!(page_checksum(0, &page), page_checksum(1, &page));
    }

    #[test]
    fn checksum_of_short_page_below_200_bytes_is_just_the_nonce() {
        let page = vec![0xffu8; 100];
        assert_eq!(page_checksum(42, &page), 42);
    }

    // storage_row_pager_journal_page_checksum_247b853d: `idx > 0 && idx < page.len()` in page_checksum's sampling
    // loop. Only 2 of the 3 MC/DC vectors are reachable through this
    // public function: `idx` is seeded as `page.len().saturating_sub(200)`
    // and only ever decreases by further `saturating_sub(200)` steps, so
    // it can never exceed `page.len()` while positive — the "idx > 0 but
    // idx >= page.len()" combination is structurally unreachable, not
    // just untested.
    #[test]
    fn mcdc__storage_row_pager_journal_page_checksum_247b853d__v1_both_true_the_loop_samples() {
        let page = vec![1u8; 512];
        assert_eq!(page_checksum(0, &page), 2);
    }

    #[test]
    fn mcdc__storage_row_pager_journal_page_checksum_247b853d__v2_idx_zero_the_loop_never_runs() {
        let page = vec![0xffu8; 100];
        assert_eq!(page_checksum(42, &page), 42);
    }

    /// Third vector, leaf B (`idx < page.len()`) false with leaf A true:
    /// structurally unreachable (see the note above -- `idx` starts at
    /// `len - 200` and only ever decreases), so this pins the invariant
    /// that makes it so instead: for every page size the guard's second
    /// leaf holds on the first iteration, and the sampled sum matches a
    /// direct recomputation.
    #[test]
    fn mcdc__storage_row_pager_journal_page_checksum_247b853d__v3_idx_never_reaches_len() {
        for len in [201usize, 512, 1024, 4096, 65536] {
            let page: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut expected = 7u32;
            let mut idx = len - 200;
            while idx > 0 {
                assert!(idx < len, "idx {idx} escaped page of {len}");
                expected = expected.wrapping_add(u32::from(page[idx]));
                idx = idx.saturating_sub(200);
            }
            assert_eq!(page_checksum(7, &page), expected, "len {len}");
        }
    }

    #[test]
    fn writer_then_recover_restores_original_pages() {
        use crate::storage::row::vfs::{AnyVfs, MemoryVfs};
        use std::path::Path;

        let mut memory = MemoryVfs::new();
        let page_size = 512u32;
        // Main db: two pages, page 2 already "corrupted" by an
        // in-progress write whose pre-image (all 0xAA) is what recovery
        // must restore.
        let mut db = vec![0u8; page_size as usize];
        db.extend(vec![0xAAu8; page_size as usize]);
        memory.insert("/test.db", db);
        let vfs = AnyVfs::new(memory);

        let original_page_2 = vec![0xAAu8; page_size as usize];
        let writer = JournalWriter::create(
            &vfs,
            Path::new("/test.db-journal"),
            page_size,
            page_size,
            2,
            1,
            0x1234,
        )
        .unwrap();
        writer.write_record(0, 2, &original_page_2).unwrap();
        writer.sync().unwrap();

        // Simulate the crash: the main file now holds garbage in page 2.
        let db_file = vfs.open_write(Path::new("/test.db")).unwrap();
        db_file
            .write_at(&vec![0xFFu8; page_size as usize], page_size as u64)
            .unwrap();

        let journal_file = vfs.open_read(Path::new("/test.db-journal")).unwrap();
        let size = journal_file.size().unwrap();
        let mut journal_bytes = vec![0u8; size as usize];
        journal_file.read_at(&mut journal_bytes, 0).unwrap();

        let recovered = recover(&journal_bytes, &db_file).unwrap();
        assert_eq!(recovered.initial_page_count, 2);
        assert_eq!(recovered.page_size, page_size);

        let mut restored = vec![0u8; page_size as usize];
        db_file.read_at(&mut restored, page_size as u64).unwrap();
        assert_eq!(restored, original_page_2);
    }

    #[test]
    fn recover_stops_at_first_bad_checksum() {
        use crate::storage::row::vfs::{AnyVfs, MemoryVfs};
        use std::path::Path;

        let vfs = AnyVfs::new(MemoryVfs::new());
        let page_size = 512u32;
        vfs.create_or_open_write(Path::new("/x.db")).unwrap();
        let db_file = vfs.open_write(Path::new("/x.db")).unwrap();
        db_file.write_at(&[0u8; 512], 0).unwrap();

        let writer = JournalWriter::create(
            &vfs,
            Path::new("/x.db-journal"),
            page_size,
            page_size,
            1,
            1,
            7,
        )
        .unwrap();
        // A record whose checksum won't match (garbage page content
        // written directly, bypassing write_record's checksum).
        let mut bogus = vec![0u8; 4 + page_size as usize + 4];
        bogus[..4].copy_from_slice(&1u32.to_be_bytes());
        let journal_file = vfs.open_write(Path::new("/x.db-journal")).unwrap();
        journal_file.write_at(&bogus, page_size as u64).unwrap();
        drop(writer);

        let journal_file = vfs.open_read(Path::new("/x.db-journal")).unwrap();
        let size = journal_file.size().unwrap();
        let mut journal_bytes = vec![0u8; size as usize];
        journal_file.read_at(&mut journal_bytes, 0).unwrap();

        // No panic, no page written — just an early stop.
        let recovered = recover(&journal_bytes, &db_file).unwrap();
        assert_eq!(recovered.initial_page_count, 1);
        let mut untouched = vec![0xFFu8; page_size as usize];
        db_file.read_at(&mut untouched, 0).unwrap();
        assert_eq!(untouched, vec![0u8; page_size as usize]);
    }
}
