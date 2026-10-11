//! The record WAL (ADR 0027, #571): LSNs, group commit, three watermarks.
//!
//! [`Wal::append`] assigns the next LSN, frames the record into a shared
//! buffer and advances `visible`. A dedicated WAL thread drains the buffer
//! in rounds: one `write`, one `fsync`, then `durable` moves to the last
//! LSN written and every waiter wakes. One fsync covers everything that
//! arrived during the previous one, so `Ack::Durable` costs latency, not
//! throughput. `flushed` moves when the caller has committed records
//! elsewhere ([`Wal::mark_flushed`], the manifest commit of #572), which
//! also unlinks segment files that hold nothing above it.
//!
//! `flushed <= durable <= visible` always holds.
//!
//! Segment files (`wal/<lsn_lo:020>.wal`) are created on their first
//! write, the directory fsynced, and closed after the round that passes
//! [`WalOptions::segment_bytes`] or after [`Wal::roll`]. A closed file is
//! complete and fsynced before the next exists, so only the last file can
//! end in a torn frame; see [`super::recover`].

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::{fmt, fs};

use super::frame;
use super::lock::{LockError, TableLock};
use super::recover::{self, Segment};

/// The WAL directory inside the table directory.
pub const WAL_DIR: &str = "wal";

/// A replayed record: its LSN and payload.
pub type Record = (u64, Vec<u8>);

/// What an append waits for before returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// Return once the record has an LSN and is visible; a crash can lose
    /// it until the next group commit.
    Memory,
    /// Return once the record is fsynced (`durable >= lsn`).
    Durable,
    /// Return once the record is committed elsewhere (`flushed >= lsn`).
    Flushed,
}

/// The three watermarks; `flushed <= durable <= visible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Watermarks {
    /// Highest LSN appended.
    pub visible: u64,
    /// Highest LSN fsynced.
    pub durable: u64,
    /// Highest LSN the caller has committed elsewhere.
    pub flushed: u64,
}

/// WAL settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalOptions {
    /// A segment file is closed after the group commit that takes it to
    /// at least this many bytes.
    pub segment_bytes: u64,
}

impl Default for WalOptions {
    /// 64 MiB segment files.
    fn default() -> Self {
        WalOptions {
            segment_bytes: 64 << 20,
        }
    }
}

/// Why a WAL operation failed.
#[derive(Debug)]
pub enum WalError {
    /// File system error.
    Io(io::Error),
    /// Another writer holds the table's `LOCK`.
    Locked,
    /// Replay found damage that is not a torn tail; nothing is dropped.
    Corrupt {
        /// The segment file.
        file: PathBuf,
        /// Byte offset of the bad frame.
        offset: u64,
        /// What was wrong.
        reason: String,
    },
    /// A record longer than a frame's `u32` length can describe.
    TooLarge,
    /// The WAL was closed while the call waited, or before it.
    Closed,
    /// The WAL thread stopped on an I/O error; no further writes succeed.
    Failed(String),
    /// [`Wal::mark_flushed`] was given an LSN above `durable`.
    FlushBeyondDurable {
        /// The LSN given.
        flushed: u64,
        /// `durable` at the time.
        durable: u64,
    },
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalError::Io(e) => write!(f, "wal: {e}"),
            WalError::Locked => write!(f, "wal: the table is locked by another writer"),
            WalError::Corrupt {
                file,
                offset,
                reason,
            } => write!(f, "wal: {} at byte {offset}: {reason}", file.display()),
            WalError::TooLarge => write!(f, "wal: record larger than 4 GiB"),
            WalError::Closed => write!(f, "wal: closed"),
            WalError::Failed(why) => write!(f, "wal: writer stopped: {why}"),
            WalError::FlushBeyondDurable { flushed, durable } => {
                write!(
                    f,
                    "wal: flushed LSN {flushed} is above durable LSN {durable}"
                )
            }
        }
    }
}

impl std::error::Error for WalError {}

impl From<io::Error> for WalError {
    fn from(e: io::Error) -> Self {
        WalError::Io(e)
    }
}

/// State shared by appenders and the WAL thread.
#[derive(Debug, Default)]
struct State {
    next_lsn: u64,
    /// Frames for LSNs `durable + 1 .. next_lsn`, not yet written.
    buffer: Vec<u8>,
    marks: Watermarks,
    /// Pending file boundaries: `(offset in buffer, first LSN after it)`,
    /// one per [`Wal::roll`] since the last round.
    rolls: Vec<(usize, u64)>,
    closing: bool,
    failed: Option<String>,
    segments: Vec<Segment>,
    syncs: u64,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    /// Wakes the WAL thread: new frames, a roll, or close.
    work: Condvar,
    /// Wakes waiters: a watermark moved, or the WAL failed or closed.
    progress: Condvar,
}

fn poisoned() -> WalError {
    WalError::Failed("state lock poisoned".to_string())
}

impl Shared {
    fn lock(&self) -> Result<MutexGuard<'_, State>, WalError> {
        self.state.lock().map_err(|_| poisoned())
    }
}

/// An open WAL: holds the table lock and runs the WAL thread until
/// [`Wal::close`] or drop.
#[derive(Debug)]
pub struct Wal {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    dir: PathBuf,
    _lock: TableLock,
}

impl Wal {
    /// Locks `table`, replays `table/wal/` from `flushed_lsn` and starts
    /// the WAL thread. Returns the frames above `flushed_lsn` (to rebuild
    /// in-memory state from) in LSN order; the next append continues after
    /// the highest LSN on disk.
    pub fn open(
        table: &Path,
        flushed_lsn: u64,
        options: WalOptions,
    ) -> Result<(Wal, Vec<Record>), WalError> {
        let lock = TableLock::acquire(table).map_err(|e| match e {
            LockError::Held => WalError::Locked,
            LockError::Io(e) => WalError::Io(e),
        })?;
        Wal::open_locked(lock, table, flushed_lsn, options)
    }

    /// [`Wal::open`] for a caller that already holds the table lock (and
    /// read the manifest under it); the WAL keeps the lock until closed.
    pub fn open_locked(
        lock: TableLock,
        table: &Path,
        flushed_lsn: u64,
        options: WalOptions,
    ) -> Result<(Wal, Vec<Record>), WalError> {
        let dir = table.join(WAL_DIR);
        let replay = recover::replay(&dir, flushed_lsn)?;
        let last = replay.last_lsn;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                next_lsn: last.saturating_add(1),
                marks: Watermarks {
                    visible: last,
                    durable: last,
                    flushed: flushed_lsn,
                },
                segments: replay.segments,
                ..State::default()
            }),
            ..Shared::default()
        });
        let thread = {
            let shared = Arc::clone(&shared);
            let dir = dir.clone();
            std::thread::Builder::new()
                .name("db-core-wal".to_string())
                .spawn(move || run(&shared, &dir, &options))?
        };
        let wal = Wal {
            shared,
            thread: Some(thread),
            dir,
            _lock: lock,
        };
        Ok((wal, replay.records))
    }

    /// Appends one record and waits for `ack`; returns its LSN.
    pub fn append(&self, record: &[u8], ack: Ack) -> Result<u64, WalError> {
        let mut state = self.shared.lock()?;
        if let Some(why) = &state.failed {
            return Err(WalError::Failed(why.clone()));
        }
        if state.closing {
            return Err(WalError::Closed);
        }
        let lsn = state.next_lsn;
        let next = lsn.checked_add(1).ok_or(WalError::TooLarge)?;
        frame::encode(&mut state.buffer, lsn, record).ok_or(WalError::TooLarge)?;
        state.next_lsn = next;
        state.marks.visible = lsn;
        self.shared.work.notify_one();
        match ack {
            Ack::Memory => Ok(lsn),
            Ack::Durable => self.wait(state, lsn, |m| m.durable),
            Ack::Flushed => self.wait(state, lsn, |m| m.flushed),
        }
    }

    /// Blocks until `mark(watermarks) >= lsn`, the WAL fails, or it closes.
    fn wait(
        &self,
        mut state: MutexGuard<'_, State>,
        lsn: u64,
        mark: impl Fn(&Watermarks) -> u64,
    ) -> Result<u64, WalError> {
        loop {
            if mark(&state.marks) >= lsn {
                return Ok(lsn);
            }
            if let Some(why) = &state.failed {
                return Err(WalError::Failed(why.clone()));
            }
            if state.closing {
                return Err(WalError::Closed);
            }
            state = self.shared.progress.wait(state).map_err(|_| poisoned())?;
        }
    }

    /// The current watermarks.
    pub fn watermarks(&self) -> Result<Watermarks, WalError> {
        Ok(self.shared.lock()?.marks)
    }

    /// Waits until everything appended so far is durable.
    pub fn sync(&self) -> Result<u64, WalError> {
        let state = self.shared.lock()?;
        let visible = state.marks.visible;
        self.wait(state, visible, |m| m.durable)
    }

    /// Starts a new segment file with the next frame written (to align
    /// files with in-memory pages, #572).
    pub fn roll(&self) -> Result<(), WalError> {
        let mut state = self.shared.lock()?;
        let boundary = (state.buffer.len(), state.next_lsn);
        state.rolls.push(boundary);
        drop(state);
        self.shared.work.notify_one();
        Ok(())
    }

    /// Records that every LSN up to `lsn` is committed elsewhere: advances
    /// `flushed`, wakes `Ack::Flushed` waiters and unlinks segment files
    /// that hold nothing above it (never the newest file).
    pub fn mark_flushed(&self, lsn: u64) -> Result<(), WalError> {
        let removable = {
            let mut state = self.shared.lock()?;
            if lsn > state.marks.durable {
                return Err(WalError::FlushBeyondDurable {
                    flushed: lsn,
                    durable: state.marks.durable,
                });
            }
            state.marks.flushed = state.marks.flushed.max(lsn);
            self.shared.progress.notify_all();
            let keep_from = state
                .segments
                .windows(2)
                .take_while(|pair| {
                    pair.get(1)
                        .is_some_and(|next| next.lsn_lo.saturating_sub(1) <= lsn)
                })
                .count();
            state.segments.drain(..keep_from).collect::<Vec<_>>()
        };
        for segment in removable {
            fs::remove_file(&segment.path)?;
        }
        Ok(())
    }

    /// The WAL directory (`<table>/wal`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stops accepting appends, writes and fsyncs what is buffered, stops
    /// the WAL thread and releases the table lock.
    pub fn close(mut self) -> Result<(), WalError> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), WalError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        {
            let mut state = self.shared.lock()?;
            state.closing = true;
        }
        self.shared.work.notify_all();
        self.shared.progress.notify_all();
        thread
            .join()
            .map_err(|_| WalError::Failed("WAL thread panicked".to_string()))?;
        match self.shared.lock()?.failed.take() {
            Some(why) => Err(WalError::Failed(why)),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    fn syncs(&self) -> u64 {
        self.shared.lock().map_or(0, |s| s.syncs)
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        // Best effort: buffered frames still reach disk; errors were
        // either already reported to appenders or are unreportable here.
        self.shutdown().ok();
    }
}

/// The open segment file and its size.
struct Current {
    file: File,
    bytes: u64,
}

/// The WAL thread: group-commit rounds until close, then a final round
/// for whatever is buffered.
fn run(shared: &Shared, dir: &Path, options: &WalOptions) {
    let mut current: Option<Current> = None;
    loop {
        let round = match next_round(shared) {
            Ok(Some(round)) => round,
            Ok(None) | Err(_) => return,
        };
        if let Err(e) = commit(shared, dir, options, &mut current, round) {
            if let Ok(mut state) = shared.lock() {
                state.failed = Some(e.to_string());
            }
            shared.progress.notify_all();
            return;
        }
    }
}

/// One group commit's input.
struct Round {
    bytes: Vec<u8>,
    first: u64,
    last: u64,
    /// File boundaries inside `bytes`, from [`State::rolls`].
    rolls: Vec<(usize, u64)>,
}

/// Waits for frames, a roll or close; `None` once closing with nothing
/// left to write.
fn next_round(shared: &Shared) -> Result<Option<Round>, WalError> {
    let mut state = shared.lock()?;
    loop {
        if !state.buffer.is_empty() {
            break;
        }
        if !state.rolls.is_empty() {
            break;
        }
        if state.closing {
            return Ok(None);
        }
        state = shared.work.wait(state).map_err(|_| poisoned())?;
    }
    Ok(Some(Round {
        bytes: std::mem::take(&mut state.buffer),
        first: state.marks.durable.saturating_add(1),
        last: state.next_lsn.saturating_sub(1),
        rolls: std::mem::take(&mut state.rolls),
    }))
}

/// Writes a round: the buffer split at its roll boundaries, each piece
/// written and fsynced into the open segment (or a new one named by the
/// piece's first LSN), the segment closed at each boundary and once it
/// reaches the size limit. Then `durable` moves to the round's last LSN.
fn commit(
    shared: &Shared,
    dir: &Path,
    options: &WalOptions,
    current: &mut Option<Current>,
    round: Round,
) -> Result<(), WalError> {
    let mut start = 0usize;
    let mut first = round.first;
    let mut syncs = 0u64;
    let boundaries = round.rolls.iter().map(|&(offset, lsn)| (offset, Some(lsn)));
    for (end, next_first) in boundaries.chain(std::iter::once((round.bytes.len(), None))) {
        let piece = round.bytes.get(start..end).unwrap_or_default();
        if !piece.is_empty() {
            write_piece(shared, dir, options, current, first, piece)?;
            syncs = syncs.saturating_add(1);
        }
        if let Some(next_first) = next_first {
            *current = None;
            first = next_first;
        }
        start = end;
    }
    if syncs > 0 {
        let mut state = shared.lock()?;
        state.marks.durable = round.last;
        state.syncs = state.syncs.saturating_add(syncs);
    }
    shared.progress.notify_all();
    Ok(())
}

fn write_piece(
    shared: &Shared,
    dir: &Path,
    options: &WalOptions,
    current: &mut Option<Current>,
    first: u64,
    piece: &[u8],
) -> Result<(), WalError> {
    let mut open = match current.take() {
        Some(open) => open,
        None => create_segment(shared, dir, first)?,
    };
    open.file.write_all(piece)?;
    open.file.sync_data()?;
    let written = u64::try_from(piece.len()).unwrap_or(u64::MAX);
    open.bytes = open.bytes.saturating_add(written);
    if open.bytes < options.segment_bytes {
        *current = Some(open);
    }
    Ok(())
}

/// Creates `<lsn_lo:020>.wal`, fsyncs the directory so the file survives a
/// crash, and records it as the newest segment.
fn create_segment(shared: &Shared, dir: &Path, lsn_lo: u64) -> Result<Current, WalError> {
    let path = dir.join(recover::segment_name(lsn_lo));
    let file = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)?;
    File::open(dir)?.sync_all()?;
    shared.lock()?.segments.push(Segment { lsn_lo, path });
    Ok(Current { file, bytes: 0 })
}

#[cfg(test)]
mod tests;
