//! The flush side of the writer (#855): everything that writes files and
//! commits the manifest, run inline or on a background thread.
//!
//! The writer thread seals pages and hands them over as [`Job`]s; the
//! [`Flusher`] runs them one at a time, in order, by ADR 0027's five steps,
//! and is the only code that writes the manifest after open, so commits
//! stay single-writer. Pages stay in the shared [`View`] until their commit
//! adopts the new manifest, in one step under the lock, so a snapshot sees
//! every row exactly once at any moment.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use super::{
    entries, page_hour, publish, remove_empty_parents, IngestError, TableOptions, DATA_DIR,
    STAGING_ROW_GROUP_ROWS, TMP_DIR,
};
use crate::storage::column::posix::PosixFile;
use crate::storage::column::Vfs;
use crate::storage::ingest::flush;
use crate::storage::ingest::layout::{
    archive_dir, day_of_hour, hour_of, live_dir, staging_dir, Area, Layout, TierOptions,
};
use crate::storage::ingest::manifest::{FileEntry, Manifest};
use crate::storage::ingest::page::Page;
use crate::storage::ingest::tier;
use crate::storage::ingest::wal::Wal;
use crate::storage::PosixVfs;

/// What a [`Job`] does.
#[derive(Debug, Clone, Copy)]
pub(super) enum Work {
    /// Write the pages to one file in `data/` (flat table).
    Flat,
    /// Write the pages to `staging/` as one sorted run (tiered table).
    Spill,
    /// Close `hour`: its runs and the pages into one file in `live/`.
    Close { hour: i64 },
    /// Merge the closed days into `archive/`, then apply retention.
    Compact,
    /// Delete the files whose grace period is over.
    Sweep,
}

/// One unit of flush work.
#[derive(Debug)]
pub(super) struct Job {
    pub work: Work,
    /// Sealed pages this job commits, oldest first; the same pages sit in
    /// [`View::inflight`] until the commit.
    pub pages: Arc<Vec<Page>>,
    /// The writer's clock when the job was submitted (ns).
    pub now: i64,
    #[cfg(test)]
    pub crash_after: Option<u8>,
}

/// What the writer thread and the flusher share.
#[derive(Debug)]
pub(super) struct Shared {
    view: Mutex<View>,
    changed: Condvar,
}

/// The committed manifest and the pages not yet in it.
#[derive(Debug)]
pub(super) struct View {
    pub manifest: Manifest,
    /// Pages handed to the flusher whose commit is pending, oldest first;
    /// one entry per job that carries pages.
    pub inflight: VecDeque<Arc<Vec<Page>>>,
    /// Jobs submitted and not finished.
    pub queued: usize,
    /// Rows in those jobs' pages.
    pub queued_rows: usize,
    /// Files committed in the background, not yet reported to the caller.
    pub done: Vec<FileEntry>,
    /// Replaced files still waiting out their grace period.
    pub pending_deletes: Vec<PathBuf>,
    /// Why the background flusher stopped, if it did.
    pub failed: Option<String>,
    /// Total time appends waited for room in the queue (ns).
    pub stalled_ns: u64,
}

impl Shared {
    pub fn new(manifest: Manifest) -> Shared {
        Shared {
            view: Mutex::new(View {
                manifest,
                inflight: VecDeque::new(),
                queued: 0,
                queued_rows: 0,
                done: Vec::new(),
                pending_deletes: Vec::new(),
                failed: None,
                stalled_ns: 0,
            }),
            changed: Condvar::new(),
        }
    }

    /// The view. A panicked flush left it as its last commit did, so a
    /// poisoned lock is still good.
    pub fn lock(&self) -> MutexGuard<'_, View> {
        self.view.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for the view to change.
    pub fn wait<'a>(&self, view: MutexGuard<'a, View>) -> MutexGuard<'a, View> {
        self.changed
            .wait(view)
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The error a stopped background flusher leaves for the writer.
pub(super) fn failure(why: &str) -> IngestError {
    IngestError::Io(io::Error::other(format!("background flush failed: {why}")))
}

/// Runs [`Job`]s, inline on the writer's thread or on a background one.
#[derive(Debug)]
pub(super) enum Runner {
    Inline(Box<Flusher>),
    Background {
        tx: Option<Sender<Job>>,
        thread: Option<JoinHandle<()>>,
    },
}

impl Runner {
    /// Starts the flusher thread, which runs jobs until the writer drops
    /// its sender, or one fails.
    pub fn spawn(flusher: Flusher) -> Result<Runner, IngestError> {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("db-core-flush".to_string())
            .spawn(move || flusher.run_all(&rx))?;
        Ok(Runner::Background {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// Stops the background thread after the jobs already sent; nothing for
    /// an inline runner.
    pub fn finish(&mut self) {
        if let Runner::Background { tx, thread } = self {
            tx.take();
            if let Some(thread) = thread.take() {
                // A panic is already in `View::failed` (see `StopGuard`).
                thread.join().ok();
            }
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Writes files and commits them to the manifest.
#[derive(Debug)]
pub(super) struct Flusher {
    dir: PathBuf,
    wal: Arc<Wal>,
    options: TableOptions,
    /// The manifest as last committed: the flusher's own copy, the only one
    /// that moves.
    manifest: Manifest,
    /// Files a compaction replaced, deleted at the time given (ns).
    pending_deletes: Vec<(PathBuf, i64)>,
    shared: Arc<Shared>,
    /// Whether commits are reported through [`View::done`] (background) or
    /// returned to the caller (inline).
    background: bool,
    #[cfg(test)]
    crash_after: Option<u8>,
}

/// Marks the flusher stopped if its thread exits by a panic, so a writer
/// waiting on the queue wakes with an error instead of for ever.
struct StopGuard<'a> {
    shared: &'a Shared,
    /// Set once the thread left its loop on its own.
    done: bool,
}

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut view = self.shared.lock();
        view.failed
            .get_or_insert_with(|| "the flush thread stopped".to_string());
        drop(view);
        self.shared.changed.notify_all();
    }
}

/// The staging directory's manifest entries: the open hour's runs, last.
fn staging_range(manifest: &Manifest) -> std::ops::Range<usize> {
    let files = &manifest.files;
    let start = files
        .iter()
        .position(|e| e.area == Area::Staging)
        .unwrap_or(files.len());
    start..files.len()
}

impl Flusher {
    pub fn new(
        dir: PathBuf,
        wal: Arc<Wal>,
        options: TableOptions,
        manifest: Manifest,
        shared: Arc<Shared>,
        background: bool,
    ) -> Flusher {
        Flusher {
            dir,
            wal,
            options,
            manifest,
            pending_deletes: Vec::new(),
            shared,
            background,
            #[cfg(test)]
            crash_after: None,
        }
    }

    /// The background thread: jobs in order until the writer hangs up or
    /// one fails (its error goes to the writer through the view).
    fn run_all(mut self, jobs: &Receiver<Job>) {
        let shared = Arc::clone(&self.shared);
        let mut guard = StopGuard {
            shared: &shared,
            done: false,
        };
        for job in jobs {
            if let Err(e) = self.run(job) {
                shared.lock().failed = Some(e.to_string());
                break;
            }
        }
        guard.done = true;
        shared.changed.notify_all();
    }

    /// Runs `job` and finishes it in the view; returns the files it added
    /// (none when they are reported through the view).
    pub fn run(&mut self, job: Job) -> Result<Vec<FileEntry>, IngestError> {
        let rows: usize = job.pages.iter().map(Page::len).sum();
        let result = self.execute(&job);
        let mut view = self.shared.lock();
        view.queued = view.queued.saturating_sub(1);
        view.queued_rows = view.queued_rows.saturating_sub(rows);
        view.pending_deletes = self
            .pending_deletes
            .iter()
            .map(|(p, _)| p.clone())
            .collect();
        let result = result.map(|added| {
            if self.background {
                view.done.extend(added);
                Vec::new()
            } else {
                added
            }
        });
        drop(view);
        self.shared.changed.notify_all();
        result
    }

    fn execute(&mut self, job: &Job) -> Result<Vec<FileEntry>, IngestError> {
        #[cfg(test)]
        {
            self.crash_after = job.crash_after;
        }
        let pages = job.pages.as_slice();
        match job.work {
            Work::Flat => Ok(self.flush_flat(pages)?.into_iter().collect()),
            Work::Spill => {
                self.spill(pages)?;
                Ok(Vec::new())
            }
            Work::Close { hour } => {
                self.delete_expired(job.now)?;
                self.flush_hour(hour, pages, job.now)
            }
            Work::Compact => {
                self.delete_expired(job.now)?;
                self.compact(job.now)
            }
            Work::Sweep => {
                self.delete_expired(job.now)?;
                Ok(Vec::new())
            }
        }
    }

    /// Adopts `next` as the committed manifest, and with `release` drops
    /// the job's pages from the view in the same step.
    fn adopt(&mut self, next: Manifest, release: bool) {
        let mut view = self.shared.lock();
        view.manifest = next.clone();
        if release {
            view.inflight.pop_front();
        }
        drop(view);
        self.manifest = next;
    }

    fn flush_flat(&mut self, pages: &[Page]) -> Result<Option<FileEntry>, IngestError> {
        let (Some(first), Some(last)) = (pages.first(), pages.last()) else {
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
        let written = flush::write_file(&tmp, pages, self.options.format, &self.options.parquet)?;
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
            keys: written.keys,
        };
        let next = self.manifest.with_file(entry.clone());
        next.write(&self.dir)?;
        self.crash_point(3)?;

        // 4. Adopt; 5. release the WAL and the pages.
        self.adopt(next, true);
        self.wal.mark_flushed(lsn_hi)?;
        self.crash_point(5)?;
        Ok(Some(entry))
    }

    /// Writes the pages (by now all of the open hour) to `staging/` as one
    /// sorted run and commits it like a flush: write under `tmp/`, rename
    /// into the hour's staging directory, commit, release the WAL and the
    /// pages.
    fn spill(&mut self, pages: &[Page]) -> Result<(), IngestError> {
        let Layout::Tiered(tier_options) = &self.options.layout else {
            return Ok(());
        };
        let (Some(first), Some(last)) = (pages.first(), pages.last()) else {
            return Ok(());
        };
        let Some(hour) = page_hour(first) else {
            return Ok(());
        };
        let lsn_hi = last.lsn_hi;
        self.wal.sync()?;
        let id = format!("{:020}-{lsn_hi:020}", first.lsn_lo);

        // 1. Write under tmp/, in small row groups: the hour's close holds
        //    one row group of each run.
        let tmp = self.dir.join(TMP_DIR).join(&id);
        fs::create_dir_all(&tmp)?;
        let run_options = TierOptions {
            row_group_max_rows: tier_options.row_group_max_rows.min(STAGING_ROW_GROUP_ROWS),
            ..tier_options.clone()
        };
        let outputs = tier::write_hour(
            &tmp,
            pages,
            self.options.format,
            &run_options,
            &self.options.parquet,
        )?;
        self.crash_point(21)?;

        // 2. Publish the run into the hour's staging directory.
        let target = staging_dir(hour);
        let target_dir = self.dir.join(&target);
        fs::create_dir_all(&target_dir)?;
        for output in &outputs {
            fs::rename(tmp.join(&output.name), target_dir.join(&output.name))?;
        }
        File::open(&target_dir)?.sync_all()?;
        fs::remove_dir(&tmp)?;
        self.crash_point(22)?;

        // 3. Commit.
        let mut next = self.manifest.clone();
        for entry in entries(&target, Area::Staging, outputs) {
            next = next.with_file(entry);
        }
        next.write(&self.dir)?;
        self.crash_point(23)?;

        // 4. Adopt; 5. release the WAL and the pages.
        self.adopt(next, true);
        self.wal.mark_flushed(lsn_hi)?;
        self.crash_point(25)?;
        Ok(())
    }

    /// Closes hour `hour`: its runs in `staging/` (if it spilled) and
    /// `pages`, merged into one file in `live/`, and commits it: ADR 0027's
    /// five steps, with a directory as the output. The commit replaces the
    /// runs, which are deleted after the grace period.
    fn flush_hour(
        &mut self,
        hour: i64,
        pages: &[Page],
        now: i64,
    ) -> Result<Vec<FileEntry>, IngestError> {
        let Layout::Tiered(tier_options) = &self.options.layout else {
            return Ok(Vec::new());
        };
        let staged = staging_range(&self.manifest);
        let runs: Vec<FileEntry> = self
            .manifest
            .files
            .get(staged.clone())
            .unwrap_or_default()
            .to_vec();
        let lsn_lo = runs
            .first()
            .map(|e| e.lsn_lo)
            .or(pages.first().map(|p| p.lsn_lo));
        let lsn_hi = pages
            .last()
            .map(|p| p.lsn_hi)
            .or(runs.last().map(|e| e.lsn_hi));
        let (Some(lsn_lo), Some(lsn_hi)) = (lsn_lo, lsn_hi) else {
            return Ok(Vec::new());
        };
        self.wal.sync()?;
        let id = format!("{lsn_lo:020}-{lsn_hi:020}");
        // Read a chunk at a time, not mapped: a mapping keeps every page the
        // merge touched resident until it ends (#854).
        let files = runs
            .iter()
            .map(|e| PosixVfs.open(&self.dir.join(&e.path)))
            .collect::<io::Result<Vec<_>>>()?;
        let run_files: Vec<(&FileEntry, &PosixFile)> = runs.iter().zip(files.iter()).collect();

        // 1. Write under tmp/.
        let tmp = self.dir.join(TMP_DIR).join(&id);
        fs::create_dir_all(&tmp)?;
        let outputs = tier::close_hour(
            &tmp,
            &run_files,
            pages,
            self.options.format,
            self.manifest.schema.as_deref(),
            tier_options,
            &self.options.parquet,
        )?;
        self.crash_point(1)?;

        // 2. Publish the hour's directory.
        let target = live_dir(hour);
        publish(&self.dir, &tmp, &target)?;
        self.crash_point(2)?;

        // 3. Commit: the runs out, the hour in.
        let entries = entries(&target, Area::Live, outputs);
        let mut next = self.manifest.replacing(staged, entries.clone());
        next.flushed_lsn = lsn_hi;
        next.write(&self.dir)?;
        self.crash_point(3)?;

        // 4. Adopt; 5. release the WAL and the pages.
        let grace = tier_options.delete_grace_ns;
        self.adopt(next, !pages.is_empty());
        if !pages.is_empty() {
            self.wal.mark_flushed(lsn_hi)?;
        }
        let at = now.saturating_add(grace);
        self.pending_deletes
            .extend(runs.iter().map(|e| (self.dir.join(&e.path), at)));
        self.crash_point(5)?;
        Ok(entries)
    }

    /// Merges every closed day's hours in `live/` into `archive/` (ADR
    /// 0028), one commit per day, and drops the days `retention_days` no
    /// longer keeps. Returns the files it added.
    fn compact(&mut self, now: i64) -> Result<Vec<FileEntry>, IngestError> {
        let Layout::Tiered(tier_options) = self.options.layout.clone() else {
            return Ok(Vec::new());
        };
        let open_day = day_of_hour(hour_of(now));
        let day_of = |e: &FileEntry| e.observed_ts.map(|(lo, _)| day_of_hour(hour_of(lo)));
        let mut added = Vec::new();
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
                .position(|e| e.area != Area::Live || day_of(e) != Some(day))
                .map_or(files.len(), |n| start.saturating_add(n));
            added.extend(self.merge_day(day, start..end, &tier_options, now)?);
        }
        self.retain(&tier_options, now)?;
        Ok(added)
    }

    /// Retention (ADR 0028): drops the leading `archive/` files of days
    /// before the last `retention_days` closed ones in one commit, which
    /// moves `dropped_lsn` to their end, and deletes them after the grace
    /// period. Their LSNs are at or below `flushed_lsn`, so the WAL holds
    /// none of them.
    fn retain(&mut self, tier_options: &TierOptions, now: i64) -> Result<(), IngestError> {
        let Some(days) = tier_options.retention_days else {
            return Ok(());
        };
        let open_day = day_of_hour(hour_of(now));
        let first_kept = open_day.saturating_sub(i64::from(days));
        let count = self
            .manifest
            .files
            .iter()
            .take_while(|e| {
                e.area == Area::Archive
                    && e.observed_ts
                        .is_some_and(|(_, hi)| day_of_hour(hour_of(hi)) < first_kept)
            })
            .count();
        if count == 0 {
            return Ok(());
        }
        let next = self.manifest.dropping(count);
        next.write(&self.dir)?;
        self.crash_point(14)?;
        let dropped = self
            .manifest
            .files
            .get(..count)
            .unwrap_or_default()
            .to_vec();
        self.adopt(next, false);
        let at = now.saturating_add(tier_options.delete_grace_ns);
        self.pending_deletes
            .extend(dropped.iter().map(|e| (self.dir.join(&e.path), at)));
        Ok(())
    }

    /// Merges `files[range]` (day `day`'s hours) into `archive/`.
    fn merge_day(
        &mut self,
        day: i64,
        range: std::ops::Range<usize>,
        tier_options: &TierOptions,
        now: i64,
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
        // Read a chunk at a time, not mapped: a mapping would keep the whole
        // day resident until the merge ends (#854).
        let files = inputs
            .iter()
            .map(|e| PosixVfs.open(&self.dir.join(&e.path)))
            .collect::<io::Result<Vec<_>>>()?;
        let input_files: Vec<(&FileEntry, &PosixFile)> = inputs.iter().zip(files.iter()).collect();

        // 1. Write under tmp/.
        let tmp = self.dir.join(TMP_DIR).join(&id);
        fs::create_dir_all(&tmp)?;
        let outputs = tier::merge_day(
            &tmp,
            &input_files,
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
        self.adopt(next, false);

        // 4. Delete the inputs after the grace period.
        let at = now.saturating_add(tier_options.delete_grace_ns);
        self.pending_deletes
            .extend(inputs.iter().map(|e| (self.dir.join(&e.path), at)));
        Ok(entries)
    }

    /// Deletes replaced files whose grace period is over, and their
    /// directories once empty.
    fn delete_expired(&mut self, now: i64) -> Result<(), IngestError> {
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
