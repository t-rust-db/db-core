---
domain: storage
version: 0.1.0
status: draft
date: 2026-08-14
---

# 003 — Pager

> Moved from t-rust-db/tools `sqlite-rs/.openspec/specs/007-pager/spec.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

The page-access layer between the VFS and the b-tree cursor: resolves page numbers to bytes, refuses to serve a database with a hot rollback journal, and (Requirement 2) merges in committed WAL frames for databases with an uncheckpointed `-wal` file. Backs V1 step 2 (#35) and step 6 (#36), part of epic #5 phase 3. Depends on spec 001 (VFS, header) and spec 002 (the `PageSource` trait `TableCursor`/`IndexCursor` already consume generically — this spec's `Pager` implements it directly, so neither cursor changes). Refs: sqlite-rs 001/Req-4.

Everything in this spec is **Tier 0 READ CORE** — never droppable.

## Philosophy

`Pager` replaces `VfsPageSource` (spec 001) as the page source `TableCursor`/`IndexCursor` are built against, adding exactly two things `VfsPageSource` doesn't: hot-journal refusal (Requirement 1) and WAL-frame merging (Requirement 2). Everything else — page fetch, page size, reserved bytes — is unchanged, by construction: `Pager` wraps a `VfsPageSource` internally rather than reimplementing page reads. `src/storage/row/pager/` is not exempt from the `mvl-limit` qualified-subset gate (no `unsafe`/`dyn`/explicit lifetimes); `Pager::open` takes its `Vfs` generically (`<V: Vfs>`), never as `&dyn Vfs`, so the trait-object boundary stays inside `src/storage/row/vfs/` where it's already accounted for. That generic-parameter shape is the reference pattern for keeping the boundary contained; the VDBE deliberately keeps a second, narrower `dyn` boundary instead (`Rc<dyn PageSource>` in `src/engine/row.rs` and `src/engine/row/adapter.rs`, exempted since #90). sqlite-rs ADR-0013 considered making `Vm` generic over `P: PageSource` (#114) and rejected it: a `Vm` shares one page source across N open cursors via cheap `Rc` clones, so genericizing would force the no-database constructor path (`Vm::new()`) to name a concrete `P`, drop `Vm`'s `Default` derive, and thread `<P: PageSource>` through every opcode handler — trading a justified trait object for pervasive generic noise without changing runtime dispatch, since real callers would still instantiate `Vm<Rc<dyn PageSource>>`.

Locking was out of scope for both requirements below at V1 (#35/#36): Spike 005 (#8, closed) validated the *approach* — byte-identical `fcntl` locks (journal-mode lock ladder and WAL `-shm` reader-mark protocol) genuinely interoperate with a live stock `sqlite3` process — but implementing it was separate, larger scope than a page-view layer, tracked as a follow-up (#45). #45 shipped the SHARED-lock/busy-detection/WAL-reader-mark slice; the full 5-state lock ladder (`src/storage/row/vfs/lock.rs`'s `FileLockState`, #357) and its use in hot-journal recovery (Requirement 1's RESERVED-probe clause below, #359) are V5 scope (`.openspec/plan.md`'s V5 Slim block: "Pager: Journal mode DELETE, hot-journal recovery, all 5 lock states"). `Pager` now takes real file locks — this paragraph's older "`Pager` takes no file locks" was accurate at V1 and is superseded.

## Requirements

### Requirement 1: Hot-Journal Detection [MUST]

The system MUST detect (never silently ignore) a database that has an adjacent `-journal` file with a valid rollback-journal header, rather than risk serving pre-rollback (uncommitted) pages as committed data. A `-journal` file that exists but does not start with the rollback-journal magic (e.g. zeroed by `PRAGMA journal_mode=PERSIST`'s post-commit reset, or too short to hold a full header) is not hot and MUST NOT block opening. V1 responded to a detected hot journal by refusing to open (`PagerError::HotJournal`); Requirement 6 (#172) upgrades this to actual recovery — `PagerError::HotJournal` is retained only as the "no `-journal` companion" precondition this requirement establishes, not as `open`'s outcome once a hot journal is found.

A journal confirmed hot by magic alone is not necessarily safe to replay: if another connection already holds RESERVED (or higher) on the main file, that connection is either mid-transaction or already rolling this same journal back itself, and replaying it here too would race it (V5, #359; ADR-0035). `Pager::open` MUST non-blocking-probe RESERVED before recovering — matching stock SQLite's `hasHotJournal`/`sqlite3PagerSharedLock` (`os_unix.c`/`pager.c`) — and MUST fail with `VfsError::Locked` rather than recover if the probe finds it held. When clear, the lock MUST escalate straight from SHARED to EXCLUSIVE, deliberately skipping RESERVED (see `FileLock::escalate_to_exclusive`'s doc comment for why), and every read/write/truncate the probe and the replay perform MUST go through the one fd `Pager::open` already holds the lock on — never a second, independently-opened handle to the same path (ADR-0035's fd-trap rationale).

**Implementation:** `src/storage/row/pager.rs::Pager::open`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/pager.rs`

**Corpus:** `sqlite-rs:tests/corpus/fixtures/journalstates/`

#### Scenario: Hot journal is detected and never silently ignored

- GIVEN `journalstates/hot_journal.db` and its adjacent `hot_journal.db-journal` (a rollback-journal writer that spilled uncommitted pages into the main file before being interrupted — see `sqlite-rs:tools/gen_fixtures.sh`)
- WHEN `Pager::open` is called
- THEN the journal's magic is recognized as hot before any page is read, and Requirement 6's recovery runs rather than the main file's spilled pages being served as committed data

**Tests:** `sqlite-rs:tests/unit/pager_fixtures.rs::hot_journal_fixture_recovers_committed_state`

#### Scenario: A live connection holding RESERVED blocks recovery

- GIVEN a hot journal (valid magic) and a second, live connection holding RESERVED (or higher) on the main file
- WHEN `Pager::open` is called
- THEN it fails with `VfsError::Locked` rather than recovering, and both the journal and the main file are left untouched

**Tests:** `src/storage/row/pager.rs::tests::hot_journal_open_fails_when_another_connection_holds_reserved`

#### Scenario: Cold or absent journal opens cleanly

- GIVEN a database with no `-journal` file, or one that exists but is zeroed/too short to hold a valid header
- WHEN `Pager::open` is called
- THEN it succeeds

**Tests:** `src/storage/row/pager.rs::tests::no_journal_opens_cleanly`, `src/storage/row/pager.rs::tests::zeroed_persist_mode_journal_is_not_hot`, `src/storage/row/pager.rs::tests::empty_journal_file_is_not_hot`, `src/storage/row/pager.rs::tests::short_journal_file_is_not_hot`

### Requirement 2: Page-View Abstraction, Zero Behavior Change [MUST]

`Pager` MUST implement the same `PageSource` trait `VfsPageSource` does, so `TableCursor<Pager>` / `IndexCursor<Pager>` produce byte-identical results to `TableCursor<VfsPageSource>` / `IndexCursor<VfsPageSource>` on every fixture that has no hot journal and no pending WAL — including auto-vacuum databases, where the b-tree cursor's pointer-following traversal never visits the interleaved pointer-map pages directly and therefore needs no pointer-map-specific logic in `Pager`.

**Implementation:** `src/storage/row/pager.rs::Pager` (`impl PageSource for Pager`)

**Tests:** inline `#[cfg(test)]` in `src/storage/row/pager.rs`

**Corpus:** `sqlite-rs:tests/corpus/fixtures/btrees/`, `sqlite-rs:tests/corpus/fixtures/features/autovacuum.db`

#### Scenario: At-rest fixture unchanged

- GIVEN `btrees/table_single_page.db`
- WHEN read through `TableCursor<Pager>` instead of `TableCursor<VfsPageSource>`
- THEN the decoded rows are identical

**Tests:** `sqlite-rs:tests/unit/pager_fixtures.rs::table_single_page_fixture_reads_identically_through_pager`

#### Scenario: Auto-vacuum fixture unaffected by pointer-map page

- GIVEN `features/autovacuum.db`, whose table `t` root page is discovered via `read_schema` (never hardcoded, since the interleaved pointer-map page can shift it)
- WHEN read through `TableCursor<Pager>`
- THEN the row decodes identically to the non-auto-vacuum case

**Tests:** `sqlite-rs:tests/unit/pager_fixtures.rs::autovacuum_fixture_reads_identically_through_pager`

### Requirement 3: WAL Frame Reading (Read-Only Recovery) [MUST]

For a WAL-mode database with a non-empty, sub-header-length-or-larger `-wal` file, `Pager` MUST parse the WAL header (both checksum-endianness variants — magic `0x377f0682` is native byte order, the common case, `0x377f0683` is always big-endian, per spike #7 finding 2), walk its frames validating checksum and salt, and overlay the latest committed frame per page over the main file's pages. A frame whose salts don't match the header's, or whose checksum doesn't verify, ends the scan (not an error — a torn tail is the normal shape of a WAL file mid-write); everything published by an earlier commit in the same scan survives. No `-shm` file is read — this is quiescent, read-only recovery only (live-writer coexistence is validated as needed by spike 005/#8, closed; implementation tracked as #45). A malformed WAL *header* (bad magic, too short, bad checksum, or a page size that doesn't match the main database's) MUST return `PagerError::Wal`, never panic; a missing or empty `-wal` file (the common case: a fully checkpointed WAL) is not an error and yields no overlay.

**Implementation:** `src/storage/row/pager/wal.rs`, `src/storage/row/pager.rs::read_wal_pages`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/pager/wal.rs` and `src/storage/row/pager.rs`

**Corpus:** `sqlite-rs:tests/corpus/fixtures/journalstates/`

#### Scenario: Uncheckpointed WAL rows are visible

- GIVEN `journalstates/wal_pending.db` and its adjacent `wal_pending.db-wal` (three separate commits to the same page, none checkpointed into the main file)
- WHEN read through `TableCursor<Pager>`
- THEN all three rows are visible, matching `sqlite3`

**Tests:** `sqlite-rs:tests/unit/pager_fixtures.rs::wal_pending_fixture_shows_uncheckpointed_rows`

#### Scenario: Both checksum-endianness paths decode identically

- GIVEN `wal_pending_bigendian.db-wal` — `wal_pending.db-wal`'s content with the magic flipped to `0x377f0683` and every checksum independently recomputed in big-endian arithmetic
- WHEN read through `TableCursor<Pager>`
- THEN the decoded rows are identical to `wal_pending.db`'s

**Tests:** `sqlite-rs:tests/unit/pager_fixtures.rs::wal_pending_bigendian_fixture_decodes_identically`, `src/storage/row/pager/wal.rs::tests::native_checksum_header_parses`, `src/storage/row/pager/wal.rs::tests::bigendian_checksum_header_parses`

#### Scenario: Stale foreign-generation frame is rejected

- GIVEN `wal_pending_stale.db-wal`, which has a committed frame from an unrelated WAL generation (different salts) appended after its own last commit
- WHEN read through `TableCursor<Pager>`
- THEN only the two rows from this generation's own commits are visible — the foreign frame's row never surfaces

**Tests:** `sqlite-rs:tests/unit/pager_fixtures.rs::wal_pending_stale_fixture_rejects_foreign_frame`, `src/storage/row/pager/wal.rs::tests::stale_foreign_frame_is_rejected_on_salt_mismatch`

#### Scenario: Trailing spilled-but-uncommitted frames are ignored

- GIVEN `wal_pending_trailing.db-wal`, where a big transaction spilled dirty pages into the WAL as non-commit frames before rolling back
- WHEN read through `TableCursor<Pager>`
- THEN only the pre-existing committed row is visible — none of the rolled-back insert

**Tests:** `sqlite-rs:tests/unit/pager_fixtures.rs::wal_pending_trailing_fixture_shows_only_committed_row`, `src/storage/row/pager/wal.rs::tests::trailing_spilled_frames_are_ignored`

#### Scenario: Malformed WAL never panics

- GIVEN arbitrary malformed byte sequences in place of a `-wal` file's contents
- WHEN parsed
- THEN `WalHeader::parse` and `committed_pages` return errors or empty results, never panic (fuzz target)

**Tests:** `src/storage/row/pager/wal.rs::tests::too_short_is_err_not_panic`, `src/storage/row/pager/wal.rs::tests::bad_magic_is_err`, `src/storage/row/pager/wal.rs::tests::corrupted_header_checksum_is_err`, `src/storage/row/pager/wal.rs::tests::garbage_input_never_panics`, `sqlite-rs:tests/fuzz/fuzz_targets/wal_frames.rs`

### Requirement 4: Dirty Page Tracking and Flush [MUST]

Unlike Requirements 1-3, this requirement is **Tier 2 WRITE CORE** (V3 phase 1, epic #161, #166) — the pager's first write-path capability, added on top of the read-only Requirements 1-3 above without changing their behavior. `Pager::get_page_mut` MUST return a mutable buffer for a page, transparently reading it first (through the same WAL-overlay-then-file precedence `read_page` already uses) the first time it's requested since the last flush, and caching it as dirty. `Pager::read_page` MUST consult this dirty cache ahead of the WAL overlay, so a page mutated via `get_page_mut` reads back its new bytes immediately through the same `Pager`, even before `flush` runs. `Pager::flush` MUST write every dirty page back to the underlying file (in ascending page-number order — a deterministic default, not a correctness requirement of this requirement alone, since no partial-flush recovery exists in this pager yet; see #172), `fsync` it, and clear the dirty set.

Writing to the underlying file requires a read-write file handle. Rather than open a second file descriptor to the same path alongside the existing read-only one — which would trip the documented "`close()` drops all `fcntl` locks held on that inode, regardless of which fd acquired them" hazard (this spec's Requirement 1 doc, #45) before #45's per-inode fd-cache exists to guard against it — `Pager::open` now acquires its single file handle via the new `Vfs::open_write`/`VfsFile::write_at`/`VfsFile::sync` surface (`WritablePageSource`, `src/storage/row/vfs/page_source.rs`) instead of `Vfs::open_read`. Every other read-only `PageSource` consumer (`VfsPageSource` itself, used directly by non-`Pager` cursors) is unaffected: it keeps using `Vfs::open_read`, never asking a genuinely read-only filesystem for write access it doesn't need.

**Implementation:** `src/storage/row/pager.rs::Pager::get_page_mut`, `src/storage/row/pager.rs::Pager::flush`, `src/storage/row/vfs/page_source.rs::WritablePageSource`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/pager.rs` and `src/storage/row/vfs.rs`

**Corpus:** `sqlite-rs:tests/corpus/pager_write_test.rs` (shells out to a real, writable `sqlite3`-created fixture rather than a committed corpus file, since this requirement's whole point is writing)

#### Scenario: A mutated page reads back immediately, and again after flush

- GIVEN an open `Pager` over a two-page database
- WHEN `get_page_mut(2)` is called and its returned buffer is overwritten, then `read_page(2)` is called before `flush`, then again after `flush`
- THEN both reads return the new bytes, the untouched page 1 is unaffected, and a freshly-opened `Pager` over the same file also sees the new bytes on page 2 and the original bytes on page 1

**Tests:** `src/storage/row/pager.rs::tests::get_page_mut_then_flush_roundtrips`, `src/storage/row/pager.rs::tests::flush_with_no_dirty_pages_is_a_no_op`

#### Scenario: A flushed page still opens in stock `sqlite3`

- GIVEN a database created by stock `sqlite3` with one table and one row
- WHEN a page is fetched via `get_page_mut`, written back unchanged, and flushed
- THEN stock `sqlite3` still opens the file, `PRAGMA integrity_check` reports `ok`, and the original row reads back unchanged — the compatibility proof this whole epic (#161) is gated on

**Tests:** `sqlite-rs:tests/corpus/pager_write_test.rs::flushed_page_still_opens_and_integrity_checks_in_stock_sqlite3`

#### Scenario: Both VFS backends satisfy the write contract

- GIVEN a file opened via `Vfs::open_write`
- WHEN bytes are written at an offset and synced
- THEN a fresh `Vfs::open_read` handle on the same path reads back the new bytes — true for both `UnixVfs` and `MemoryVfs`

**Tests:** `src/storage/row/vfs.rs::tests::memory_vfs_contract`, `src/storage/row/vfs.rs::tests::unix_vfs_contract`

### Requirement 5: Freelist Allocate/Deallocate [MUST]

Tier 2 WRITE CORE (V3 phase 1, epic #161, #167), built on top of Requirement 4's dirty-page-tracking/flush primitives. `Pager::allocate_page` MUST return a free page number: popping one off the freelist (a leaf page number from the current trunk page's leaf array, or the trunk page itself once its leaf array is empty, promoting the trunk's own next-trunk pointer) when the freelist is non-empty, or extending the database by one page (incrementing the header's page-count field) when it is empty. `Pager::deallocate_page` MUST return a page to the freelist: appended to the current trunk page's leaf array if it has room (`(page_size - 8) / 4` entries), or made the new trunk page itself (pointing at the old trunk) once the current trunk is full. Both operations MUST update the header's freelist-trunk-page and freelist-page-count fields (bytes 32-35 and 36-39) on page 1 in the same call, via `Pager::get_page_mut`, so the bookkeeping is flushed atomically with the allocation/deallocation on the next `Pager::flush`.

Refs: 001/Req-2, 003/Req-4.

**Implementation:** `src/storage/row/pager.rs::Pager::allocate_page`, `src/storage/row/pager.rs::Pager::deallocate_page`, `src/storage/row/pager/freelist.rs::TrunkPage`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/pager.rs` and `src/storage/row/pager/freelist.rs`

**Corpus:** `sqlite-rs:tests/corpus/pager_write_test.rs`

#### Scenario: Allocation extends the file when the freelist is empty

- GIVEN a `Pager` over a database with an empty freelist
- WHEN `allocate_page` is called
- THEN it returns `page_count + 1` and the header's page-count field is incremented; no freelist field changes

**Tests:** `src/storage/row/pager.rs::tests::allocate_with_empty_freelist_extends_file`

#### Scenario: Deallocate then allocate round-trips a single page

- GIVEN a `Pager` over a database with an empty freelist
- WHEN a page is deallocated (becoming the sole trunk page) and then allocated again
- THEN the same page number is returned, and the header's freelist-trunk-page and freelist-page-count fields return to their original (empty) values

**Tests:** `src/storage/row/pager.rs::tests::deallocate_then_allocate_round_trips_single_page`

#### Scenario: Trunk leaves fill before a new trunk is chained

- GIVEN a `Pager` whose freelist has one trunk page with room for more leaves
- WHEN additional pages are deallocated
- THEN they append to the existing trunk's leaf array, and `allocate_page` pops leaves before ever consuming the trunk page itself; once a trunk is full, the next deallocation chains a new trunk page pointing at the old one

**Tests:** `src/storage/row/pager.rs::tests::deallocate_appends_to_existing_trunk_leaves`, `src/storage/row/pager.rs::tests::deallocate_overflows_into_new_trunk_when_full`

#### Scenario: Allocate/deallocate round trip still integrity-checks in stock `sqlite3`

- GIVEN a database created by stock `sqlite3` with one table and one row
- WHEN a page is allocated (extending the file), flushed, then deallocated (returned to the freelist), and flushed again
- THEN stock `sqlite3` still opens the file, `PRAGMA integrity_check` reports `ok`, `PRAGMA freelist_count` reports `1`, and the original row reads back unchanged

**Tests:** `sqlite-rs:tests/corpus/pager_write_test.rs::allocate_then_deallocate_page_still_integrity_checks_in_stock_sqlite3`

### Requirement 6: Rollback Journal Write Path, Commit, and Recovery [MUST]

Tier 2 WRITE CORE (V3 phase 1, epic #161, #172; the RESERVED-probe/single-fd locking added in V5 #359 is ADR-0035), built on top of Requirement 4's dirty-page-tracking/flush primitives and Requirement 1's hot-journal detection. `Pager::flush` MUST, before overwriting any page that existed prior to the current transaction (page number `<=` the page count recorded on disk at the start of `flush`), write that page's pre-transaction content as a checksummed record into a `-journal` companion file (DELETE mode only — TRUNCATE/PERSIST/MEMORY are out of scope), `fsync` the journal, then write and `fsync` the dirty pages to the main file, then delete the journal. Pages beyond the pre-transaction page count (freshly allocated by `Pager::allocate_page` within the same transaction) are never journaled: a crash before commit leaves them referenced by nothing, and recovery's truncate-to-pre-transaction-page-count step drops them. `Pager::open` MUST, on detecting a hot journal (Requirement 1), replay every checksum-valid record from it into the main file, truncate the main file back to the journal's recorded pre-transaction page count, `fsync`, and delete the journal, before proceeding with the rest of `open`. The on-disk journal header and per-page checksum algorithm MUST match stock SQLite's `pager.c` byte-for-byte (28-byte header, `pager_cksum`'s every-200-bytes sampling) so a journal either implementation writes is recoverable by the other.

Refs: sqlite-rs 001/Req-4, 003/Req-1, 003/Req-4.

**Implementation:** `src/storage/row/pager.rs::Pager::flush`, `src/storage/row/pager.rs::recover_hot_journal`, `src/storage/row/pager/journal.rs`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/pager.rs` and `src/storage/row/pager/journal.rs`

**Corpus:** `sqlite-rs:tests/corpus/fixtures/journalstates/hot_journal.db`, `sqlite-rs:tests/corpus/journal_interop_test.rs`

#### Scenario: A committed transaction leaves no journal behind

- GIVEN an open `Pager` with dirty pages
- WHEN `flush` is called
- THEN the main file reflects every dirty page, and the `-journal` file (if one was created) no longer exists afterward

**Tests:** `sqlite-rs:tests/tiers/tier2.rs::t2_journal_transactions_commit_and_rollback`, `src/storage/row/pager.rs::tests::get_page_mut_then_flush_roundtrips`

#### Scenario: A statement that never reaches flush leaves the database unchanged

- GIVEN an open `Pager` with a page mutated via `get_page_mut`
- WHEN the `Pager` is dropped without calling `flush`
- THEN a freshly-opened `Pager` over the same file sees the original, pre-mutation content

**Tests:** `sqlite-rs:tests/tiers/tier2.rs::t2_statement_atomicity`

#### Scenario: A crash between journal-sync and main-file-sync rolls back on next open

- GIVEN a main file with a torn write to a page, and an adjacent well-formed `-journal` recording that page's pre-transaction content
- WHEN `Pager::open` is called
- THEN the torn page is restored to its pre-transaction content, and the journal is deleted before `open` returns

**Tests:** `sqlite-rs:tests/tiers/tier2.rs::t2_journal_transactions_commit_and_rollback`, `src/storage/row/pager.rs::tests::hot_journal_with_one_record_restores_original_page_and_deletes_journal`, `src/storage/row/pager/journal.rs::tests::writer_then_recover_restores_original_pages`

#### Scenario: A real sqlite3-written hot journal recovers through our Pager

- GIVEN `journalstates/hot_journal.db` and its adjacent `hot_journal.db-journal` — a real `sqlite3` rollback-journal writer that spilled ~1999 uncommitted rows into the main file before being interrupted
- WHEN `Pager::open` is called (against a scratch copy — recovery mutates the main file and deletes the journal in place)
- THEN only the one row genuinely committed before the transaction is visible, and the journal is gone

**Tests:** `sqlite-rs:tests/unit/pager_fixtures.rs::hot_journal_fixture_recovers_committed_state`, `sqlite-rs:tests/tiers/tier0.rs::t0_hot_journal_recovers_committed_state`

#### Scenario: A journal we write recovers through a real sqlite3

- GIVEN a database created by stock `sqlite3`, and a `-journal` file written via `JournalWriter` recording a page's pre-transaction content, with that page then torn in the main file (simulating a crash mid-flush)
- WHEN a real `sqlite3` opens the database
- THEN it transparently rolls back to the pre-transaction content and deletes the journal, with no explicit recovery command needed

**Tests:** `sqlite-rs:tests/corpus/journal_interop_test.rs::our_journal_recovers_through_stock_sqlite3`

### Requirement 7: PASSIVE WAL Checkpoint [MUST]

Tier 3 (V6 Slim, epic #354, #386; ADR-0036), built on top of Requirement 3's WAL-frame reading. `checkpoint_passive` MUST copy every committed WAL frame up to the oldest active reader's published mark (`active_wal_reader_marks`, guarded by `claim_wal_checkpoint_lock`) into the main database file, in frame order, then publish the new backfill boundary (`publish_wal_backfill`). It MUST NOT wait for a lagging reader to finish — a reader still pinned to an older frame simply bounds how far a given pass can go (`CheckpointResult::checkpoint_complete = false`), rather than blocking; FULL/RESTART checkpoint modes are out of scope, deferred to V7. A missing, empty, or sub-header-length `-wal` file is not an error: it MUST return a `CheckpointResult` reporting zero frames, already complete. A WAL page size that doesn't match `expected_page_size` MUST return `Err`, never panic or checkpoint a mismatched-page-size WAL.

**Implementation:** `src/storage/row/pager/checkpoint.rs::checkpoint_passive`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/pager/checkpoint.rs`

#### Scenario: No WAL file is a complete no-op

- GIVEN a database path with no adjacent `-wal` file
- WHEN `checkpoint_passive` runs
- THEN it returns `CheckpointResult { backfilled_frames: 0, total_frames: 0, checkpoint_complete: true }`

**Tests:** `src/storage/row/pager/checkpoint.rs::tests::no_wal_file_is_a_complete_no_op`, `src/storage/row/pager/checkpoint.rs::tests::empty_wal_with_header_only_is_a_complete_no_op`

#### Scenario: Every frame backfills when no reader is active

- GIVEN a WAL with several committed frames and no active reader marks
- WHEN `checkpoint_passive` runs
- THEN every frame is copied into the main file and `checkpoint_complete` is `true`

**Tests:** `src/storage/row/pager/checkpoint.rs::tests::backfills_all_frames_when_no_readers_active`

#### Scenario: An active reader's mark bounds the checkpoint

- GIVEN a WAL with committed frames beyond an active reader's published mark
- WHEN `checkpoint_passive` runs
- THEN only frames up to that mark are backfilled, and `checkpoint_complete` is `false`

**Tests:** `src/storage/row/pager/checkpoint.rs::tests::reader_mark_bounds_the_checkpoint`

#### Scenario: A page-size mismatch errors rather than checkpointing garbage

- GIVEN a WAL header whose page size does not match `expected_page_size`
- WHEN `checkpoint_passive` runs
- THEN it returns `Err`, and the main file is left untouched

**Tests:** `src/storage/row/pager/checkpoint.rs::tests::page_size_mismatch_is_an_error`

#### Scenario: A second pass with no new frames is a no-op

- GIVEN a WAL already fully checkpointed by a prior pass
- WHEN `checkpoint_passive` runs again with no new frames written
- THEN it reports the same backfill boundary and `checkpoint_complete: true`, writing nothing new

**Tests:** `src/storage/row/pager/checkpoint.rs::tests::second_pass_with_no_new_frames_is_a_no_op`
