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
    super::create_dir_all_synced(dir)?;
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

/// Where a read of the WAL stopped: the end of the last complete frame of
/// the segment starting at `lsn_lo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalPos {
    pub(crate) lsn_lo: u64,
    pub(crate) offset: u64,
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
    Ok(read_from(dir, flushed_lsn.saturating_add(1), None)?.map(|(records, _)| records))
}

/// The frames a read found and where it stopped.
pub(crate) type Tail = (Vec<super::wal::Record>, Option<WalPos>);

/// The frames from LSN `next` on, as [`read_tail`] reads them, and where
/// the read stopped. With `start`, the one a previous call returned, only
/// the bytes after it are read (#593): `next` is then the LSN after the
/// last one that call returned. `None` when `start` no longer holds: its
/// segment is gone or shorter than the position, or the frame there is not
/// LSN `next`, as after the writer truncated a torn tail and appended
/// again. The caller then reads everything again.
pub(crate) fn read_from(
    dir: &Path,
    next: u64,
    start: Option<WalPos>,
) -> Result<Option<Tail>, WalError> {
    let all = match list(dir) {
        Ok(all) => all,
        Err(WalError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(start.is_none().then(|| (Vec::new(), None)))
        }
        Err(e) => return Err(e),
    };
    if start.is_some_and(|p| !all.iter().any(|s| s.lsn_lo == p.lsn_lo)) {
        return Ok(None);
    }
    let mut records = Vec::new();
    let mut next = next;
    let mut pos = None;
    for (i, segment) in all.iter().enumerate() {
        let later = all.get(i.saturating_add(1));
        // Every frame of this file is at or below what we already have.
        if later.is_some_and(|n| n.lsn_lo <= next) {
            continue;
        }
        if segment.lsn_lo > next {
            return Ok(None);
        }
        let from = start
            .filter(|p| p.lsn_lo == segment.lsn_lo)
            .map_or(0, |p| p.offset);
        let buf = match read_file_from(&segment.path, from) {
            Ok(Some(buf)) => buf,
            Ok(None) => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let base = usize::try_from(from).unwrap_or(usize::MAX);
        let mut at = 0usize;
        while at < buf.len() {
            let Frame::Complete {
                lsn,
                payload_start,
                end,
            } = frame::decode(&buf, at)
            else {
                if later.is_some() {
                    if from > 0 {
                        return Ok(None);
                    }
                    let reason =
                        "bad frame in a segment that was complete before the next one started";
                    return Err(corrupt(&segment.path, at, reason.to_string()));
                }
                break;
            };
            if from > 0 && at == 0 && lsn != next {
                return Ok(None);
            }
            if lsn > next {
                return Err(corrupt(
                    &segment.path,
                    base.saturating_add(at),
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
        pos = Some(WalPos {
            lsn_lo: segment.lsn_lo,
            offset: u64::try_from(base.saturating_add(at)).unwrap_or(u64::MAX),
        });
    }
    Ok(Some((records, pos)))
}

/// `path`'s bytes from `from` on; `None` if it is shorter than `from`.
fn read_file_from(path: &Path, from: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() < from {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(Some(buf))
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

    /// A wal directory with one segment of frames 1..=4 (payload `p<lsn>`),
    /// and the byte offset after each.
    fn four_frames(name: &str) -> (PathBuf, [u64; 4]) {
        let dir =
            std::env::temp_dir().join(format!("db-core-read-from-{}-{name}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        let mut buf = Vec::new();
        let mut ends = [0u64; 4];
        for (i, end) in ends.iter_mut().enumerate() {
            frame::encode(&mut buf, u64::try_from(i + 1).unwrap(), b"payload").unwrap();
            *end = u64::try_from(buf.len()).unwrap();
        }
        fs::write(dir.join(segment_name(1)), buf).unwrap();
        (dir, ends)
    }

    fn lsns(read: Option<Tail>) -> Vec<u64> {
        read.map(|(records, _)| records.iter().map(|r| r.0).collect())
            .unwrap_or_default()
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_recover_read_from_7c6cac3d__v1_a_position_whose_frame_is_not_the_next_lsn_is_refused(
    ) {
        let (dir, ends) = four_frames("v1");
        // The reader believes it has LSNs 1-3, but the frame at its
        // position is LSN 3: the file was rewritten under it.
        let pos = WalPos {
            lsn_lo: 1,
            offset: ends[1],
        };
        assert_eq!(read_from(&dir, 4, Some(pos)).unwrap(), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_recover_read_from_7c6cac3d__v2_a_read_from_the_start_takes_every_frame()
    {
        let (dir, ends) = four_frames("v2");
        let (records, pos) = read_from(&dir, 1, None).unwrap().unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(
            pos,
            Some(WalPos {
                lsn_lo: 1,
                offset: ends[3]
            })
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_recover_read_from_7c6cac3d__v3_later_frames_of_a_resumed_read_are_not_checked_against_the_position(
    ) {
        let (dir, ends) = four_frames("v3");
        let pos = WalPos {
            lsn_lo: 1,
            offset: ends[0],
        };
        assert_eq!(lsns(read_from(&dir, 2, Some(pos)).unwrap()), [2, 3, 4]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_ingest_recover_read_from_7c6cac3d__v4_a_resumed_read_takes_the_frame_it_expects(
    ) {
        let (dir, ends) = four_frames("v4");
        let pos = WalPos {
            lsn_lo: 1,
            offset: ends[2],
        };
        let (records, new) = read_from(&dir, 4, Some(pos)).unwrap().unwrap();
        assert_eq!(records.iter().map(|r| r.0).collect::<Vec<_>>(), [4]);
        assert_eq!(
            new,
            Some(WalPos {
                lsn_lo: 1,
                offset: ends[3]
            })
        );
        // Nothing new: the position stays.
        assert_eq!(read_from(&dir, 5, new).unwrap().unwrap(), (Vec::new(), new));
        fs::remove_dir_all(&dir).ok();
    }
}
