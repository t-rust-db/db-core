//! PASSIVE WAL checkpoint (#386): copies committed WAL frames into the
//! main database file up to the oldest active reader's mark, then
//! publishes the new backfill boundary (`nBackfill`). Never waits for a
//! reader to finish — a reader still pinned to an older frame simply
//! bounds how far this pass can go, rather than blocking the checkpoint.
//! FULL/RESTART/TRUNCATE (#896) are this pass plus the write lock and a
//! busy report, built in `Pager::checkpoint_wal`; none of them waits.
//!
//! Frame `1..=safe_frame` is resolved via [`wal::committed_pages`] against
//! a byte slice truncated to exactly that many frames — `safe_frame`
//! always lands on a commit boundary (every reader's published mark is a
//! `mxFrame` value taken at a moment its writer had just committed), so
//! the truncated slice's last frame is itself a valid commit and
//! `committed_pages` resolves it the same way it resolves a full WAL.
//!
//! See `.openspec/adr/0025-passive-only-checkpoint-linear-frame-scan.md`
//! for why this is PASSIVE-only with a linear scan rather than FULL/RESTART
//! plus a page→frame hash table.

use std::path::Path;

use super::wal;
use super::PagerError;
use crate::storage::row::vfs::{companion_path, AnyVfs};

/// `PRAGMA wal_checkpoint`'s mode argument (#896). Only [`Self::Passive`]
/// never reports busy; the others report `busy` instead of waiting when a
/// reader keeps them from finishing, as SQLite does without a busy handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointMode {
    /// Backfill what no reader needs, never wait.
    Passive,
    /// Backfill every frame, blocking writers while doing so.
    Full,
    /// [`Self::Full`], then make the next writer restart the WAL.
    Restart,
    /// [`Self::Restart`], then truncate the `-wal` file to zero bytes.
    Truncate,
}

/// The `(busy, log, checkpointed)` row `PRAGMA wal_checkpoint` returns
/// (#896). `log`/`checkpointed` are `-1` when the database is not in WAL
/// mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalCheckpoint {
    /// A FULL/RESTART/TRUNCATE checkpoint could not finish.
    pub busy: bool,
    /// Frames in the WAL.
    pub log: i64,
    /// Frames backfilled into the main file.
    pub checkpointed: i64,
}

/// What a [`checkpoint_passive`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointResult {
    /// How many leading frames are now backfilled into the main file
    /// (`nBackfill`'s new value) — includes frames backfilled by any prior
    /// checkpoint.
    pub backfilled_frames: u32,
    /// Total frames currently in the WAL.
    pub total_frames: u32,
    /// Whether every frame in the WAL is now backfilled — `false` means at
    /// least one active reader still bounds further progress.
    pub checkpoint_complete: bool,
}

/// The main file's current page count, from its byte size — used to
/// bound a checkpoint's write offsets (see [`checkpoint_passive`]'s call
/// site). Fails closed: if `size` (in pages) doesn't fit in a `u32`,
/// returns `0` rather than `u32::MAX` — the caller takes `max(db_size,
/// this)`, so `0` just falls back to the WAL's own already-validated
/// `db_size` bound instead of disabling the bound altogether.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "page_size.max(1) rules out division by zero"
)]
fn page_count_from_size(size: u64, page_size: u32) -> u32 {
    u32::try_from(
        size.saturating_add(u64::from(page_size).saturating_sub(1)) / u64::from(page_size).max(1),
    )
    .unwrap_or(0)
}

/// Runs one PASSIVE checkpoint pass on `db_path`'s WAL. A missing, empty,
/// or sub-header `-wal` file is not an error — there is nothing to
/// checkpoint, and the result reports zero frames, already complete.
pub fn checkpoint_passive(
    vfs: &AnyVfs,
    db_path: &Path,
    expected_page_size: u32,
) -> Result<CheckpointResult, PagerError> {
    let wal_path = companion_path(db_path, "-wal");
    if !vfs.exists(&wal_path)? {
        return Ok(CheckpointResult {
            backfilled_frames: 0,
            total_frames: 0,
            checkpoint_complete: true,
        });
    }

    let _ckpt_guard = vfs.claim_wal_checkpoint_lock(db_path)?;

    let wal_file = vfs.open_read(&wal_path)?;
    let size = wal_file.size()?;
    if size < wal::HEADER_LEN as u64 {
        return Ok(CheckpointResult {
            backfilled_frames: 0,
            total_frames: 0,
            checkpoint_complete: true,
        });
    }
    let to_pager_error = |source| PagerError::Wal {
        path: wal_path.display().to_string(),
        source,
    };
    // The header, then frames one at a time (#875), never the whole file.
    let header = match wal::read_header(&wal_file, size) {
        Ok(header) => header,
        // Unusable header: SQLite recovers no frames, so nothing to backfill.
        Err(source) if source.is_unusable_header() => {
            return Ok(CheckpointResult {
                backfilled_frames: 0,
                total_frames: 0,
                checkpoint_complete: true,
            });
        }
        Err(source) => return Err(to_pager_error(source)),
    };
    if header.page_size != expected_page_size {
        return Err(to_pager_error(wal::WalError::InvalidPageSize {
            page_size: header.page_size,
        }));
    }

    let total_frames = u32::try_from(wal::whole_frames(size, header.page_size)).unwrap_or(u32::MAX);
    if total_frames == 0 {
        return Ok(CheckpointResult {
            backfilled_frames: 0,
            total_frames: 0,
            checkpoint_complete: true,
        });
    }

    // A live reader's mark is never 0 in slots 1..=4 — a reader that opens
    // an empty WAL (mxFrame == 0) claims the reserved slot-0 lock instead
    // of one of these four (mirrors stock SQLite's `walTryBeginRead`), so
    // this filter can't accidentally drop a real reader's bound. It exists
    // only to ignore a slot's leftover mark from before any reader ever
    // claimed it (see `active_reader_marks`'s doc comment).
    let marks = vfs.active_wal_reader_marks(db_path)?;
    let safe_frame = marks
        .into_iter()
        .filter(|&mark| mark > 0)
        .min()
        .unwrap_or(total_frames)
        .min(total_frames);

    if safe_frame == 0 {
        return Ok(CheckpointResult {
            backfilled_frames: 0,
            total_frames,
            checkpoint_complete: false,
        });
    }

    // A prior checkpoint may already have backfilled a prefix of these
    // frames — re-copying them is harmless (the same page content) but
    // wasted I/O, so a pass that can't move `nBackfill` forward at all is
    // a no-op rather than a redundant full rewrite.
    let already_backfilled = vfs.read_wal_backfill(db_path)?;
    if safe_frame <= already_backfilled {
        return Ok(CheckpointResult {
            backfilled_frames: already_backfilled,
            total_frames,
            checkpoint_complete: already_backfilled == total_frames,
        });
    }

    let (pages, db_size) =
        wal::committed_pages_from(&header, safe_frame as usize, |index, frame| {
            wal::read_frame(&wal_file, index, frame)
        })
        .map_err(to_pager_error)?;

    let db_file = vfs.open_write(db_path)?;
    // A frame's page_num is bounded by the WAL's own commit record
    // (`db_size`) or, if this pass's commit predates the file's current
    // size, by the main file's existing page count — either way, a
    // corrupted-but-checksum-valid WAL (a `page_num` near `u32::MAX`, say)
    // must never be allowed to drive `write_at` to an arbitrary offset far
    // beyond the database's actual extent. See `page_count_from_size`'s
    // own doc for why it fails closed rather than open.
    let current_pages = page_count_from_size(db_file.size()?, header.page_size);
    let max_page = db_size.max(current_pages);
    // Backfill in ascending page order (#588): HashMap iteration order
    // would scatter write_at offsets randomly across the main file; a
    // sorted pass writes sequentially.
    let mut pages: Vec<(u32, Vec<u8>)> = pages.into_iter().collect();
    pages.sort_unstable_by_key(|&(page_num, _)| page_num);
    for (page_num, content) in &pages {
        if *page_num == 0 || *page_num > max_page {
            continue;
        }
        let offset =
            u64::from(page_num.saturating_sub(1)).saturating_mul(u64::from(header.page_size));
        db_file.write_at(content, offset)?;
    }
    db_file.sync()?;

    vfs.publish_wal_backfill(db_path, safe_frame)?;

    Ok(CheckpointResult {
        backfilled_frames: safe_frame,
        total_frames,
        checkpoint_complete: safe_frame == total_frames,
    })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::wal::{WalHeader, WalWriter};
    use super::*;
    use crate::storage::row::vfs::{AnyVfs, MemoryVfs};

    fn setup(page_size: u32) -> (AnyVfs, std::path::PathBuf) {
        let mut memory = MemoryVfs::new();
        let db_path = Path::new("/test.db").to_path_buf();
        memory.insert(db_path.to_str().unwrap(), vec![0u8; page_size as usize]);
        (AnyVfs::new(memory), db_path)
    }

    #[test]
    fn page_count_from_size_computes_the_real_page_count() {
        assert_eq!(page_count_from_size(0, 512), 0);
        assert_eq!(page_count_from_size(512, 512), 1);
        assert_eq!(page_count_from_size(513, 512), 2);
        assert_eq!(page_count_from_size(1024, 512), 2);
    }

    /// #421 review finding: a file size whose page count doesn't fit in
    /// a `u32` must fail *closed* (`0`), not fall back to `u32::MAX` —
    /// the caller takes `max(db_size, this)` to bound checkpoint write
    /// offsets, so `u32::MAX` here would silently disable that bound
    /// entirely for a corrupted-but-checksum-valid WAL frame, exactly
    /// the attack this bound exists to stop.
    #[test]
    fn page_count_from_size_fails_closed_on_overflow() {
        assert_eq!(page_count_from_size(u64::MAX, 512), 0);
    }

    #[test]
    fn no_wal_file_is_a_complete_no_op() {
        let (vfs, db_path) = setup(512);
        let result = checkpoint_passive(&vfs, &db_path, 512).unwrap();
        assert_eq!(
            result,
            CheckpointResult {
                backfilled_frames: 0,
                total_frames: 0,
                checkpoint_complete: true,
            }
        );
    }

    mod mcdc_vectors {
        //! Tagged MC/DC vectors for this file's multi-leaf decisions
        //! (`mcdc__<id>__vN`, joined to
        //! `tests/mcdc/obligations.json` by `make test-mcdc`; db-core#299
        //! follow-up).

        use super::*;

        // storage_row_pager_checkpoint_checkpoint_passive_aef700eb: `source.is_unusable_header()`
        #[test]
        fn mcdc__storage_row_pager_checkpoint_checkpoint_passive_aef700eb__v1_unusable_header_checkpoints_nothing(
        ) {
            let (vfs, db_path) = setup(512);
            let wal_path = companion_path(&db_path, "-wal");
            let file = vfs.create_or_open_write(&wal_path).unwrap();
            file.write_at(&[0u8; 64], 0).unwrap();

            let result = checkpoint_passive(&vfs, &db_path, 512).unwrap();
            assert_eq!(result.backfilled_frames, 0);
            assert!(result.checkpoint_complete);
        }

        #[test]
        fn mcdc__storage_row_pager_checkpoint_checkpoint_passive_aef700eb__v2_page_size_mismatch_still_errors(
        ) {
            let (vfs, db_path) = setup(512);
            let wal_path = companion_path(&db_path, "-wal");
            let header = WalHeader::new(true, 4096, 1, 2, 1);
            let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
            writer.append_frame(1, &vec![0u8; 4096], 1).unwrap();
            writer.sync().unwrap();

            assert!(checkpoint_passive(&vfs, &db_path, 512).is_err());
        }

        // storage_row_pager_checkpoint_checkpoint_passive_2a432e1c: `*page_num == 0 || *page_num > max_page`
        #[test]
        fn mcdc__storage_row_pager_checkpoint_checkpoint_passive_2a432e1c__v1_both_false_is_backfilled(
        ) {
            let (vfs, db_path) = setup(512);
            let wal_path = companion_path(&db_path, "-wal");
            let header = WalHeader::new(true, 512, 0x1111, 0x2222, 1);
            let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
            writer.append_frame(1, &vec![0xAAu8; 512], 1).unwrap();
            writer.sync().unwrap();

            let result = checkpoint_passive(&vfs, &db_path, 512).unwrap();
            assert_eq!(result.backfilled_frames, 1);
        }

        #[test]
        fn mcdc__storage_row_pager_checkpoint_checkpoint_passive_2a432e1c__v2_page_num_zero_is_skipped(
        ) {
            let (vfs, db_path) = setup(512);
            let wal_path = companion_path(&db_path, "-wal");
            let header = WalHeader::new(true, 512, 0x3333, 0x4444, 1);
            let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
            writer.append_frame(0, &vec![0xBBu8; 512], 1).unwrap();
            writer.sync().unwrap();

            let result = checkpoint_passive(&vfs, &db_path, 512).unwrap();
            assert_eq!(result.backfilled_frames, 1);

            let db_file = vfs.open_read(&db_path).unwrap();
            let mut db_bytes = vec![0u8; 512];
            db_file.read_at(&mut db_bytes, 0).unwrap();
            assert_ne!(db_bytes, vec![0xBBu8; 512], "page 0 must never be written");
        }

        #[test]
        fn mcdc__storage_row_pager_checkpoint_checkpoint_passive_2a432e1c__v3_page_num_above_max_page_is_skipped(
        ) {
            let (vfs, db_path) = setup(512);
            let wal_path = companion_path(&db_path, "-wal");
            let header = WalHeader::new(true, 512, 0x5555, 0x6666, 1);
            let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
            // Declares a commit at db_size 1 but also writes page 100 in
            // the same frame set — page 100 is out of range for both the
            // WAL's own declared size and the (empty) main file's current
            // page count, so it must be skipped rather than seeking
            // write_at to a huge offset.
            writer.append_frame(100, &vec![0xCCu8; 512], 1).unwrap();
            writer.sync().unwrap();

            let result = checkpoint_passive(&vfs, &db_path, 512).unwrap();
            assert_eq!(result.backfilled_frames, 1);

            let db_file = vfs.open_read(&db_path).unwrap();
            assert_eq!(
                db_file.size().unwrap(),
                512,
                "no write beyond the main file's extent"
            );
        }
    }

    #[test]
    fn backfills_all_frames_when_no_readers_active() {
        let (vfs, db_path) = setup(512);
        let wal_path = companion_path(&db_path, "-wal");
        let header = WalHeader::new(true, 512, 0x1111, 0x2222, 1);
        let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
        let page1 = vec![0xAAu8; 512];
        writer.append_frame(1, &page1, 1).unwrap();
        writer.sync().unwrap();

        let result = checkpoint_passive(&vfs, &db_path, 512).unwrap();
        assert_eq!(result.backfilled_frames, 1);
        assert_eq!(result.total_frames, 1);
        assert!(result.checkpoint_complete);

        let db_file = vfs.open_read(&db_path).unwrap();
        let mut db_bytes = vec![0u8; 512];
        db_file.read_at(&mut db_bytes, 0).unwrap();
        assert_eq!(db_bytes, page1);
    }

    /// An active reader pinned to an older frame must bound the
    /// checkpoint — this requires a real `-shm` file (`MemoryVfs`'s
    /// `active_wal_reader_marks` default is always empty), so this test
    /// drives `UnixVfs` against a temp directory directly. The reader-mark
    /// lock byte (`UNIX_SHM_BASE + 3 + slot` = `120 + 3 + 1`) mirrors
    /// `src/vfs/shm.rs`'s private `wal_read_lock_byte(1)`.
    #[test]
    fn reader_mark_bounds_the_checkpoint() {
        use crate::storage::row::vfs::shm::wal_read_lock_byte;
        use crate::storage::row::vfs::test_lock_probe::{hold_multiple, release_all};
        use crate::storage::row::vfs::UnixVfs;
        use std::os::unix::fs::FileExt;

        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sqlite-rs-checkpoint-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.db");
        std::fs::write(&db_path, vec![0u8; 512]).unwrap();
        let shm_path = companion_path(&db_path, "-shm");
        std::fs::write(&shm_path, vec![0u8; 32768]).unwrap();

        let vfs = AnyVfs::new(UnixVfs);
        let wal_path = companion_path(&db_path, "-wal");
        let header = WalHeader::new(true, 512, 0x3333, 0x4444, 1);
        let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
        writer.append_frame(1, &vec![0xBBu8; 512], 1).unwrap();
        writer.append_frame(1, &vec![0xCCu8; 512], 1).unwrap();
        writer.sync().unwrap();

        // Pin a reader at frame 1 (slot 1's read-lock byte), so the
        // checkpoint can only backfill through frame 1, not frame 2.
        let held = hold_multiple(&shm_path, &[("rdlock", wal_read_lock_byte(1), 1)]);
        {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&shm_path)
                .unwrap();
            file.write_all_at(&1u32.to_ne_bytes(), 104).unwrap();
        }

        let result = checkpoint_passive(&vfs, &db_path, 512).unwrap();
        assert_eq!(result.backfilled_frames, 1);
        assert_eq!(result.total_frames, 2);
        assert!(!result.checkpoint_complete);

        let db_file = vfs.open_read(&db_path).unwrap();
        let mut db_bytes = vec![0u8; 512];
        db_file.read_at(&mut db_bytes, 0).unwrap();
        assert_eq!(db_bytes, vec![0xBBu8; 512]);

        release_all(held);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn page_size_mismatch_is_an_error() {
        let (vfs, db_path) = setup(512);
        let wal_path = companion_path(&db_path, "-wal");
        let header = WalHeader::new(true, 512, 0x5555, 0x6666, 1);
        let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
        writer.append_frame(1, &vec![0xAAu8; 512], 1).unwrap();
        writer.sync().unwrap();

        let err = checkpoint_passive(&vfs, &db_path, 1024).unwrap_err();
        match err {
            PagerError::Wal { source, .. } => {
                assert!(matches!(
                    source,
                    wal::WalError::InvalidPageSize { page_size: 512 }
                ));
            }
            other => panic!("expected PagerError::Wal, got {other:?}"),
        }
    }

    #[test]
    fn empty_wal_with_header_only_is_a_complete_no_op() {
        let (vfs, db_path) = setup(512);
        let wal_path = companion_path(&db_path, "-wal");
        let header = WalHeader::new(true, 512, 0x7777, 0x8888, 1);
        let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
        writer.sync().unwrap();

        let result = checkpoint_passive(&vfs, &db_path, 512).unwrap();
        assert_eq!(
            result,
            CheckpointResult {
                backfilled_frames: 0,
                total_frames: 0,
                checkpoint_complete: true,
            }
        );
    }

    #[test]
    fn second_pass_with_no_new_frames_is_a_no_op() {
        let (vfs, db_path) = setup(512);
        let wal_path = companion_path(&db_path, "-wal");
        let header = WalHeader::new(true, 512, 0x9999, 0xAAAA, 1);
        let mut writer = WalWriter::create(&vfs, &wal_path, header).unwrap();
        writer.append_frame(1, &vec![0xDDu8; 512], 1).unwrap();
        writer.sync().unwrap();

        let first = checkpoint_passive(&vfs, &db_path, 512).unwrap();
        assert_eq!(first.backfilled_frames, 1);
        assert!(first.checkpoint_complete);

        let second = checkpoint_passive(&vfs, &db_path, 512).unwrap();
        assert_eq!(second, first);
    }
}
