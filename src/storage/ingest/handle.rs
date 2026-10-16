//! The public table handle (ADR 0029, #574): [`Table`], a cloneable handle
//! over one [`TableWriter`], and its line sources.
//!
//! A `Table` has no threads of its own: every call takes the writer's lock
//! for its duration, so clones on other threads interleave call by call.
//! That is how a long-running source and the maintenance it needs share
//! one table: one thread runs [`Table::follow`] while another calls
//! [`Table::flush`] or [`Table::compact`] on a clone now and then, and
//! [`Table::close`] from any clone stops the follow.
//!
//! The sources append through the WAL like any other record:
//!
//! - [`Table::ingest_lines`] reads lines until end of input (stdin, a
//!   pipe), then waits until they are durable.
//! - [`Table::follow`] reads a log file from where it last stopped, then
//!   follows it: new lines, a rename-and-recreate rotation (read from the
//!   start of the new file) and truncation (read from the start again).
//!   After each batch of lines is durable, the end offset and the file's
//!   identity are written to `follow/<name>.offset` in the table
//!   directory, so a restart resumes there. A crash between the two can
//!   append that batch again on restart: delivery is at least once.
//!
//! Lines are split on `\n`, a trailing `\r` is dropped, and empty lines are
//! skipped.

use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::error::IngestError;
use super::manifest::FileEntry;
use super::record::RecordValue;
use super::snapshot::Snapshot;
use super::table::{TableOptions, TableWriter};
use super::wal::{Ack, WalError};
use crate::storage::stream::file::{Block, FileIdentity, LogFile, Refresh};

/// Where [`Table::follow`] keeps its resume points, in the table directory.
pub const FOLLOW_DIR: &str = "follow";

/// How long [`Table::follow`] waits before looking at the file again when
/// nothing new arrived.
const FOLLOW_POLL: Duration = Duration::from_millis(50);

/// A table, open for appending and reading; see the [module docs](self).
#[derive(Debug, Clone)]
pub struct Table {
    dir: PathBuf,
    /// `None` once closed.
    writer: Arc<Mutex<Option<TableWriter>>>,
}

impl Table {
    /// Opens (or creates) the table directory `dir`: takes its lock and
    /// recovers, as [`TableWriter::open`] does.
    pub fn open(dir: impl AsRef<Path>, options: TableOptions) -> Result<Table, IngestError> {
        let writer = TableWriter::open(dir.as_ref(), options)?;
        Ok(Table {
            dir: dir.as_ref().to_path_buf(),
            writer: Arc::new(Mutex::new(Some(writer))),
        })
    }

    /// The table directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> MutexGuard<'_, Option<TableWriter>> {
        // A call that panicked left the writer as its last commit left it:
        // every step of a flush or compaction is recoverable on its own.
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the writer, or fails with `WalError::Closed`.
    fn with<T>(
        &self,
        f: impl FnOnce(&mut TableWriter) -> Result<T, IngestError>,
    ) -> Result<T, IngestError> {
        match self.lock().as_mut() {
            Some(writer) => f(writer),
            None => Err(IngestError::Wal(WalError::Closed)),
        }
    }

    /// Whether [`close`](Self::close) was called on any clone.
    pub fn is_closed(&self) -> bool {
        self.lock().is_none()
    }

    /// Appends one record of a lines table; see [`TableWriter::append`].
    pub fn append(&self, line: &[u8], ack: Ack) -> Result<u64, IngestError> {
        self.with(|w| w.append(line, ack))
    }

    /// Appends one row of a record table; see [`TableWriter::append_row`].
    pub fn append_row(&self, row: Vec<RecordValue>, ack: Ack) -> Result<u64, IngestError> {
        self.with(|w| w.append_row(row, ack))
    }

    /// Appends `lines` to a lines table in order and waits for `ack` once,
    /// for the last; returns its LSN (`None` for no lines). Every line is
    /// checked first, so an invalid one appends none.
    pub fn append_batch<L: AsRef<[u8]>>(
        &self,
        lines: &[L],
        ack: Ack,
    ) -> Result<Option<u64>, IngestError> {
        if let Some(why) = lines
            .iter()
            .find_map(|l| super::page::invalid_line(l.as_ref()))
        {
            return Err(IngestError::InvalidRecord(why));
        }
        self.with(|w| {
            let mut last = None;
            let count = lines.len();
            for (i, line) in lines.iter().enumerate() {
                let this = if i.saturating_add(1) == count {
                    ack
                } else {
                    Ack::Memory
                };
                last = Some(w.append(line.as_ref(), this)?);
            }
            Ok(last)
        })
    }

    /// Waits until everything appended is durable; returns the durable
    /// LSN.
    pub fn sync(&self) -> Result<u64, IngestError> {
        self.with(|w| w.sync())
    }

    /// Seals the head and flushes everything that can be; see
    /// [`TableWriter::flush_all`].
    pub fn flush(&self) -> Result<Vec<FileEntry>, IngestError> {
        self.with(TableWriter::flush_all)
    }

    /// Closes hours and merges days; see [`TableWriter::compact`].
    pub fn compact(&self) -> Result<Vec<FileEntry>, IngestError> {
        self.with(TableWriter::compact)
    }

    /// A read view of everything appended so far.
    pub fn snapshot(&self) -> Result<Snapshot, IngestError> {
        self.with(|w| w.snapshot())
    }

    /// Makes everything appended durable and closes the table for every
    /// clone: later calls fail with `WalError::Closed`, and a running
    /// [`follow`](Self::follow) returns. Closing again does nothing.
    pub fn close(&self) -> Result<(), IngestError> {
        match self.lock().take() {
            Some(writer) => writer.close(),
            None => Ok(()),
        }
    }

    /// Appends every line of `input` until its end, then waits until they
    /// are durable; returns how many were appended.
    pub fn ingest_lines(&self, mut input: impl BufRead) -> Result<u64, IngestError> {
        let mut count = 0u64;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if input.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            if let Some(line) = trimmed(&buf) {
                self.append(line, Ack::Memory)?;
                count = count.saturating_add(1);
            }
        }
        if count > 0 {
            self.sync()?;
        }
        Ok(count)
    }

    /// Follows the log file at `path` until the table is closed; returns
    /// how many lines were appended. Starts where the last follow of this
    /// path stopped, or at the start of the file if it is new, rotated or
    /// truncated since.
    pub fn follow(&self, path: impl AsRef<Path>) -> Result<u64, IngestError> {
        let path = path.as_ref();
        let state = self.dir.join(FOLLOW_DIR).join(state_name(path));
        let mut file = resume(path, &state)?;
        let mut count = 0u64;
        while !self.is_closed() {
            match file.refresh()? {
                Refresh::New(blocks) => match self.append_blocks(&file, &blocks, &state)? {
                    Some(n) => count = count.saturating_add(n),
                    None => break,
                },
                Refresh::Truncated => {
                    file = LogFile::open_at(path, 0)?;
                    save(&state, file.identity(), 0)?;
                }
                Refresh::NoNew => {
                    // A rotation: once the new file exists, read what the
                    // old one got since the last look, then switch.
                    if file.rotated()? {
                        if let Ok(new) = LogFile::open_at(path, 0) {
                            if let Refresh::New(blocks) = file.refresh()? {
                                match self.append_blocks(&file, &blocks, &state)? {
                                    Some(n) => count = count.saturating_add(n),
                                    None => break,
                                }
                            }
                            file = new;
                            save(&state, file.identity(), 0)?;
                            continue;
                        }
                    }
                    std::thread::sleep(FOLLOW_POLL);
                }
            }
        }
        Ok(count)
    }

    /// Appends the lines of `blocks` (read from `file`), waits until they
    /// are durable and records the end of the last block in `state`;
    /// returns how many were appended, `None` if the table was closed.
    fn append_blocks(
        &self,
        file: &LogFile,
        blocks: &[Block],
        state: &Path,
    ) -> Result<Option<u64>, IngestError> {
        let appended = self.with(|w| {
            let mut n = 0u64;
            let lines = blocks.iter().flat_map(|b| b.bytes.split(|&c| c == b'\n'));
            for line in lines.filter_map(trimmed) {
                w.append(line, Ack::Memory)?;
                n = n.saturating_add(1);
            }
            w.sync()?;
            Ok(n)
        });
        let n = match appended {
            Ok(n) => n,
            // Closed while reading: what was appended is in the WAL, and
            // the offset stays at the last batch.
            Err(IngestError::Wal(WalError::Closed)) => return Ok(None),
            Err(e) => return Err(e),
        };
        if let Some(end) = blocks.last().map(Block::end_off) {
            save(state, file.identity(), end)?;
        }
        Ok(Some(n))
    }
}

/// `line` without its `\n` and one `\r` before it; `None` if that leaves
/// nothing.
fn trimmed(line: &[u8]) -> Option<&[u8]> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    (!line.is_empty()).then_some(line)
}

/// The resume file of a followed `path`: its file name and a hash of the
/// whole path, so two `app.log`s in different directories differ.
fn state_name(path: &Path) -> String {
    // FNV-1a: stable across runs and Rust versions, unlike `DefaultHasher`.
    let hash = path
        .to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    let name: String = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}-{hash:016x}.offset")
}

/// Opens `path` where `state` says the last follow stopped: the same file
/// (device and inode) and at least that long. Otherwise from its start.
fn resume(path: &Path, state: &Path) -> Result<LogFile, IngestError> {
    let saved = fs::read_to_string(state).ok().and_then(|text| {
        let mut parts = text.split_whitespace().map(str::parse::<u64>);
        match (parts.next(), parts.next(), parts.next()) {
            (Some(Ok(dev)), Some(Ok(ino)), Some(Ok(offset))) => {
                Some((FileIdentity { dev, ino }, offset))
            }
            _ => None,
        }
    });
    if let Some((identity, offset)) = saved {
        if FileIdentity::of(path).ok() == Some(identity) {
            if let Ok(file) = LogFile::open_at(path, offset) {
                return Ok(file);
            }
        }
    }
    Ok(LogFile::open_at(path, 0)?)
}

/// Records that everything before `offset` of the file `identity` is
/// durable in the table: written aside, fsynced, renamed over `state`.
fn save(state: &Path, identity: FileIdentity, offset: u64) -> io::Result<()> {
    let dir = state
        .parent()
        .ok_or_else(|| io::Error::other("the resume file has no directory"))?;
    fs::create_dir_all(dir)?;
    let tmp = state.with_extension("offset.tmp");
    {
        let mut file = File::create(&tmp)?;
        writeln!(file, "{} {} {offset}", identity.dev, identity.ino)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, state)?;
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests;
