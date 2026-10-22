---
domain: storage
version: 0.1.0
status: draft
date: 2026-08-14
---

# 002 — B-Tree

> Moved from t-rust-db/tools `sqlite-rs/.openspec/specs/006-btree/spec.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

The table and index b-tree cursors and write path: page header/cell-pointer-array layout, cell decode, key comparison, overflow-chain reassembly, and cell insert/delete with split and collapse. Backs V1 step 4 (#32) and step 5 (#33), part of epic #5 phase 2. Depends on spec 001 (header, VFS, record decode). Validated in part by spike 005 (#12), which exercised interior-node traversal and overflow reassembly against real multi-page/overflow fixtures before this spec was written.

Everything in this spec is **Tier 0 READ CORE** — never droppable.

## Philosophy

Transcribed from [fileformat2.html](https://www.sqlite.org/fileformat2.html)'s b-tree page and cell-payload-overflow sections, verified byte-by-byte against the pinned oracle (sqlite-rs spec 004) rather than against any secondary source. `src/storage/row/btree/` is not exempt from the `mvl-limit` qualified-subset gate (no `unsafe`/`dyn`/explicit lifetimes); the `PageSource` trait this module depends on is defined in `src/storage/row/vfs/` for exactly that reason. Tier 0 core carries no exclusions at all — where the gate and a convenience collide here, the code changes: `TableCursor::prev()`'s precondition is a real `BtreeError::CursorNotPositioned` rather than a `debug_assert!` (outside the gate's macro allowlist), which also makes the check survive into release builds. The exempt set is `src/storage/row/vfs/` plus, since #90, the VDBE's `Rc<dyn PageSource>` handle in `src/vm/row/vm.rs` — a deliberate, permanent second boundary (sqlite-rs ADR-0013 considered genericizing `Vm` over `P: PageSource` instead, #114, and rejected it; see the Makefile's boundary policy).

## Requirements

### Requirement 1: Table B-Tree Page Format [MUST]

The system MUST parse table b-tree pages (page type `0x0d` leaf, `0x05` interior): the 8-byte leaf / 12-byte interior page header, the cell-pointer array, and — for page 1 specifically — resolve cell-pointer-array offsets relative to byte 0 of the page, not to the 100-byte file header that precedes the b-tree page header on that page only.

**Implementation:** `src/storage/row/btree.rs`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/btree.rs`

**Corpus:** `sqlite-rs:tests/corpus/fixtures/btrees/`

#### Scenario: Page-1 trap

- GIVEN page 1, which carries the 100-byte file header before its own b-tree page header
- WHEN the cursor reads page 1 as a table b-tree root (e.g. `sqlite_master`, always rootpage 1)
- THEN cell-pointer-array offsets MUST resolve relative to byte 0 of the page, not to byte 100

**Tests:** `src/storage/row/btree.rs::page_one_trap_sqlite_master_root_is_page_one`

#### Scenario: Interior-node depth-first traversal

- GIVEN a table b-tree spanning multiple pages (interior + leaf nodes)
- WHEN the cursor walks the full table via `first()`/`next()`
- THEN every row is visited exactly once, in ascending rowid order, matching the pinned oracle row-for-row

**Tests:** `src/storage/row/btree.rs::table_multipage_full_scan_matches_oracle`

#### Scenario: Point lookup by rowid

- GIVEN a multi-page table b-tree
- WHEN `seek(rowid)` is called for an existing rowid, the first rowid, the last rowid, or a rowid that does not exist
- THEN the result MUST match the pinned oracle's point-lookup result (`Some` row or `None`)

**Tests:** `src/storage/row/btree.rs::table_multipage_seek_matches_oracle`

### Requirement 2: Cell Payload Overflow [MUST]

The system MUST compute a cell's local payload size using SQLite's overflow formula (`max_local = usable_size - 35`; `min_local = ((usable_size - 12) * 32 / 255) - 23`; `K = min_local + (payload_len - min_local) % (usable_size - 4)`) and, when the payload overflows, walk the overflow-page chain (each overflow page's first 4 bytes are the next page number, or 0 to end the chain) to reassemble the full payload byte-for-byte.

**Implementation:** `src/storage/row/btree.rs`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/btree.rs`

**Corpus:** `sqlite-rs:tests/corpus/fixtures/btrees/` (`overflow_single_page.db`, `overflow_multi_page.db`)

#### Scenario: Single-page overflow

- GIVEN a cell whose payload spills into exactly one overflow page
- WHEN the cursor reads that row
- THEN the reassembled payload MUST be byte-identical to the pinned oracle's value

**Tests:** `src/storage/row/btree.rs::overflow_single_page_payload_is_byte_identical_to_oracle`

#### Scenario: Multi-page overflow chain

- GIVEN a cell whose payload spans a chain of overflow pages (validated at 14 pages in spike 005, #12)
- WHEN the cursor reads that row
- THEN the reassembled payload MUST be byte-identical to the pinned oracle's value

**Tests:** `src/storage/row/btree.rs::overflow_multi_page_payload_is_byte_identical_to_oracle`

### Requirement 3: Malformed Input Never Panics [MUST]

Every page-parsing and overflow-reassembly path MUST return `Err`, never panic, on truncated pages, unexpected page types, out-of-bounds cell pointers, implausible payload-length claims, and overflow chains that end early or exceed a sanity-bounded page count (cycle protection). A fuzz target MUST exercise this against arbitrary bytes.

**Implementation:** `src/storage/row/btree.rs`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/btree.rs`; `sqlite-rs:tests/fuzz/fuzz_targets/btree_cursor.rs`

#### Scenario: Truncated or malformed page

- GIVEN a page shorter than its declared header, or with an unrecognized page-type byte
- WHEN the cursor attempts to read it
- THEN the cursor MUST return `Err`, never panic

**Tests:** `src/storage/row/btree.rs::truncated_page_errors_not_panics`, `src/storage/row/btree.rs::unexpected_page_type_errors_not_panics`, `src/storage/row/btree/index.rs::truncated_page_errors_not_panics`, `src/storage/row/btree/index.rs::unexpected_page_type_errors_not_panics`

#### Scenario: Broken overflow chain

- GIVEN an overflow chain that reaches a terminating page (next pointer `0`) before all declared payload bytes have been read
- WHEN the cursor reassembles the payload
- THEN the cursor MUST return `Err`, never panic or silently truncate

**Tests:** `src/storage/row/btree.rs::overflow_chain_hitting_page_zero_early_errors_not_panics`

#### Scenario: Fuzz safety

- GIVEN arbitrary byte input treated as a page
- WHEN the fuzz target runs
- THEN the cursor MUST never panic, regardless of input

**Tests:** `sqlite-rs:tests/fuzz/fuzz_targets/btree_cursor.rs`

### Requirement 4: Rowid-Alias Columns Are Not This Layer's Job [SHOULD]

A column declared exactly `INTEGER PRIMARY KEY` is not stored in the record (SQLite encodes it as `NULL` and expects the reader to substitute the cell's own rowid). This module MUST return the record payload faithfully, including that `NULL`, rather than attempting schema-aware substitution — that requires knowing which column, if any, is the alias, which is DDL/schema information this layer does not have. Found and documented in spike 005 (#12); the substitution itself is deferred to the DDL reader (#34) or a higher row-assembly layer.

**Implementation:** `src/storage/row/btree.rs` (module doc's rowid-alias note)

#### Scenario: Rowid-alias column decodes as NULL, not silently wrong

- GIVEN a table with an `INTEGER PRIMARY KEY` column
- WHEN the cursor decodes a row's payload via `record::decode_record`
- THEN the alias column's decoded value MUST be `Value::Null` (faithful to the stored bytes), and callers needing the real value MUST substitute the row's own `rowid`

**Tests:** `src/storage/row/btree.rs::rowid_alias_column_decodes_as_null_not_substituted`

#### Scenario: A higher layer substitutes the alias column with the row's own rowid

- GIVEN a table with an `INTEGER PRIMARY KEY` column, queried via `SELECT`
- WHEN the alias column is projected (directly or via `SELECT *`), including through a covering-index scan where the value comes from an index leaf's own rowid rather than a table lookup
- THEN the returned value MUST be the row's actual rowid, byte-identical to the pinned oracle's value, never the `Value::Null` this layer returns

**Tests:** `sqlite-rs:src/dump.rs::tests::rowid_alias_detects_plain_integer_primary_key`, `sqlite-rs:tests/corpus/no_stats_optimizations_test.rs::covering_index_select_star_with_rowid_alias_matches_oracle`, `sqlite-rs:tests/corpus/no_stats_optimizations_test.rs::covering_index_select_star_with_rowid_alias_non_unique_duplicates_matches_oracle`

Substitution landed via #34 (DDL reader)'s `rowid_alias_from_sql`/`TableSchema::rowid_alias`, now wired through `src/codegen/row/select/projection.rs`, `src/codegen/row/stmt/insert.rs`, and `src/codegen/row/stmt/update.rs` — this requirement's original "(planned)" note was stale; the two scenarios above are both discharged.

### Requirement 5: Index B-Tree Page Format [MUST]

The system MUST parse index b-tree pages (page type `0x0a` leaf, `0x02` interior). Unlike table b-tree interior cells (routing-only, no payload), index b-tree interior cells carry a full key payload — a real, sorted entry with a left-child subtree of lesser keys, not merely a separator — so in-order traversal MUST yield each interior cell's own key interleaved with descending into its children. WITHOUT ROWID tables are stored as index b-trees (confirmed on a real fixture in spike 005, #12: FTS5's `t_idx`/`t_config` shadow tables) and MUST be readable through this cursor.

**Implementation:** `src/storage/row/btree/index.rs`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/btree/index.rs`

**Corpus:** `sqlite-rs:tests/corpus/fixtures/btrees/` (`index.db`, `without_rowid.db`)

#### Scenario: Secondary-index walk in BINARY order

- GIVEN a multi-page secondary index over a rowid table
- WHEN the cursor walks the full index via `first()`/`next()`
- THEN every entry is visited exactly once, in ascending BINARY-collation key order, matching the pinned oracle key-for-key (including lexicographic, not numeric, text ordering)

**Tests:** `src/storage/row/btree/index.rs::secondary_index_walk_matches_oracle_binary_order`

#### Scenario: WITHOUT ROWID table read via index b-tree

- GIVEN a `WITHOUT ROWID` table (stored as an index b-tree keyed on its declared primary key)
- WHEN the cursor walks the full table via `first()`/`next()`
- THEN every row is visited exactly once, in ascending primary-key order, matching the pinned oracle row-for-row (this is sqlite-rs spec 001 Req 4's "WITHOUT ROWID" scenario, made concrete here)

**Tests:** `src/storage/row/btree/index.rs::without_rowid_table_is_readable_as_index_btree`

#### Scenario: Malformed index page

- GIVEN a page shorter than its declared header, or with an unrecognized page-type byte, where an index b-tree page was expected
- WHEN the cursor attempts to read it
- THEN the cursor MUST return `Err`, never panic

**Tests:** `src/storage/row/btree/index.rs::truncated_page_errors_not_panics`, `src/storage/row/btree/index.rs::unexpected_page_type_errors_not_panics`

### Requirement 6: Minimal Key Comparison and Seek [SHOULD]

The system SHOULD provide enough key ordering to walk an index b-tree correctly (NULL < numeric < text < blob, BINARY collation only — no other collating sequences at Tier 0) and a minimal `seek` that finds the first entry not less than a target key. `seek` MAY be a linear scan rather than a tree descent — Tier 0 needs enough ordering to walk, not a fully general logarithmic seek — trading efficiency for a simpler, harder-to-get-wrong implementation.

**Implementation:** `src/storage/row/btree/index.rs`

**Tests:** inline `#[cfg(test)]` in `src/storage/row/btree/index.rs`

#### Scenario: Point lookup by key

- GIVEN a multi-page secondary index
- WHEN `seek(target)` is called for an existing key
- THEN the returned entry's key MUST match the pinned oracle's point-lookup result

**Tests:** `src/storage/row/btree/index.rs::secondary_index_seek_matches_oracle`

### Requirement 7: Overflowing Index Keys Use the Index (Not Table) Local-Size Threshold [MUST]

The originating issue's corpus section expected an index fixture with an overflowing key; none existed (`sqlite-rs:tests/corpus/fixtures/btrees/index.db`'s content — `b TEXT`, max length 15 bytes — never exercised it), flagged as a real coverage gap rather than a silent oversight. Building `overflow_index_key.db` (an ~8000-byte indexed TEXT key against a 4096-byte page) surfaced that the gap was hiding an actual bug, not just missing coverage: `local_payload_size`'s `max_local` was computed as `usable_size - 35` unconditionally, but SQLite defines a smaller `max_local` for index cells (leaf AND interior) — `(usable_size - 12) * 64 / 255 - 23` — than for table leaf cells. Every index cell whose payload fell between the two thresholds was read with a `local_size` far larger than what SQLite actually reserved on the page, corrupting the read (`PayloadTooShort`) the moment a fixture forced a payload past the *correct* (smaller) index threshold while still under the incorrect (larger) table one. The system MUST select `max_local` by cell kind, not just by whether the payload overflows.

**Implementation:** `src/storage/row/btree.rs::local_payload_size` (takes an `is_index` flag); `sqlite-rs:tools/gen_fixtures.sh` (`overflow_index_key.db`)

#### Scenario: Overflowing index key

- GIVEN an index whose key column is large enough to overflow into one or more overflow pages under the index (not table) local-size threshold
- WHEN the cursor reads that entry
- THEN the reassembled key payload MUST be byte-identical to the pinned oracle's value

**Tests:** `sqlite-rs:tests/corpus/btree_test.rs::overflowing_index_key_reassembles_byte_identical_to_oracle`

### Requirement 8: Leaf Cell Insert Without Split [MUST]

The system MUST insert a `(rowid, payload)` row into a table b-tree leaf page that has room for it: encoding the cell (payload-length varint + rowid varint + local payload bytes, plus a 4-byte overflow-page pointer per Requirement 2 when the payload doesn't fit locally), locating the rowid-ordered insertion position, and rewriting the page's cell-pointer array and cell count so the leaf's cells remain in strict ascending rowid order. Inserting a rowid that already exists in the leaf MUST return `Err`, never silently overwrite or duplicate.

**Implementation:** `src/storage/row/btree/table/table_insert.rs::insert_into_leaf`, `src/storage/row/btree/table/table_insert.rs::encode_leaf_cell`

#### Scenario: Single-row insert into an otherwise-empty leaf

- GIVEN a table with no existing rows
- WHEN one row is inserted via `insert_row`
- THEN stock `sqlite3` MUST open the file, pass `PRAGMA integrity_check`, and read back the exact row

**Tests:** `sqlite-rs:tests/corpus/btree_insert_test.rs::insert_single_row_no_split`

#### Scenario: Duplicate rowid is rejected

- GIVEN a leaf that already holds a row with rowid `R`
- WHEN a second insert targets rowid `R`
- THEN `insert_row` MUST return `Err(BtreeError::DuplicateRowid)`, leaving the page unchanged

**Tests:** `src/storage/row/btree/table/table_insert.rs::tests::duplicate_rowid_is_rejected`

### Requirement 9: Leaf Split with Median Propagation [MUST]

When a cell won't fit in its target leaf, the system MUST allocate a new page (via the pager's freelist-aware allocator), distribute the leaf's existing cells plus the new cell roughly in half by count (the original page keeps the lower rowids, the newly allocated page takes the upper half), and propagate the split to the parent interior page by inserting a routing cell (child = original leaf, key = the original leaf's new maximum rowid) immediately before whatever routing entry previously pointed at the original leaf, redirecting that entry to the new page.

**Implementation:** `src/storage/row/btree/table/table_insert.rs::insert_into_leaf`, `src/storage/row/btree/table/table_insert.rs::insert_into_parent`

#### Scenario: Insert forces exactly one leaf split

- GIVEN enough rows inserted that a single leaf page's free space is exhausted, but not enough to grow past one interior level
- WHEN the split runs
- THEN stock `sqlite3` MUST open the file, pass `PRAGMA integrity_check`, and read back every row, in order, unchanged

**Tests:** `sqlite-rs:tests/corpus/btree_insert_test.rs::insert_forces_a_leaf_split`

### Requirement 10: Cascading Interior Splits [MUST]

When an interior page's routing-cell insert (propagated up from a child split, per Requirement 9) doesn't fit, the system MUST split the interior page itself: promoting the median key to the grandparent without duplicating it in either child, with the left interior page's rightmost pointer becoming the promoted entry's former child pointer. This MUST recurse arbitrarily many levels up the ancestor chain produced by the leaf-to-root path search.

**Implementation:** `src/storage/row/btree/table/table_insert.rs::insert_into_parent`

#### Scenario: Insert forces multiple cascading interior splits

- GIVEN enough rows inserted to overflow more than one interior page's worth of routing cells
- WHEN the cascade runs
- THEN stock `sqlite3` MUST open the file, pass `PRAGMA integrity_check`, and read back every row, in order, unchanged

**Tests:** `sqlite-rs:tests/corpus/btree_insert_test.rs::insert_forces_cascading_splits_and_a_root_split`

### Requirement 11: Root Split [MUST]

Because a table's root page number is fixed (referenced by its `sqlite_master` entry) and can never be relocated, the system MUST handle a split that reaches the root by relocating the root's current content (leaf or interior, verbatim) to a newly allocated page, then reinitializing the root page in place as a fresh interior page holding a single routing cell (child = the relocated page, key = the promoted divider) and the split's new sibling as the rightmost pointer. This MUST work whether the root is page 1 (the 100-byte file-header offset per the page-1 trap) or any other page number.

**Implementation:** `src/storage/row/btree/table/table_insert.rs::root_split`

#### Scenario: Insert forces a root split

- GIVEN enough rows inserted that even the root page overflows
- WHEN the root split runs
- THEN stock `sqlite3` MUST open the file, pass `PRAGMA integrity_check`, and read back every row, including the first and last rowids inserted, unchanged

**Tests:** `sqlite-rs:tests/corpus/btree_insert_test.rs::insert_forces_cascading_splits_and_a_root_split`

#### Scenario: Root split on page 1 preserves the 100-byte file header

- GIVEN a table b-tree rooted at page 1 (`sqlite_master` itself), the one root that physically shares its page with the 100-byte file header
- WHEN enough rows are inserted that page 1 itself splits into an interior root
- THEN stock `sqlite3` MUST still open the file, pass `PRAGMA integrity_check`, and every row present before the split (including the file header's own fields, proven by the file still opening at all) MUST survive unchanged

**Tests:** `sqlite-rs:tests/corpus/btree_insert_test.rs::insert_into_page_one_root_preserves_the_file_header_across_a_split`

#### Scenario: Bulk insert stays oracle-identical at scale

- GIVEN 1000 rows inserted one at a time (spanning multiple leaf and interior splits)
- WHEN the file is reopened by stock `sqlite3`
- THEN `PRAGMA integrity_check` MUST pass and every row MUST read back identically, in rowid order

**Tests:** `sqlite-rs:tests/corpus/btree_insert_test.rs::bulk_insert_1000_rows_is_oracle_identical`

#### Scenario: Overflow payload combined with a split

- GIVEN rows whose payload is large enough to require an overflow chain (Requirement 2), inserted in enough quantity to also force a leaf split
- WHEN the file is reopened by stock `sqlite3`
- THEN `PRAGMA integrity_check` MUST pass and every row's overflowing payload MUST read back byte-identical

**Tests:** `sqlite-rs:tests/corpus/btree_insert_test.rs::insert_with_overflow_payload_combined_with_a_split`

### Requirement 12: Leaf Cell Delete [MUST]

The system MUST delete the row with a given rowid from a table b-tree leaf page: locating the cell by rowid, removing it from the cell-pointer array, and rewriting the page's remaining cells (in order) and cell count. Deleting a rowid that doesn't exist in the tree MUST return `Err`, leaving the tree unchanged.

**Implementation:** `src/storage/row/btree/table/table_delete.rs::delete_row`

#### Scenario: Deleting a missing rowid errors without mutating the tree

- GIVEN a table b-tree that does not contain rowid `R`
- WHEN `delete_row` is called with rowid `R`
- THEN it MUST return `Err(BtreeError::RowidNotFound)`, leaving the page unchanged

**Tests:** `src/storage/row/btree/table/table_delete.rs::tests::deleting_a_missing_rowid_errors`

#### Scenario: Deleting one row out of several leaves the rest intact

- GIVEN a leaf holding more than one row
- WHEN one row's rowid is deleted
- THEN the remaining rows MUST stay in ascending rowid order, unchanged

**Tests:** `src/storage/row/btree/table/table_delete.rs::tests::deleting_one_of_two_rows_keeps_the_other`, `sqlite-rs:tests/corpus/btree_delete_test.rs::delete_single_row_from_a_two_row_leaf`

#### Scenario: Bulk delete stays oracle-identical at scale

- GIVEN 1000 rows inserted, then every other rowid deleted one at a time
- WHEN the file is reopened by stock `sqlite3`
- THEN `PRAGMA integrity_check` MUST pass and every surviving row MUST read back identically, in rowid order

**Tests:** `sqlite-rs:tests/corpus/btree_delete_test.rs::bulk_delete_every_other_row_out_of_1000`

#### Scenario: Deleting a row with an overflow payload frees its overflow chain

- GIVEN a row whose payload spilled into one or more overflow pages (Requirement 2)
- WHEN that row is deleted
- THEN every page in its overflow chain MUST be deallocated via the pager's freelist (Requirement per spec 003/freelist), and a later insert MUST reuse that freed space rather than growing the file

**Tests:** `sqlite-rs:tests/corpus/btree_delete_test.rs::deleting_a_row_with_an_overflow_payload_frees_its_overflow_pages`

### Requirement 13: Page Merge/Collapse on Underflow [MUST]

When a delete leaves a non-root page with zero cells, the system MUST remove that page's routing entry from its parent (redirecting the parent's `rightmost` pointer if the emptied page was the parent's rightmost child) and deallocate the emptied page via the pager's freelist (Requirement per spec 003/freelist). This is a documented simplification of SQLite's proactive half-full-threshold sibling redistribution: pages are only collapsed once completely empty, not proactively rebalanced while still holding rows — sufficient for structural validity (`PRAGMA integrity_check`) and for freed pages to be reused by a later insert, without porting the exact 3-sibling balance algorithm.

**Implementation:** `src/storage/row/btree/table/table_delete.rs::collapse_into_ancestors`

#### Scenario: Deleting the only row in the root leaves an empty root leaf

- GIVEN a table with exactly one row, at a root page that is itself a leaf
- WHEN that row is deleted
- THEN the root page MUST remain a valid, empty leaf page (the root can never be deallocated)

**Tests:** `src/storage/row/btree/table/table_delete.rs::tests::deleting_the_only_row_leaves_an_empty_root_leaf`

#### Scenario: Page merge triggers correctly when a non-root leaf empties

- GIVEN a table b-tree with more than one leaf page (a prior split), all rows in one leaf deleted
- WHEN the last row in that leaf is deleted
- THEN the leaf MUST be removed from its parent's routing entries and deallocated, and stock `sqlite3` MUST still pass `PRAGMA integrity_check`

**Tests:** `sqlite-rs:tests/corpus/btree_delete_test.rs::delete_triggers_page_collapse_across_a_split_boundary`

### Requirement 14: Underflow Cascading to Root [MUST]

When collapsing a page leaves its own parent with zero routing entries (just a `rightmost` pointer), the system MUST cascade the collapse up the ancestor chain. If the cascade reaches the root itself, the system MUST relocate the sole remaining child's content (leaf or interior, verbatim) into the fixed root page in place, then deallocate the now-vacated child page — mirroring `insert.rs::root_split` in reverse.

**Implementation:** `src/storage/row/btree/table/table_delete.rs::collapse_into_ancestors`, `src/storage/row/btree/table/table_delete.rs::collapse_root`

#### Scenario: Delete-all cascades every level back to a single empty leaf root

- GIVEN enough rows inserted to force at least one leaf split, then every row deleted
- WHEN the last row is deleted
- THEN the tree MUST cascade-collapse back to a single empty leaf at the root page, and stock `sqlite3` MUST report zero rows and pass `PRAGMA integrity_check`

**Tests:** `sqlite-rs:tests/corpus/btree_delete_test.rs::delete_all_rows_leaves_an_empty_table`

#### Scenario: Round-trip insert → delete → insert reuses freed pages

- GIVEN rows inserted (forcing a split), then all deleted, then the same rows reinserted
- WHEN the reinsert completes
- THEN the file's page count MUST NOT grow past what the first insert pass produced (freed pages are reused via the freelist), and stock `sqlite3` MUST pass `PRAGMA integrity_check`

**Tests:** `sqlite-rs:tests/corpus/btree_delete_test.rs::round_trip_insert_delete_insert_reuses_freed_pages`

#### Scenario: Draining one subtree never orphans a sibling's surviving `rightmost` subtree

- GIVEN an interior page that drains down to zero routing entries while its own `rightmost` pointer still leads to a subtree holding real, live rows
- WHEN the cascade collapses that interior page away
- THEN `rightmost`'s subtree MUST be spliced into the interior page's own parent (replacing whichever reference pointed at the collapsing page), never dropped — every surviving row MUST remain reachable from the root

**Tests:** `src/storage/row/btree/table/table_delete.rs::tests::deleting_one_subtree_never_orphans_a_sibling_rightmost_subtree`

### Requirement 15: Index Leaf Cell Insert and Split [MUST]

The system MUST insert an entry (a full record — indexed columns plus the referenced rowid for an ordinary secondary index, or the whole row for a WITHOUT ROWID table) into an index b-tree leaf page (`src/storage/row/btree/index.rs`'s `LEAF_INDEX`/`INTERIOR_INDEX` page types), keyed by [`compare_keys`](BINARY-collation, per Requirement 6) rather than numeric rowid order. Unlike a table leaf split (Requirement 9), which copies the divider and keeps it in the leaf, an index leaf split MUST promote its median entry into the parent, removing it from both halves — because index interior cells carry a full entry, not just a routing key (Requirement 5). Inserting an entry whose key compares exactly equal to an existing one — whether that existing entry lives in a leaf or has been promoted to interior level — MUST return `Err(BtreeError::DuplicateKey)`.

**Implementation:** `src/storage/row/btree/index/index_insert.rs::insert_entry`, `src/storage/row/btree/index/index_insert.rs::insert_into_index_leaf`

#### Scenario: Duplicate key is rejected even when the existing entry lives at interior level

- GIVEN an index b-tree where a prior split promoted some entry's key to interior level
- WHEN a second insert targets that same key
- THEN `insert_entry` MUST return `Err(BtreeError::DuplicateKey)`, leaving the tree unchanged

**Tests:** `src/storage/row/btree/index/index_insert.rs::tests::duplicate_key_is_rejected`

#### Scenario: Bulk insert forces index splits and reads back in BINARY order

- GIVEN 500 secondary-index entries inserted one at a time (forcing leaf and cascading interior splits)
- WHEN the file is reopened by stock `sqlite3`
- THEN `PRAGMA integrity_check` MUST pass and every entry MUST be reachable in ascending BINARY-collation order via both the oracle and this crate's own `IndexCursor`

**Tests:** `sqlite-rs:tests/corpus/btree_index_insert_delete_test.rs::bulk_insert_forces_index_splits_and_reads_back_in_order`

#### Scenario: WITHOUT ROWID table insert shares the same code path

- GIVEN a WITHOUT ROWID table (stored as an index b-tree clustered on its declared PRIMARY KEY, per Requirement 5)
- WHEN rows are inserted via `insert_entry`
- THEN stock `sqlite3` MUST open the file, pass `PRAGMA integrity_check`, and read back every row in primary-key order

**Tests:** `sqlite-rs:tests/corpus/btree_index_insert_delete_test.rs::without_rowid_table_insert_and_delete_round_trip`

### Requirement 16: Index Entry Delete [MUST]

The system MUST delete the entry with a given key from an index b-tree. Deleting a key that doesn't exist MUST return `Err(BtreeError::KeyNotFound)`, leaving the tree unchanged. An emptied leaf (or an interior page that drains to zero of its own entries) is left in place rather than deallocated — a documented simplification mirroring Requirement 13's for table b-trees, adapted for the fact that an index interior entry's own value must never be discarded merely because its child subtree emptied (see Requirement 17).

**Implementation:** `src/storage/row/btree/index/index_delete.rs::delete_entry`, `src/storage/row/btree/index/index_delete.rs::delete_from_leaf`

#### Scenario: Deleting a missing key errors without mutating the tree

- GIVEN an index b-tree that does not contain the given key
- WHEN `delete_entry` is called with that key
- THEN it MUST return `Err(BtreeError::KeyNotFound)`, leaving the tree unchanged

**Tests:** `src/storage/row/btree/index/index_delete.rs::tests::deleting_a_missing_key_errors`

#### Scenario: Delete-all leaves the index empty

- GIVEN 200 secondary-index entries inserted (forcing splits), then every entry deleted in ascending order
- WHEN the last entry is deleted
- THEN stock `sqlite3` MUST report zero rows in the underlying table and pass `PRAGMA integrity_check`, and this crate's own `IndexCursor` MUST also report zero entries

**Tests:** `sqlite-rs:tests/corpus/btree_index_insert_delete_test.rs::delete_all_entries_leaves_an_empty_index`

#### Scenario: Deleting an entry with an overflowing value frees its overflow chain

- GIVEN an index entry whose key/value overflows into one or more overflow pages (per Requirement 7's corrected threshold)
- WHEN that entry is deleted, whether directly from a leaf or removed outright by the interior-match path (Requirement 17)
- THEN every page in its overflow chain MUST be returned to the freelist, not leaked — found via Requirement 7's fixture work, since index cells overflow far more readily under the corrected (smaller) threshold than the previous bug allowed

**Tests:** `src/storage/row/btree/index/index_delete.rs::tests::deleting_an_entry_with_overflow_frees_its_overflow_chain`, `src/storage/row/btree/index/index_delete.rs::tests::deleting_all_entries_orphans_no_page`

### Requirement 17: Interior-Match Deletion via Predecessor Swap [MUST]

Because index interior cells carry a full entry (Requirement 5), deleting a key that was promoted to interior level by an earlier split MUST NOT simply remove that routing entry — its child pointer is load-bearing, and removing the entry would also discard whichever value it carries. The system MUST instead find that entry's in-order predecessor (the maximum entry within its own left-child subtree, found by recursively descending — preferring the rightmost subtree, falling back to an interior page's own last entry once its rightmost subtree is confirmed drained) and swap the predecessor's value into the matched entry's position, physically removing the predecessor from wherever it actually lived. If the matched entry's subtree is entirely drained (no predecessor available), the entry is removed outright instead.

**Implementation:** `src/storage/row/btree/index/index_delete.rs::delete_via_predecessor_swap`, `src/storage/row/btree/index/index_delete.rs::extract_max_entry`

#### Scenario: Deleting an entry promoted to interior level swaps in its predecessor

- GIVEN an index b-tree where a split promoted some entry's key to interior level, with a live predecessor entry in its left-child subtree
- WHEN that interior-level key is deleted
- THEN the interior entry's value MUST be replaced by its predecessor's, the predecessor MUST be physically removed from its leaf, and the tree MUST remain oracle-valid

**Tests:** `src/storage/row/btree/index/index_delete.rs::tests::minimal_two_entry_split_then_delete_promoted_key`

#### Scenario: Deleting every entry, including ones promoted to interior level, in ascending order

- GIVEN 30 entries small enough (relative to the page size) to force splits after just 2 entries, then every entry deleted in ascending order
- WHEN each delete runs — some hitting a leaf directly, others hitting an interior-level promoted entry
- THEN every delete MUST succeed (no `KeyNotFound` false negative from an incomplete predecessor search) and the tree MUST end fully empty

**Tests:** `src/storage/row/btree/index/index_delete.rs::tests::split_then_delete_all_including_promoted_interior_entries`

### Requirement 18: Structural Integrity Verification [MUST]

The system MUST be able to walk every table and index b-tree plus the freelist trunk chain and report structural problems in the same textual shape stock `sqlite3` uses: a single `"ok"` row when nothing is wrong, otherwise one row per problem found. `PRAGMA quick_check` MUST skip the exhaustive index-vs-table cross-check pass that `PRAGMA integrity_check` performs (every index entry's trailing rowid exists in its table, and per-index entry counts match table row counts) — both share the same tree-walking core. An auto-vacuum database (`largest_root_btree_page != 0`) MUST report a single informational problem rather than a silent false negative: this crate has no auto-vacuum/incremental-vacuum write path and therefore never writes a pointer-map, so pointer-map cross-validation is out of scope until auto-vacuum support lands, not silently skipped.

**Implementation:** `src/storage/row/integrity.rs::run_integrity_check`, `src/vm/row/vm.rs::integrity_check`, `src/codegen/row/pragma.rs::compile_pragma`, `src/parser/ast.rs::Pragma::IntegrityCheck`

#### Scenario: A well-formed database with tables and indexes passes both pragmas

- GIVEN a database with multiple tables, secondary indexes, and rows inserted through this crate's own write path
- WHEN `PRAGMA integrity_check` or `PRAGMA quick_check` runs
- THEN both MUST report a single `"ok"` row

**Tests:** `sqlite-rs:tests/unit/vdbe_integrity_check_test.rs::integrity_check_on_a_well_formed_database_reports_ok`, `sqlite-rs:tests/unit/vdbe_integrity_check_test.rs::integrity_check_covers_multiple_tables_and_indexes`

#### Scenario: An empty database passes

- GIVEN a freshly created database with no user tables
- WHEN `PRAGMA integrity_check` runs
- THEN it MUST report a single `"ok"` row rather than erroring on an empty `sqlite_master`

**Tests:** `sqlite-rs:tests/unit/vdbe_integrity_check_test.rs::integrity_check_on_an_empty_database_reports_ok`

#### Scenario: quick_check skips the index-vs-table cross-check

- GIVEN `PRAGMA integrity_check` and `PRAGMA quick_check` compiled from the same `Pragma::IntegrityCheck` AST node
- WHEN each compiles
- THEN `quick_check` MUST compile `Opcode::IntegrityCheck` with `P1 = 1` and `integrity_check` with `P1 = 0`, the flag `run_integrity_check` uses to skip the index cross-check pass

**Tests:** `src/codegen/row/pragma.rs::tests::integrity_check_compiles_p1_zero`, `src/codegen/row/pragma.rs::tests::quick_check_compiles_p1_one`
