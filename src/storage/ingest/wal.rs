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

use std::fmt;
use std::io;
use std::path::PathBuf;

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

#[cfg(feature = "ingest")]
mod writer;
#[cfg(feature = "ingest")]
pub use writer::Wal;
