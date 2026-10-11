//! The table writer (ADR 0027, #572): WAL, pages, flush and manifest in
//! one owner.
//!
//! A table directory holds `LOCK`, `manifest.json`, `wal/`, `data/` (the
//! live Parquet files) and `tmp/` (files being written). [`TableWriter`]
//! appends records (one line each, in the table's format) to the WAL and
//! the head page, seals full pages, and flushes sealed pages to one
//! Parquet file at a time by the commit protocol:
//!
//! 1. write the file under `tmp/` and fsync it;
//! 2. rename it into `data/` and fsync the directory;
//! 3. **commit**: replace `manifest.json` with the file added and
//!    `flushed_lsn` at its last LSN;
//! 4. adopt the new manifest;
//! 5. tell the WAL (`mark_flushed`, which unlinks its flushed segments) and
//!    drop the flushed pages.
//!
//! Opening recovers from a crash at any step: `tmp/` is emptied, a file in
//! `data/` the manifest does not list is deleted (its rows are still in the
//! WAL), and the WAL replays everything above `flushed_lsn` into pages, so
//! the table holds every acknowledged record exactly once.
//!
//! This is the single-threaded core; #574 wraps it as the public `Table`
//! with an ingest thread and input sources.

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use super::flush;
use super::lock::{LockError, TableLock};
use super::manifest::{FileEntry, Manifest, ManifestError};
use super::page::{decode_record, encode_record, invalid_line, HeadPage, Page};
use super::snapshot::Snapshot;
use super::wal::{Ack, Wal, WalError, WalOptions, Watermarks};
use crate::storage::parquet::writer::{WriteError, WriterOptions};
use crate::storage::stream::adapter::now_ns;
use crate::storage::stream::detect::{DetectedParser, Format};
use crate::storage::stream::{Source, SourceKind};

/// Live Parquet files, relative to the table directory.
pub const DATA_DIR: &str = "data";
/// Files being written by a flush.
pub const TMP_DIR: &str = "tmp";

/// How a table is written.
#[derive(Debug, Clone)]
pub struct TableOptions {
    /// The record format, fixed when the table is created.
    pub format: Format,
    /// WAL settings.
    pub wal: WalOptions,
    /// Parquet writer settings for flushed files.
    pub parquet: WriterOptions,
}

impl Default for TableOptions {
    /// JSON Lines records, default WAL and Parquet settings.
    fn default() -> Self {
        TableOptions {
            format: Format::Jsonl,
            wal: WalOptions::default(),
            parquet: WriterOptions::default(),
        }
    }
}

/// Why a table operation failed.
#[derive(Debug)]
pub enum IngestError {
    /// The WAL failed.
    Wal(WalError),
    /// The manifest could not be read, written or validated.
    Manifest(ManifestError),
    /// Writing a Parquet file failed.
    Parquet(WriteError),
    /// File system error.
    Io(io::Error),
    /// The record is not one non-empty line.
    InvalidRecord(String),
    /// The table exists with another record format.
    FormatMismatch {
        /// The table's format.
        table: Format,
        /// The format asked for.
        requested: Format,
    },
    /// A replayed WAL payload is not a record, or the parser rejected a
    /// page.
    Corrupt(String),
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestError::Wal(e) => write!(f, "{e}"),
            IngestError::Manifest(e) => write!(f, "{e}"),
            IngestError::Parquet(e) => write!(f, "{e}"),
            IngestError::Io(e) => write!(f, "table: {e}"),
            IngestError::InvalidRecord(why) => write!(f, "invalid record: {why}"),
            IngestError::FormatMismatch { table, requested } => write!(
                f,
                "the table's format is {}, not {}",
                table.as_str(),
                requested.as_str()
            ),
            IngestError::Corrupt(why) => write!(f, "table: {why}"),
        }
    }
}

impl std::error::Error for IngestError {}

impl From<WalError> for IngestError {
    fn from(e: WalError) -> Self {
        IngestError::Wal(e)
    }
}

impl From<ManifestError> for IngestError {
    fn from(e: ManifestError) -> Self {
        IngestError::Manifest(e)
    }
}

impl From<WriteError> for IngestError {
    fn from(e: WriteError) -> Self {
        IngestError::Parquet(e)
    }
}

impl From<io::Error> for IngestError {
    fn from(e: io::Error) -> Self {
        IngestError::Io(e)
    }
}

/// Owns a table directory for writing; see the [module docs](self).
#[derive(Debug)]
pub struct TableWriter {
    dir: PathBuf,
    wal: Wal,
    manifest: Manifest,
    options: TableOptions,
    parser: DetectedParser,
    source: Source,
    head: HeadPage,
    sealed: Vec<Page>,
    last_observed: i64,
    #[cfg(test)]
    pub(super) crash_after: Option<u8>,
}

impl TableWriter {
    /// Opens (or creates) the table at `table`: takes the lock, reads or
    /// writes the manifest, cleans up after an interrupted flush, and
    /// rebuilds pages from the WAL above `flushed_lsn`.
    pub fn open(table: &Path, options: TableOptions) -> Result<TableWriter, IngestError> {
        let lock = TableLock::acquire(table).map_err(|e| match e {
            LockError::Held => IngestError::Wal(WalError::Locked),
            LockError::Io(e) => IngestError::Io(e),
        })?;
        let manifest = match Manifest::read(table)? {
            Some(m) => m,
            None => {
                let m = Manifest::new(options.format);
                m.write(table)?;
                m
            }
        };
        if manifest.format != options.format {
            return Err(IngestError::FormatMismatch {
                table: manifest.format,
                requested: options.format,
            });
        }
        clean_up(table, &manifest)?;
        let (wal, records) =
            Wal::open_locked(lock, table, manifest.flushed_lsn, options.wal.clone())?;
        let mut writer = TableWriter {
            dir: table.to_path_buf(),
            wal,
            parser: DetectedParser::for_format(manifest.format),
            source: Source::new(SourceKind::Stdin, &table.to_string_lossy()),
            manifest,
            options,
            head: HeadPage::default(),
            sealed: Vec::new(),
            last_observed: i64::MIN,
            #[cfg(test)]
            crash_after: None,
        };
        for (lsn, payload) in records {
            let (observed, line) = decode_record(&payload)
                .ok_or_else(|| IngestError::Corrupt(format!("WAL record {lsn} is not a record")))?;
            writer.last_observed = writer.last_observed.max(observed);
            writer.head.push(lsn, observed, line);
            if writer.head.is_full() {
                writer.seal_head()?;
            }
        }
        Ok(writer)
    }

    /// Appends one record (a line, without its newline) and waits for
    /// `ack`. `Ack::Flushed` seals and flushes everything up to it before
    /// returning. Returns the record's LSN.
    pub fn append(&mut self, line: &[u8], ack: Ack) -> Result<u64, IngestError> {
        if let Some(why) = invalid_line(line) {
            return Err(IngestError::InvalidRecord(why));
        }
        // Arrival time, never going backwards (ADR 0027).
        let observed = now_ns().max(self.last_observed);
        let wal_ack = match ack {
            Ack::Flushed => Ack::Durable,
            other => other,
        };
        let lsn = self.wal.append(&encode_record(observed, line), wal_ack)?;
        self.last_observed = observed;
        self.head.push(lsn, observed, line);
        if self.head.is_full() {
            self.seal_head()?;
        }
        if ack == Ack::Flushed {
            self.seal_head()?;
            self.flush()?;
        }
        Ok(lsn)
    }

    /// Seals the head into a page (nothing if empty) and starts a new WAL
    /// segment, so WAL files line up with pages.
    pub fn seal_head(&mut self) -> Result<(), IngestError> {
        if self.head.len() == 0 {
            return Ok(());
        }
        let page = self
            .head
            .snapshot(&self.source, &self.parser)
            .ok_or_else(|| IngestError::Corrupt("the parser rejected a page".to_string()))?;
        self.sealed.push(page);
        self.head.clear();
        self.wal.roll()?;
        Ok(())
    }

    /// Flushes every sealed page to one Parquet file and commits it;
    /// `None` when nothing is sealed.
    pub fn flush(&mut self) -> Result<Option<FileEntry>, IngestError> {
        let (Some(first), Some(last)) = (self.sealed.first(), self.sealed.last()) else {
            return Ok(None);
        };
        let (lsn_lo, lsn_hi) = (first.lsn_lo, last.lsn_hi);
        // flushed <= durable: everything going into the file is fsynced.
        self.wal.sync()?;
        let name = format!("{lsn_lo:020}-{lsn_hi:020}.parquet");

        // 1. Write under tmp/.
        let tmp_dir = self.dir.join(TMP_DIR);
        fs::create_dir_all(&tmp_dir)?;
        let tmp = tmp_dir.join(&name);
        let written = flush::write_file(&tmp, &self.sealed, &self.options.parquet)?;
        self.crash_point(1)?;

        // 2. Publish into data/.
        let data_dir = self.dir.join(DATA_DIR);
        fs::create_dir_all(&data_dir)?;
        fs::rename(&tmp, data_dir.join(&name))?;
        File::open(&data_dir)?.sync_all()?;
        self.crash_point(2)?;

        // 3. Commit.
        let entry = FileEntry {
            path: format!("{DATA_DIR}/{name}"),
            lsn_lo,
            lsn_hi,
            rows: written.rows,
            bytes: written.bytes,
            observed_ts: written.observed_ts,
            timestamp: written.timestamp,
        };
        let next = self.manifest.with_file(entry.clone());
        next.write(&self.dir)?;
        self.crash_point(3)?;

        // 4. Adopt.
        self.manifest = next;

        // 5. Release the WAL and the pages.
        self.wal.mark_flushed(lsn_hi)?;
        self.sealed.clear();
        self.crash_point(5)?;
        Ok(Some(entry))
    }

    /// Seals the head and flushes everything.
    pub fn flush_all(&mut self) -> Result<Option<FileEntry>, IngestError> {
        self.seal_head()?;
        self.flush()
    }

    /// The committed manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The WAL watermarks.
    pub fn watermarks(&self) -> Result<Watermarks, IngestError> {
        Ok(self.wal.watermarks()?)
    }

    /// Sealed pages above `flushed_lsn`, oldest first.
    pub fn sealed_pages(&self) -> &[Page] {
        &self.sealed
    }

    /// A copy of the head page, if it holds anything.
    pub fn head_page(&self) -> Option<Page> {
        self.head.snapshot(&self.source, &self.parser)
    }

    /// A read view of everything appended so far: the committed files and
    /// every page, the head included.
    pub fn snapshot(&self) -> Result<Snapshot, IngestError> {
        let pages = self
            .sealed
            .iter()
            .cloned()
            .chain(self.head_page())
            .collect();
        Snapshot::new(&self.dir, self.manifest.clone(), pages)
    }

    /// The table directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Makes everything appended durable and closes the WAL. Pages not
    /// flushed stay in the WAL for the next open.
    pub fn close(self) -> Result<(), IngestError> {
        Ok(self.wal.close()?)
    }

    #[cfg(test)]
    fn crash_point(&self, step: u8) -> Result<(), IngestError> {
        if self.crash_after == Some(step) {
            return Err(IngestError::Io(io::Error::other(format!(
                "injected crash after flush step {step}"
            ))));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[allow(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "test-only fail points"
    )]
    fn crash_point(&self, _step: u8) -> Result<(), IngestError> {
        Ok(())
    }
}

/// Recovery steps 1-2: empty `tmp/`, delete Parquet files in `data/` the
/// manifest does not list.
fn clean_up(table: &Path, manifest: &Manifest) -> Result<(), IngestError> {
    let tmp = table.join(TMP_DIR);
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    let data = table.join(DATA_DIR);
    let Ok(entries) = fs::read_dir(&data) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let relative = format!("{DATA_DIR}/{}", entry.file_name().to_string_lossy());
        if !manifest.files.iter().any(|f| f.path == relative) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
