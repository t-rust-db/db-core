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
//! `data/`, `live/` or `archive/` the manifest does not list is deleted
//! (its rows are still in the WAL, or in the files a compaction read), and
//! the WAL replays everything above `flushed_lsn` into pages, so the table
//! holds every acknowledged record exactly once.
//!
//! A tiered table (ADR 0028, [`Layout::Tiered`]) seals the head at every
//! hour boundary, and a flush writes only closed hours, each as one
//! directory under `live/`; [`TableWriter::compact`] merges closed days
//! into `archive/` and deletes what it replaced once `delete_grace_ns` has
//! passed. A tiered table keeps its open hour in memory until it closes
//! (spilling to `staging/` under a memory budget is #573's second part).
//!
//! A record table (#594, [`TableOptions::schema`]) is appended to with
//! typed rows ([`TableWriter::append_row`]) instead of lines; its pages,
//! files and manifest carry exactly its schema (see [`super::record`]).
//!
//! This is the single-threaded core; #574 wraps it as the public `Table`
//! with an ingest thread and input sources.

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::flush;
use super::layout::{archive_dir, day_of_hour, hour_of, live_dir, Area, Layout, LayoutKind};
use super::lock::{LockError, TableLock};
use super::manifest::{FileEntry, Manifest, ManifestError};
use super::page::{decode_record, encode_record, invalid_line, HeadPage, Page};
use super::record::{check_row, decode_row, encode_row, invalid_schema, RecordValue};
use super::snapshot::Snapshot;
use super::tier::{self, Output};
use super::wal::{Ack, Wal, WalError, WalOptions, Watermarks};
use crate::storage::column::{Vfs, VfsFile};
use crate::storage::parquet::writer::{ColumnSpec, WriteError, WriterOptions};
use crate::storage::stream::adapter::now_ns;
use crate::storage::stream::detect::{DetectedParser, Format};
use crate::storage::stream::{Source, SourceKind};
use crate::storage::PosixVfs;

/// Live Parquet files, relative to the table directory.
pub const DATA_DIR: &str = "data";
/// Files being written by a flush.
pub const TMP_DIR: &str = "tmp";

/// How a table is written.
#[derive(Debug, Clone)]
pub struct TableOptions {
    /// The record format, fixed when the table is created. Unused by a
    /// record table.
    pub format: Format,
    /// A record table's schema (#594), fixed when the table is created:
    /// rows are appended with [`TableWriter::append_row`] and flushed as
    /// exactly these columns. `None` (the default) is a lines table.
    pub schema: Option<Vec<ColumnSpec>>,
    /// WAL settings.
    pub wal: WalOptions,
    /// Parquet writer settings for flushed files.
    pub parquet: WriterOptions,
    /// Where files go; the choice is fixed when the table is created.
    pub layout: Layout,
    /// Arrival time in nanoseconds since the epoch: `observed_ts`, and the
    /// hour and day a tiered table partitions by. The system clock by
    /// default; tests and imports set their own.
    pub clock: fn() -> i64,
}

impl Default for TableOptions {
    /// JSON Lines records, default WAL and Parquet settings, flat, the
    /// system clock.
    fn default() -> Self {
        TableOptions {
            format: Format::Jsonl,
            schema: None,
            wal: WalOptions::default(),
            parquet: WriterOptions::default(),
            layout: Layout::Flat,
            clock: now_ns,
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
    /// The table exists with another layout.
    LayoutMismatch {
        /// The table's layout.
        table: LayoutKind,
        /// The layout asked for.
        requested: LayoutKind,
    },
    /// A record table's schema (or a tiered one's sort key) is not valid.
    InvalidSchema(String),
    /// The table exists with another schema, or is a lines table opened
    /// as a record table or the other way round.
    SchemaMismatch,
    /// The operation is not available for this table's layout or kind.
    Unsupported(String),
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
            IngestError::LayoutMismatch { table, requested } => write!(
                f,
                "the table's layout is {}, not {}",
                table.as_str(),
                requested.as_str()
            ),
            IngestError::InvalidSchema(why) => write!(f, "invalid record schema: {why}"),
            IngestError::SchemaMismatch => {
                write!(f, "the table's schema is not the one asked for")
            }
            IngestError::Unsupported(why) => write!(f, "unsupported: {why}"),
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
    /// Files a compaction replaced, deleted at the time given (ns).
    pending_deletes: Vec<(PathBuf, i64)>,
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
        if let Some(schema) = &options.schema {
            if let Some(why) = invalid_schema(schema) {
                return Err(IngestError::InvalidSchema(why));
            }
            if let Layout::Tiered(tier) = &options.layout {
                if let Some(key) = tier
                    .sort_key
                    .iter()
                    .find(|k| !schema.iter().any(|s| s.name == **k))
                {
                    return Err(IngestError::InvalidSchema(format!(
                        "sort key {key} is not a column"
                    )));
                }
            }
        }
        let manifest = match Manifest::read(table)? {
            Some(m) => m,
            None => {
                let mut m = Manifest::new(options.format, options.layout.kind());
                m.schema.clone_from(&options.schema);
                m.write(table)?;
                m
            }
        };
        if manifest.schema != options.schema {
            return Err(IngestError::SchemaMismatch);
        }
        if manifest.schema.is_none() && manifest.format != options.format {
            return Err(IngestError::FormatMismatch {
                table: manifest.format,
                requested: options.format,
            });
        }
        if manifest.layout != options.layout.kind() {
            return Err(IngestError::LayoutMismatch {
                table: manifest.layout,
                requested: options.layout.kind(),
            });
        }
        clean_up(table, &manifest)?;
        let (wal, records) =
            Wal::open_locked(lock, table, manifest.flushed_lsn, options.wal.clone())?;
        let head = match &manifest.schema {
            Some(schema) => HeadPage::for_records(Arc::from(schema.as_slice())),
            None => HeadPage::default(),
        };
        let mut writer = TableWriter {
            dir: table.to_path_buf(),
            wal,
            parser: DetectedParser::for_format(manifest.format),
            source: Source::new(SourceKind::Stdin, &table.to_string_lossy()),
            manifest,
            options,
            head,
            sealed: Vec::new(),
            last_observed: i64::MIN,
            pending_deletes: Vec::new(),
            #[cfg(test)]
            crash_after: None,
        };
        for (lsn, payload) in records {
            let corrupt = || IngestError::Corrupt(format!("WAL record {lsn} is not a record"));
            let (observed, body) = decode_record(&payload).ok_or_else(corrupt)?;
            writer.last_observed = writer.last_observed.max(observed);
            match writer.manifest.schema.as_deref() {
                Some(schema) => {
                    let row = decode_row(body)
                        .filter(|row| check_row(schema, row).is_none())
                        .ok_or_else(corrupt)?;
                    writer.push(observed, |head| head.push_row(lsn, observed, row))?;
                }
                None => writer.push(observed, |head| head.push(lsn, observed, body))?,
            }
        }
        Ok(writer)
    }

    /// Adds a record that arrived at `observed` to the head with `add` (a
    /// line or a row): a tiered table first seals a head of an earlier
    /// hour, so no page spans an hour.
    fn push(&mut self, observed: i64, add: impl FnOnce(&mut HeadPage)) -> Result<(), IngestError> {
        let other_hour = self
            .head
            .first_observed()
            .is_some_and(|first| hour_of(first) != hour_of(observed));
        if other_hour && self.options.layout.kind() == LayoutKind::Tiered {
            self.seal_head()?;
        }
        add(&mut self.head);
        if self.head.is_full() {
            self.seal_head()?;
        }
        Ok(())
    }

    /// The latest arrival time seen or now, whichever is later.
    fn now(&self) -> i64 {
        (self.options.clock)().max(self.last_observed)
    }

    /// Appends one record (a line, without its newline) and waits for
    /// `ack`. `Ack::Flushed` seals and flushes everything up to it before
    /// returning. Returns the record's LSN.
    pub fn append(&mut self, line: &[u8], ack: Ack) -> Result<u64, IngestError> {
        if self.manifest.schema.is_some() {
            return Err(IngestError::Unsupported(
                "append on a record table, which takes typed rows: append_row".to_string(),
            ));
        }
        if let Some(why) = invalid_line(line) {
            return Err(IngestError::InvalidRecord(why));
        }
        self.append_record(line, ack, |head, lsn, observed| {
            head.push(lsn, observed, line);
        })
    }

    /// Appends one row to a record table (#594) and waits for `ack`, like
    /// [`append`](Self::append). The row must match the table's schema:
    /// one value per column, of its type and in its range, NULL only in a
    /// nullable column; nothing is converted. Returns the row's LSN.
    pub fn append_row(&mut self, row: Vec<RecordValue>, ack: Ack) -> Result<u64, IngestError> {
        let Some(schema) = self.manifest.schema.as_deref() else {
            return Err(IngestError::Unsupported(
                "append_row on a lines table, which takes lines: append".to_string(),
            ));
        };
        if let Some(why) = check_row(schema, &row) {
            return Err(IngestError::InvalidRecord(why));
        }
        let body = encode_row(&row)
            .ok_or_else(|| IngestError::InvalidRecord("a value is over 4 GiB".to_string()))?;
        self.append_record(&body, ack, |head, lsn, observed| {
            head.push_row(lsn, observed, row);
        })
    }

    /// The shared tail of [`append`](Self::append) and
    /// [`append_row`](Self::append_row): `body` into the WAL, then `add`
    /// puts the record into the head at its LSN and arrival time.
    fn append_record(
        &mut self,
        body: &[u8],
        ack: Ack,
        add: impl FnOnce(&mut HeadPage, u64, i64),
    ) -> Result<u64, IngestError> {
        if ack == Ack::Flushed && self.options.layout.kind() == LayoutKind::Tiered {
            return Err(IngestError::Unsupported(
                "Ack::Flushed on a tiered table: a record is flushed when its hour closes"
                    .to_string(),
            ));
        }
        // Arrival time, never going backwards (ADR 0027).
        let observed = self.now();
        let wal_ack = match ack {
            Ack::Flushed => Ack::Durable,
            other => other,
        };
        let lsn = self.wal.append(&encode_record(observed, body), wal_ack)?;
        self.last_observed = observed;
        self.push(observed, |head| add(head, lsn, observed))?;
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

    /// Flushes and commits; returns the files it added (none when nothing
    /// was ready). A flat table writes every sealed page to one file in
    /// `data/`. A tiered table writes each closed hour (one before the
    /// latest arrival or the clock's hour) to `live/`, sealing the head
    /// first if its hour has closed.
    pub fn flush(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        match self.options.layout.kind() {
            LayoutKind::Flat => Ok(self.flush_flat()?.into_iter().collect()),
            LayoutKind::Tiered => self.flush_hours(),
        }
    }

    fn flush_flat(&mut self) -> Result<Option<FileEntry>, IngestError> {
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
        let written = flush::write_file(
            &tmp,
            &self.sealed,
            self.options.format,
            &self.options.parquet,
        )?;
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
            area: Area::Data,
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

    /// Seals the head and flushes everything that can be: all of a flat
    /// table, the closed hours of a tiered one.
    pub fn flush_all(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.seal_head()?;
        self.flush()
    }

    /// The tiered flush: every closed hour, one commit each.
    fn flush_hours(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.delete_expired()?;
        let open_hour = hour_of(self.now());
        if self
            .head
            .first_observed()
            .is_some_and(|t| hour_of(t) < open_hour)
        {
            self.seal_head()?;
        }
        let mut added = Vec::new();
        while let Some(hour) = self.sealed.first().and_then(page_hour) {
            if hour >= open_hour {
                break;
            }
            let count = self
                .sealed
                .iter()
                .take_while(|p| page_hour(p) == Some(hour))
                .count();
            added.extend(self.flush_hour(hour, count)?);
        }
        Ok(added)
    }

    /// Writes the first `count` sealed pages (hour `hour`) to `live/` and
    /// commits them: ADR 0027's five steps, with a directory as the output.
    fn flush_hour(&mut self, hour: i64, count: usize) -> Result<Vec<FileEntry>, IngestError> {
        let Layout::Tiered(tier_options) = &self.options.layout else {
            return Ok(Vec::new());
        };
        let pages = self.sealed.get(..count).unwrap_or_default();
        let (Some(first), Some(last)) = (pages.first(), pages.last()) else {
            return Ok(Vec::new());
        };
        let lsn_hi = last.lsn_hi;
        self.wal.sync()?;
        let id = format!("{:020}-{lsn_hi:020}", first.lsn_lo);

        // 1. Write under tmp/.
        let tmp = self.dir.join(TMP_DIR).join(&id);
        fs::create_dir_all(&tmp)?;
        let outputs = tier::write_hour(
            &tmp,
            pages,
            self.options.format,
            tier_options,
            &self.options.parquet,
        )?;
        self.crash_point(1)?;

        // 2. Publish the hour's directory.
        let target = live_dir(hour);
        publish(&self.dir, &tmp, &target)?;
        self.crash_point(2)?;

        // 3. Commit.
        let entries = entries(&target, Area::Live, outputs);
        let mut next = self.manifest.clone();
        for entry in &entries {
            next = next.with_file(entry.clone());
        }
        next.write(&self.dir)?;
        self.crash_point(3)?;

        // 4. Adopt; 5. release the WAL and the pages.
        self.manifest = next;
        self.wal.mark_flushed(lsn_hi)?;
        self.sealed.drain(..count);
        self.crash_point(5)?;
        Ok(entries)
    }

    /// Flushes every closed hour, then merges every closed day's hours in
    /// `live/` into `archive/` (ADR 0028), one commit per day, and deletes
    /// files replaced long enough ago. Returns the files it added. Nothing
    /// for a flat table.
    pub fn compact(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        let Layout::Tiered(tier_options) = self.options.layout.clone() else {
            return Ok(Vec::new());
        };
        // Flush every closed hour first: a day is merged only once all of
        // it is in live/, so its archive directory is published once.
        let mut added = self.flush_hours()?;
        let open_day = day_of_hour(hour_of(self.now()));
        let day_of = |e: &FileEntry| e.observed_ts.map(|(lo, _)| day_of_hour(hour_of(lo)));
        loop {
            let files = &self.manifest.files;
            let Some(start) = files.iter().position(|e| e.area == Area::Live) else {
                break;
            };
            let day = files.get(start).and_then(day_of);
            let Some(day) = day.filter(|d| *d < open_day) else {
                break;
            };
            let end = files
                .iter()
                .skip(start)
                .position(|e| day_of(e) != Some(day))
                .map_or(files.len(), |n| start.saturating_add(n));
            added.extend(self.merge_day(day, start..end, &tier_options)?);
        }
        Ok(added)
    }

    /// Merges `files[range]` (day `day`'s hours) into `archive/`.
    fn merge_day(
        &mut self,
        day: i64,
        range: std::ops::Range<usize>,
        tier_options: &super::layout::TierOptions,
    ) -> Result<Vec<FileEntry>, IngestError> {
        let inputs: Vec<FileEntry> = self
            .manifest
            .files
            .get(range.clone())
            .unwrap_or_default()
            .to_vec();
        let (Some(first), Some(last)) = (inputs.first(), inputs.last()) else {
            return Ok(Vec::new());
        };
        let id = format!("{:020}-{:020}", first.lsn_lo, last.lsn_hi);
        let maps = inputs
            .iter()
            .map(|e| PosixVfs.open(&self.dir.join(&e.path))?.mmap())
            .collect::<io::Result<Vec<_>>>()?;
        let input_bytes: Vec<(&FileEntry, &[u8])> =
            inputs.iter().zip(maps.iter().map(|m| &**m)).collect();

        // 1. Write under tmp/.
        let tmp = self.dir.join(TMP_DIR).join(&id);
        fs::create_dir_all(&tmp)?;
        let outputs = tier::merge_day(
            &tmp,
            &input_bytes,
            self.manifest.schema.as_deref(),
            tier_options,
            &self.options.parquet,
        )?;
        self.crash_point(11)?;

        // 2. Publish the day's directory.
        let target = archive_dir(day);
        publish(&self.dir, &tmp, &target)?;
        self.crash_point(12)?;

        // 3. Commit: the inputs out, the outputs in, in one swap.
        let entries = entries(&target, Area::Archive, outputs);
        let next = self.manifest.replacing(range, entries.clone());
        next.write(&self.dir)?;
        self.crash_point(13)?;
        self.manifest = next;

        // 4. Delete the inputs after the grace period.
        let at = self.now().saturating_add(tier_options.delete_grace_ns);
        self.pending_deletes
            .extend(inputs.iter().map(|e| (self.dir.join(&e.path), at)));
        Ok(entries)
    }

    /// Deletes replaced files whose grace period is over, and their
    /// directories once empty.
    fn delete_expired(&mut self) -> Result<(), IngestError> {
        let now = self.now();
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_deletes)
            .into_iter()
            .partition(|(_, at)| *at <= now);
        self.pending_deletes = later;
        for (path, _) in due {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            remove_empty_parents(&self.dir, &path);
        }
        Ok(())
    }

    /// Files a compaction replaced that are still on disk, waiting out the
    /// grace period.
    pub fn pending_deletes(&self) -> impl Iterator<Item = &Path> {
        self.pending_deletes.iter().map(|(p, _)| p.as_path())
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

/// The hour a page's records arrived in (a tiered table's pages never
/// span an hour).
fn page_hour(page: &Page) -> Option<i64> {
    page.first_observed().map(hour_of)
}

/// Renames `tmp` (a finished output directory) to `target` under `table`,
/// creating `target`'s parent and fsyncing it.
fn publish(table: &Path, tmp: &Path, target: &str) -> Result<(), IngestError> {
    let target = table.join(target);
    let parent = target
        .parent()
        .ok_or_else(|| IngestError::Corrupt(format!("{} has no parent", target.display())))?;
    fs::create_dir_all(parent)?;
    fs::rename(tmp, &target)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Manifest entries for an output published at `dir` in `area`.
fn entries(dir: &str, area: Area, outputs: Vec<Output>) -> Vec<FileEntry> {
    outputs
        .into_iter()
        .map(|o| FileEntry {
            path: format!("{dir}/{}", o.name),
            area,
            lsn_lo: o.lsn_lo,
            lsn_hi: o.lsn_hi,
            rows: o.file.rows,
            bytes: o.file.bytes,
            observed_ts: o.file.observed_ts,
            timestamp: o.file.timestamp,
        })
        .collect()
}

/// Removes `path`'s parent directories under `table` while they are empty
/// (an hour's, then a day's), up to the area directory.
fn remove_empty_parents(table: &Path, path: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == table || d.parent() == Some(table) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// Recovery steps 1-2: empty `tmp/`, delete the Parquet files in `data/`,
/// `live/` and `archive/` the manifest does not list (a flush or
/// compaction published but not committed, or inputs a compaction replaced
/// and had not deleted yet), and the directories left empty.
fn clean_up(table: &Path, manifest: &Manifest) -> Result<(), IngestError> {
    let tmp = table.join(TMP_DIR);
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    for area in [Area::Data, Area::Live, Area::Archive] {
        clean_dir(&table.join(area.as_str()), area.as_str(), manifest)?;
    }
    Ok(())
}

/// [`clean_up`] for one directory, `relative` to the table; returns whether
/// it is left empty.
fn clean_dir(dir: &Path, relative: &str, manifest: &Manifest) -> Result<bool, IngestError> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(false);
    };
    let mut empty = true;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{relative}/{name}");
        if entry.file_type()?.is_dir() {
            if clean_dir(&entry.path(), &path, manifest)? {
                fs::remove_dir(entry.path())?;
            } else {
                empty = false;
            }
        } else if manifest.files.iter().any(|f| f.path == path) {
            empty = false;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(empty)
}

#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tiered_tests;
