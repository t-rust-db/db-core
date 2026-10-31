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
//! passed. A tiered table keeps its open hour in pages until more than
//! `spill_rows` rows are sealed, then spills them to `staging/` as a sorted
//! run (write, publish, commit, release, like a flush); the hour's close
//! merges its runs and remaining pages into one file in `live/`.
//!
//! A record table (#594, [`TableOptions::schema`]) is appended to with
//! typed rows ([`TableWriter::append_row`]) instead of lines; its pages,
//! files and manifest carry exactly its schema (see [`super::record`]).
//!
//! This is the single-threaded core; [`super::Table`] (#574) is the
//! public, cloneable handle over it, with the line sources.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

pub use super::error::IngestError;
use super::layout::{hour_of, Area, Layout, LayoutKind};
use super::lock::{LockError, TableLock};
use super::manifest::{FileEntry, Manifest};
use super::page::{decode_record, encode_record, invalid_line, HeadPage, Page};
use super::record::{
    check_row, decode_row, encode_row, extends_schema, invalid_schema, join_repeated_keys, pad_row,
    RecordValue,
};
use super::snapshot::Snapshot;
use super::tier::Output;
use super::wal::{Ack, Wal, WalError, WalOptions, Watermarks};
use crate::storage::parquet::writer::{ColumnSpec, ColumnType, WriterOptions};
use crate::storage::stream::adapter::now_ns;
use crate::storage::stream::detect::{DetectedParser, Format};
use crate::storage::stream::{Source, SourceKind};
use flusher::{failure, Drain, Flusher, Job, Runner, Shared, Work};

/// Live Parquet files, relative to the table directory.
pub const DATA_DIR: &str = "data";
/// Files being written by a flush.
pub const TMP_DIR: &str = "tmp";
/// Largest row group of a spill to `staging/`: an hour's close holds one
/// row group of each of its runs.
const STAGING_ROW_GROUP_ROWS: usize = 65_536;

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
    /// How many flush jobs (a closed hour, a spill, a day merge) may wait
    /// for the background flusher (#855). `0` (the default) flushes on the
    /// appending thread: a closed hour blocks the next append until it is
    /// written. With `n > 0` the hand-off returns at once and an append
    /// waits only when `n` jobs are already queued; see
    /// [`TableWriter::flush_status`].
    ///
    /// The default stays `0` (#864): queued pages stay in memory until
    /// their commit, so `n > 0` trades memory for append latency. A live
    /// ingester that cannot absorb a flush stall should set `2`.
    pub flush_queue: usize,
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
            flush_queue: 0,
        }
    }
}

/// Owns a table directory for writing; see the [module docs](self).
#[derive(Debug)]
pub struct TableWriter {
    // Declared before `wal` so the flusher is stopped, and lets go of the
    // WAL, before the writer's own handle drops.
    runner: Runner,
    shared: Arc<Shared>,
    dir: PathBuf,
    wal: Arc<Wal>,
    options: TableOptions,
    parser: DetectedParser,
    source: Source,
    head: HeadPage,
    /// Sealed pages not yet handed to the flusher, oldest first.
    sealed: Vec<Page>,
    last_observed: i64,
    /// The hour whose runs are in `staging/` or queued to go there.
    spilled_hour: Option<i64>,
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
                let sortable = |k: &String| {
                    schema.iter().any(|s| {
                        s.name == *k
                            && !matches!(s.column_type, ColumnType::MapUtf8 | ColumnType::ListUtf8)
                    })
                };
                if let Some(key) = tier.sort_key.iter().find(|k| !sortable(k)) {
                    return Err(IngestError::InvalidSchema(format!(
                        "sort key {key} is not a column that sorts (a MAP or LIST does not)"
                    )));
                }
                if let Some(key) = &tier.session_key {
                    let integer = schema.iter().any(|s| {
                        s.name == key.column
                            && matches!(s.column_type, ColumnType::Int64 | ColumnType::UInt64)
                    });
                    if !integer {
                        return Err(IngestError::InvalidSchema(format!(
                            "session key {} is not an Int64 or UInt64 column",
                            key.column
                        )));
                    }
                }
            }
        }
        let mut manifest = match Manifest::read(table)? {
            Some(m) => m,
            None => {
                let mut m = Manifest::new(options.format, options.layout.kind());
                m.schema.clone_from(&options.schema);
                m.write(table)?;
                m
            }
        };
        if manifest.schema != options.schema {
            // Only columns appended at the end, all nullable, are accepted
            // (#740); the new schema is committed before anything is
            // appended, so a crash leaves a table that opens either way.
            let (Some(stored), Some(requested)) = (&manifest.schema, &options.schema) else {
                return Err(IngestError::SchemaMismatch);
            };
            if !extends_schema(stored, requested) {
                return Err(IngestError::SchemaMismatch);
            }
            manifest.schema.clone_from(&options.schema);
            manifest.evolved = true;
            manifest.write(table)?;
        }
        // Like the other tier options the session key may change between
        // opens; readers see it through the manifest, so it is committed
        // before anything is appended.
        let session_key = match &options.layout {
            Layout::Tiered(tier) => tier.session_key.clone(),
            Layout::Flat => None,
        };
        if manifest.session_key != session_key {
            manifest.session_key = session_key;
            manifest.write(table)?;
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
        let wal = Arc::new(wal);
        let shared = Arc::new(Shared::new(manifest.clone()));
        let spilled_hour = staging_hour(&manifest);
        let flusher = Flusher::new(
            table.to_path_buf(),
            Arc::clone(&wal),
            options.clone(),
            manifest.clone(),
            Arc::clone(&shared),
            options.flush_queue > 0,
        );
        let runner = if options.flush_queue > 0 {
            Runner::spawn(flusher)?
        } else {
            Runner::Inline(Box::new(flusher))
        };
        let head = match &manifest.schema {
            Some(schema) => HeadPage::for_records(Arc::from(schema.as_slice())),
            None => HeadPage::default(),
        };
        let mut writer = TableWriter {
            runner,
            shared,
            dir: table.to_path_buf(),
            wal,
            parser: DetectedParser::for_format(manifest.format),
            source: Source::new(SourceKind::Stdin, &table.to_string_lossy()),
            options,
            head,
            sealed: Vec::new(),
            last_observed: i64::MIN,
            spilled_hour,
            #[cfg(test)]
            crash_after: None,
        };
        for (lsn, payload) in records {
            let corrupt = || IngestError::Corrupt(format!("WAL record {lsn} is not a record"));
            let (observed, body) = decode_record(&payload).ok_or_else(corrupt)?;
            writer.last_observed = writer.last_observed.max(observed);
            match writer.options.schema.as_deref() {
                Some(schema) => {
                    let row = decode_row(body)
                        .map(|row| pad_row(schema, row))
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
    /// returning (a tiered table's open hour to `staging/`, one file per
    /// call). Returns the record's LSN.
    pub fn append(&mut self, line: &[u8], ack: Ack) -> Result<u64, IngestError> {
        if self.options.schema.is_some() {
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
        let Some(schema) = self.options.schema.as_deref() else {
            return Err(IngestError::Unsupported(
                "append_row on a lines table, which takes lines: append".to_string(),
            ));
        };
        if let Some(why) = check_row(schema, &row) {
            return Err(IngestError::InvalidRecord(why));
        }
        let row = join_repeated_keys(row);
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
        self.check_flusher()?;
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
            self.spill()?;
            self.drain()?;
        }
        Ok(lsn)
    }

    /// Seals the head into a page (nothing if empty) and starts a new WAL
    /// segment, so WAL files line up with pages. A tiered table with more
    /// than `spill_rows` rows (or `spill_bytes` bytes) sealed then closes its
    /// closed hours and spills the open one to `staging/`; with a background flusher
    /// (`flush_queue`) that hands the work over and returns.
    pub fn seal_head(&mut self) -> Result<(), IngestError> {
        self.seal()?;
        if let Layout::Tiered(tier) = &self.options.layout {
            let sealed: usize = self.sealed.iter().map(Page::len).sum();
            let over_bytes = tier.spill_bytes.is_some_and(|limit| {
                self.sealed.iter().map(Page::byte_size).sum::<usize>() > limit
            });
            if sealed > tier.spill_rows || over_bytes {
                self.flush_hours()?;
                self.spill()?;
            }
        }
        Ok(())
    }

    /// [`seal_head`](Self::seal_head) without the spill.
    fn seal(&mut self) -> Result<(), IngestError> {
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

    /// Hands `pages` (taken from `sealed`) to the flusher as `work`. Inline
    /// it runs now and returns the files it added; in the background it
    /// returns at once, after waiting for room in the queue, and the files
    /// come with a later [`flush`](Self::flush) or
    /// [`drain`](Self::drain).
    fn submit(&mut self, work: Work, pages: Vec<Page>) -> Result<Vec<FileEntry>, IngestError> {
        let rows: usize = pages.iter().map(Page::len).sum();
        let pages = Arc::new(pages);
        {
            let mut view = self.shared.lock();
            let cap = match self.runner {
                Runner::Inline(_) => usize::MAX,
                Runner::Background { .. } => self.options.flush_queue,
            };
            let waited = Instant::now();
            let mut stalled = false;
            loop {
                if view.failed.is_some() {
                    break;
                }
                if view.queued < cap {
                    break;
                }
                stalled = true;
                view = self
                    .shared
                    .changed
                    .wait(view)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if stalled {
                let ns = u64::try_from(waited.elapsed().as_nanos()).unwrap_or(u64::MAX);
                view.stalled_ns = view.stalled_ns.saturating_add(ns);
            }
            if let Some(why) = &view.failed {
                let err = failure(why);
                drop(view);
                self.reclaim_unsent(&pages);
                return Err(err);
            }
            view.queued = view.queued.saturating_add(1);
            view.queued_rows = view.queued_rows.saturating_add(rows);
            if !pages.is_empty() {
                view.inflight.push_back(Arc::clone(&pages));
            }
        }
        let job = Job {
            work,
            pages,
            now: self.now(),
            #[cfg(test)]
            crash_after: self.crash_after,
        };
        match &mut self.runner {
            Runner::Inline(flusher) => match flusher.run(job) {
                Ok(added) => Ok(added),
                Err(e) => {
                    self.reclaim();
                    Err(e)
                }
            },
            Runner::Background { tx, .. } => {
                let sent = tx.as_ref().is_some_and(|tx| tx.send(job).is_ok());
                if sent {
                    return Ok(Vec::new());
                }
                // The thread is gone: its reason is in the view.
                let view = self.shared.lock();
                Err(failure(
                    view.failed.as_deref().unwrap_or("the flusher stopped"),
                ))
            }
        }
    }

    /// After a failed inline job: puts the pages it still holds (the
    /// commit never adopted them) back at the front of `sealed`, so the
    /// next flush retries them.
    fn reclaim(&mut self) {
        let pages: Vec<Page> = {
            let mut view = self.shared.lock();
            view.queued = 0;
            view.queued_rows = 0;
            view.inflight
                .drain(..)
                .flat_map(|p| p.iter().cloned().collect::<Vec<_>>())
                .collect()
        };
        self.sealed.splice(..0, pages);
    }

    /// A job that was never queued (the flusher had stopped): its pages go
    /// back to `sealed`.
    fn reclaim_unsent(&mut self, pages: &[Page]) {
        self.sealed.splice(..0, pages.iter().cloned());
    }

    /// The error a stopped background flusher left, if it stopped.
    fn check_flusher(&self) -> Result<(), IngestError> {
        match &self.shared.lock().failed {
            Some(why) => Err(failure(why)),
            None => Ok(()),
        }
    }

    /// The files the background flusher committed since the last call.
    fn collect(&self) -> Vec<FileEntry> {
        std::mem::take(&mut self.shared.lock().done)
    }

    /// Waits until the background flusher has committed every job handed
    /// to it; returns the files it committed since the last call. Nothing
    /// to wait for with an inline flusher.
    pub fn drain(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.shared.drain()
    }

    /// Makes the flusher look busy with `n` jobs until the next commit.
    #[cfg(test)]
    pub(crate) fn hold_queue(&self, n: usize) {
        self.shared.lock().queued = n;
        self.shared.changed.notify_all();
    }

    /// Waits for the queued jobs without borrowing the writer.
    pub(crate) fn drain_handle(&self) -> Drain {
        Drain::new(&self.shared)
    }

    /// How far the background flusher is behind: see [`FlushStatus`].
    pub fn flush_status(&self) -> FlushStatus {
        let view = self.shared.lock();
        FlushStatus {
            queued_jobs: view.queued,
            queued_rows: view.queued_rows,
            stalled: Duration::from_nanos(view.stalled_ns),
        }
    }

    /// Flushes and commits; returns the files it added (none when nothing
    /// was ready). A flat table writes every sealed page to one file in
    /// `data/`. A tiered table writes each closed hour (one before the
    /// latest arrival or the clock's hour) to `live/`, sealing the head
    /// first if its hour has closed.
    ///
    /// With a background flusher (`flush_queue`) this only hands the work
    /// over: it returns the files committed so far, not the ones queued
    /// behind it; [`drain`](Self::drain) waits for those.
    pub fn flush(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        let mut added = match self.options.layout.kind() {
            LayoutKind::Flat => self.flush_flat()?,
            LayoutKind::Tiered => self.flush_hours()?,
        };
        added.extend(self.collect());
        Ok(added)
    }

    fn flush_flat(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        if self.sealed.is_empty() {
            return Ok(Vec::new());
        }
        let pages = std::mem::take(&mut self.sealed);
        self.submit(Work::Flat, pages)
    }

    /// Seals the head and flushes everything that can be: all of a flat
    /// table, the closed hours of a tiered one. Waits for the background
    /// flusher, if any, to commit it.
    pub fn flush_all(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        let mut added = self.flush_submit()?;
        added.extend(self.drain()?);
        Ok(added)
    }

    /// [`flush_all`](Self::flush_all) without the wait: seals the head and
    /// hands everything to the flusher.
    pub(crate) fn flush_submit(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.seal_head()?;
        self.flush()
    }

    /// The tiered flush: every closed hour, one commit each.
    fn flush_hours(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        self.check_flusher()?;
        let open_hour = hour_of(self.now());
        if self
            .head
            .first_observed()
            .is_some_and(|t| hour_of(t) < open_hour)
        {
            self.seal()?;
        }
        let mut added = Vec::new();
        let mut submitted = false;
        // The hour that spilled comes first: its runs hold the oldest LSNs.
        while let Some(hour) = self
            .spilled_hour
            .or_else(|| self.sealed.first().and_then(page_hour))
        {
            if hour >= open_hour {
                break;
            }
            let count = self
                .sealed
                .iter()
                .take_while(|p| page_hour(p) == Some(hour))
                .count();
            let pages: Vec<Page> = self.sealed.drain(..count).collect();
            added.extend(self.submit(Work::Close { hour }, pages)?);
            self.spilled_hour = None;
            submitted = true;
        }
        if submitted {
            return Ok(added);
        }
        // Nothing to close: still delete what the grace period released.
        if !self.shared.lock().pending_deletes.is_empty() {
            added.extend(self.submit(Work::Sweep, Vec::new())?);
        }
        Ok(added)
    }

    /// Writes the sealed pages (by now all of the open hour) to `staging/`
    /// as one sorted run and commits it like a flush: write under `tmp/`,
    /// rename into the hour's staging directory, commit, release the WAL
    /// and the pages.
    fn spill(&mut self) -> Result<(), IngestError> {
        if !matches!(self.options.layout, Layout::Tiered(_)) {
            return Ok(());
        }
        let Some(hour) = self.sealed.first().and_then(page_hour) else {
            return Ok(());
        };
        let pages = std::mem::take(&mut self.sealed);
        self.submit(Work::Spill, pages)?;
        self.spilled_hour = Some(hour);
        Ok(())
    }

    /// Flushes every closed hour, then merges every closed day's hours in
    /// `live/` into `archive/` (ADR 0028), one commit per day, drops the
    /// days `retention_days` no longer keeps, and deletes files replaced or
    /// dropped long enough ago. Returns the files it added. Nothing for a
    /// flat table.
    ///
    /// With a background flusher (`flush_queue`) the merge runs off the
    /// caller's thread: this returns the files committed so far, and
    /// [`drain`](Self::drain) waits for the rest.
    pub fn compact(&mut self) -> Result<Vec<FileEntry>, IngestError> {
        if !matches!(self.options.layout, Layout::Tiered(_)) {
            return Ok(Vec::new());
        }
        // Flush every closed hour first: a day is merged only once all of
        // it is in live/, so its archive directory is published once.
        let mut added = self.flush_hours()?;
        added.extend(self.submit(Work::Compact, Vec::new())?);
        added.extend(self.collect());
        Ok(added)
    }

    /// Files a compaction replaced that are still on disk, waiting out the
    /// grace period.
    pub fn pending_deletes(&self) -> impl Iterator<Item = PathBuf> {
        self.shared.lock().pending_deletes.clone().into_iter()
    }

    /// The committed manifest.
    pub fn manifest(&self) -> Manifest {
        self.shared.lock().manifest.clone()
    }

    /// Waits until everything appended is durable; returns the durable
    /// LSN.
    pub fn sync(&self) -> Result<u64, IngestError> {
        Ok(self.wal.sync()?)
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
        // The manifest and the pages its next commit will absorb, in one
        // step, so no row is in both or in neither.
        let (manifest, inflight) = {
            let view = self.shared.lock();
            (view.manifest.clone(), view.inflight.clone())
        };
        let pages = inflight
            .iter()
            .flat_map(|pages| pages.iter().cloned())
            .chain(self.sealed.iter().cloned())
            .chain(self.head_page())
            .collect();
        Snapshot::new(&self.dir, manifest, pages)
    }

    /// The table directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Makes everything appended durable and closes the WAL. Jobs handed
    /// to the background flusher finish first; pages not flushed stay in
    /// the WAL for the next open.
    pub fn close(self) -> Result<(), IngestError> {
        let TableWriter {
            mut runner,
            shared,
            wal,
            ..
        } = self;
        runner.finish();
        drop(runner);
        let failed = shared.lock().failed.clone();
        let wal = Arc::try_unwrap(wal).map_err(|_| {
            IngestError::Corrupt("the flusher still holds the WAL at close".to_string())
        })?;
        wal.close()?;
        match failed {
            Some(why) => Err(failure(&why)),
            None => Ok(()),
        }
    }
}

/// How far the background flusher is behind (`flush_queue`, #855): the
/// bound on queued work is visible here instead of only as a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushStatus {
    /// Jobs handed over and not yet committed: closed hours, spills, day
    /// merges.
    pub queued_jobs: usize,
    /// Rows in the pages of those jobs, still held in memory.
    pub queued_rows: usize,
    /// Total time appends waited because the queue was full.
    pub stalled: Duration,
}

/// The hour whose runs are in `staging/`, if any.
fn staging_hour(manifest: &Manifest) -> Option<i64> {
    manifest
        .files
        .last()
        .filter(|e| e.area == Area::Staging)
        .and_then(|e| e.observed_ts)
        .map(|(lo, _)| hour_of(lo))
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
    super::create_dir_all_synced(parent)?;
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
            keys: o.file.keys,
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
/// `live/`, `archive/` and `staging/` the manifest does not list (a flush,
/// spill or compaction published but not committed, or inputs a
/// compaction or an hour's close replaced and had not deleted yet), and
/// the directories left empty.
fn clean_up(table: &Path, manifest: &Manifest) -> Result<(), IngestError> {
    let tmp = table.join(TMP_DIR);
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    for area in [Area::Data, Area::Live, Area::Archive, Area::Staging] {
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

mod flusher;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tiered_tests;
