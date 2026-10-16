//! WAL replay on open (ADR 0027 §Recovery, as amended for #571).
//!
//! Segment files are `wal/<lsn_lo:020>.wal`, each holding consecutive
//! frames from `lsn_lo` on. Replay:
//!
//! 1. deletes every file whose frames are all at or below `flushed_lsn`;
//! 2. reads the rest in order, requiring consecutive LSNs within and across
//!    files, and returns the frames above `flushed_lsn`;
//! 3. at a frame that is incomplete or fails its checksum: if it is in the
//!    last file and no valid frame follows it, it is the torn tail of a
//!    crash, and the file is truncated there; anything else is corruption
//!    of data that may have been acknowledged, and opening fails rather
//!    than silently dropping it.
//!
//! An empty last file (a crash between creating it and writing to it) is
//! removed. Replaying twice gives the same result.

use std::fs;
#[cfg(feature = "ingest")]
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

#[cfg(feature = "ingest")]
use super::frame::HEADER_LEN;
use super::frame::{self, Frame};
use super::wal::WalError;

/// One WAL segment file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) lsn_lo: u64,
    pub(crate) path: PathBuf,
}

/// What replay found.
#[derive(Debug)]
#[cfg(feature = "ingest")]
pub(crate) struct Replay {
    /// Frames above `flushed_lsn`, in LSN order.
    pub(crate) records: Vec<super::wal::Record>,
    /// The highest LSN on disk or flushed: the next append gets this + 1.
    pub(crate) last_lsn: u64,
    /// The segment files left, oldest first.
    pub(crate) segments: Vec<Segment>,
}

/// `<lsn_lo:020>.wal`.
#[cfg(feature = "ingest")]
pub(crate) fn segment_name(lsn_lo: u64) -> String {
    format!("{lsn_lo:020}.wal")
}

fn parse_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(".wal")?;
    if digits.len() != 20 {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn corrupt(path: &Path, offset: usize, reason: String) -> WalError {
    WalError::Corrupt {
        file: path.to_path_buf(),
        offset: u64::try_from(offset).unwrap_or(u64::MAX),
        reason,
    }
}

fn list(dir: &Path) -> Result<Vec<Segment>, WalError> {
    let mut segments = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(lsn_lo) = entry.file_name().to_str().and_then(parse_name) {
            segments.push(Segment {
                lsn_lo,
                path: entry.path(),
            });
        }
    }
    segments.sort_by_key(|s| s.lsn_lo);
    Ok(segments)
}

/// Whether a valid frame with an LSN above `prev` starts anywhere after
/// `from`. Candidate offsets are filtered by a plausible LSN before the
/// checksum is computed, so the scan is cheap.
#[cfg(feature = "ingest")]
fn valid_frame_after(buf: &[u8], from: usize, prev: u64) -> bool {
    let room = u64::try_from(buf.len().saturating_sub(from) / HEADER_LEN).unwrap_or(u64::MAX);
    let highest = prev.saturating_add(room).saturating_add(1);
    let start = from.saturating_add(1);
    (start..buf.len()).any(|at| {
        let lsn = at
            .checked_add(8)
            .and_then(|o| buf.get(o..o.checked_add(8)?))
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_le_bytes);
        match lsn {
            Some(lsn) if lsn > prev && lsn <= highest => {
                matches!(frame::decode(buf, at), Frame::Complete { .. })
            }
            _ => false,
        }
    })
}

/// Replays `dir` (created if missing) from `flushed_lsn`.
#[cfg(feature = "ingest")]
pub(crate) fn replay(dir: &Path, flushed_lsn: u64) -> Result<Replay, WalError> {
    fs::create_dir_all(dir)?;
    let all = list(dir)?;
    let mut kept = Vec::new();
    for (i, segment) in all.iter().enumerate() {
        let next_lo = all.get(i.saturating_add(1)).map(|n| n.lsn_lo);
        match next_lo {
            Some(next) if next.saturating_sub(1) <= flushed_lsn => fs::remove_file(&segment.path)?,
            _ => kept.push(segment.clone()),
        }
    }

    let mut records = Vec::new();
    let mut prev: Option<u64> = None;
    let mut segments = Vec::new();
    let count = kept.len();
    for (i, segment) in kept.into_iter().enumerate() {
        let is_last = i.saturating_add(1) == count;
        // The first file may start at or below flushed_lsn + 1 (partly
        // flushed); every later one exactly where the previous ended.
        let expected_lo = match prev {
            Some(p) => p.saturating_add(1),
            None => segment.lsn_lo.min(flushed_lsn.saturating_add(1)),
        };
        if segment.lsn_lo != expected_lo {
            let reason = format!(
                "starts at LSN {}, expected {expected_lo} (flushed_lsn {flushed_lsn}): frames are missing",
                segment.lsn_lo
            );
            return Err(corrupt(&segment.path, 0, reason));
        }
        let buf = fs::read(&segment.path)?;
        let mut last = segment.lsn_lo.saturating_sub(1);
        let mut at = 0usize;
        while at < buf.len() {
            match frame::decode(&buf, at) {
                Frame::Complete {
                    lsn,
                    payload_start,
                    end,
                } => {
                    if lsn != last.saturating_add(1) {
                        return Err(corrupt(
                            &segment.path,
                            at,
                            format!("LSN {lsn} follows {last}"),
                        ));
                    }
                    if lsn > flushed_lsn {
                        let payload = buf.get(payload_start..end).unwrap_or_default();
                        records.push((lsn, payload.to_vec()));
                    }
                    last = lsn;
                    at = end;
                }
                Frame::Incomplete | Frame::BadChecksum => {
                    if !is_last {
                        let reason =
                            "bad frame in a segment that was complete before the next one started";
                        return Err(corrupt(&segment.path, at, reason.to_string()));
                    }
                    if valid_frame_after(&buf, at, last) {
                        let reason = "bad frame followed by valid frames";
                        return Err(corrupt(&segment.path, at, reason.to_string()));
                    }
                    truncate(&segment.path, at)?;
                    break;
                }
            }
        }
        if at == 0 {
            // Nothing valid in it: necessarily the last file (a non-last
            // one fails above), created by a crash before its first write.
            fs::remove_file(&segment.path)?;
        } else {
            segments.push(segment);
        }
        prev = Some(last);
    }
    Ok(Replay {
        records,
        last_lsn: prev.unwrap_or(0).max(flushed_lsn),
        segments,
    })
}

/// Reads the frames above `flushed_lsn` without changing anything: a
/// reader in another process, while a writer may be appending (#572).
/// A bad or partial frame in the last file ends the read, since the writer
/// may be in the middle of writing it; in an earlier file it is
/// corruption, as in [`replay`]. `None` when the files no longer reach
/// down to `flushed_lsn + 1` or one vanished during the read: a flush
/// moved on, and the caller re-reads the manifest.
pub(crate) fn read_tail(
    dir: &Path,
    flushed_lsn: u64,
) -> Result<Option<Vec<super::wal::Record>>, WalError> {
    let all = match list(dir) {
        Ok(all) => all,
        Err(WalError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(Vec::new()))
        }
        Err(e) => return Err(e),
    };
    let mut records = Vec::new();
    let mut next = flushed_lsn.saturating_add(1);
    for (i, segment) in all.iter().enumerate() {
        let later = all.get(i.saturating_add(1));
        // Every frame of this file is at or below what we already have.
        if later.is_some_and(|n| n.lsn_lo <= next) {
            continue;
        }
        if segment.lsn_lo > next {
            return Ok(None);
        }
        let buf = match fs::read(&segment.path) {
            Ok(buf) => buf,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut at = 0usize;
        while at < buf.len() {
            let Frame::Complete {
                lsn,
                payload_start,
                end,
            } = frame::decode(&buf, at)
            else {
                if later.is_some() {
                    let reason =
                        "bad frame in a segment that was complete before the next one started";
                    return Err(corrupt(&segment.path, at, reason.to_string()));
                }
                break;
            };
            if lsn > next {
                return Err(corrupt(
                    &segment.path,
                    at,
                    format!("LSN {lsn} where {next} was expected"),
                ));
            }
            if lsn == next {
                let payload = buf.get(payload_start..end).unwrap_or_default();
                records.push((lsn, payload.to_vec()));
                next = next.saturating_add(1);
            }
            at = end;
        }
    }
    Ok(Some(records))
}

#[cfg(feature = "ingest")]
fn truncate(path: &Path, len: usize) -> Result<(), WalError> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(u64::try_from(len).unwrap_or(u64::MAX))?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One junk byte, then a valid frame with LSN `lsn`.
    fn junk_then_frame(lsn: u64) -> Vec<u8> {
        let mut buf = vec![0xee];
        frame::encode(&mut buf, lsn, b"payload").unwrap();
        buf
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_recover_valid_frame_after_604f68e4__v1_lsn_in_the_plausible_range_is_checked(
    ) {
        assert!(valid_frame_after(&junk_then_frame(8), 0, 7));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_recover_valid_frame_after_604f68e4__v2_lsn_not_above_prev_is_skipped() {
        assert!(!valid_frame_after(&junk_then_frame(8), 0, 8));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_recover_valid_frame_after_604f68e4__v3_lsn_beyond_what_could_fit_is_skipped(
    ) {
        // Room for one frame after `prev` = 1: LSN 1000 cannot follow.
        assert!(!valid_frame_after(&junk_then_frame(1000), 0, 1));
    }
}
