# ADR 0034: In-place leaf/index cell splice with real freeblock bookkeeping

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0023-leaf-cell-splice.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

Date: 2026-08-22
Status: Accepted

## Context

Every single-row INSERT/UPDATE/DELETE mutation on a table leaf or a
secondary-index leaf went through `collect_leaf_cells`/`collect_index_leaf_cells`
(decode every cell on the page into a `Vec`) followed by
`write_page_common` (zero the whole b-tree portion of the page, then
re-lay every surviving cell back-to-front and rebuild the cell-pointer
array from scratch) — O(cells-per-page) work per row, repeated once per
secondary index via `IdxInsert`/`IdxDelete`. #337 confirmed this via
`sqlite-rs's former tests/performance/crud.rs`'s tier-1 bench: `insert_single`/`update_pk`/
`delete_pk` sat at 7.6-14.2x the pinned oracle even for single-row
primary-key operations.

The module docs for `src/storage/row/btree/table/table_insert.rs` and `delete.rs`
explicitly flagged this as a known simplification: no freeblock chain
existed at all (the first-freeblock header field and the
fragmented-free-bytes counter were always written `0`), and every
mutation paid the full rebuild cost regardless of whether one cell or
all of them changed.

## Decision

**Add real freeblock/fragmentation bookkeeping** (`fileformat2.html`
"Freeblocks": each freed byte range becomes either a freeblock — a
4-byte in-place header, `next-offset` + `size`, chained off the page
header's first-freeblock field, sorted ascending and coalesced with
adjacent neighbors — or, if shorter than 4 bytes, an addition to the
page header's fragmented-free-bytes counter), and use it to make the
**delete** path an honest O(1)-relative-to-other-cells splice:
`splice_delete_cell` (`src/storage/row/btree.rs`) shifts the cell-pointer array left
by one entry (a memmove of 2-byte pointer entries only, never the cell
bytes) and returns the freed range to the freeblock chain, growing
`content_start` instead when the freed range borders it.

For **insert**, a narrower `splice_insert_cell` fast path checks only
whether the *contiguous* gap between the end of the cell-pointer array
and `content_start` is large enough for the new cell; if so, it appends
below `content_start` and shifts the pointer array right by one entry —
also O(1) relative to the other cells. It does **not** attempt to reuse
existing freeblocks. When the gap is insufficient, every call site falls
back to its pre-existing collect-all/rebuild-all path unchanged — which,
being a from-scratch layout, incidentally also defragments the page
(reclaims any freeblock/fragmentation space) and handles the split case.

Both `splice_insert_cell`/`splice_delete_cell` are shared between table
leaves and index leaves (`header_start+8` layout is identical); a
`has_rowid` flag on `splice_delete_cell` selects which cell-head shape
to decode (a table leaf cell carries a payload-length varint *and* a
rowid varint before its payload; an index leaf cell carries only the
payload-length varint — the rowid rides inside the payload record
itself). Getting this wrong on the delete path silently mis-locates a
cell's end and corrupts an unrelated neighboring cell (caught, in this
PR, by `t2_written_file_passes_integrity_check` before it shipped).

## Alternatives rejected

- **Full freeblock-reuse on insert too** (best-fit search + a
  `defragment_page` compaction routine invoked mid-insert when no single
  freeblock fits but the total free space does): rejected for this PR.
  It doubles the surface area of new, correctness-critical page-layout
  code for a case the existing full-rebuild fallback already handles
  correctly (if not maximally cheaply) — the fallback path *is* a
  defragmentation, since `write_page_common` lays out cells fresh with
  zero fragmentation every time. Reusing freeblocks on insert only helps
  workloads that interleave same-size deletes and inserts on the same
  page tightly enough to matter before a split/fallback rebuild would
  have run anyway; revisit as a follow-up if profiling shows it's a real
  gap, not a hypothetical one.
- **Locating the cell-to-delete/insert-position via a page-only binary
  search** (avoiding `collect_leaf_cells`/`collect_index_leaf_cells`
  entirely on the read side): rejected for the index path specifically —
  index key comparison needs the fully decoded, reassembled key (which
  may span overflow pages), not just a rowid, so the existing
  collect-based lookup stays. The table path's rowid lookup could use a
  page-only binary search instead of `collect_leaf_cells`, but was left
  as-is here to keep this PR's diff scoped to the write side (the O(n)
  rebuild) rather than also touching the read/lookup side; a worthwhile
  follow-up, not required to close #337's stated acceptance criteria.
- **Porting SQLite's exact freeblock-reuse allocator** (`allocateSpace`'s
  best-fit-with-fallback-to-first-fit search order): rejected — the
  format only requires freeblocks to be valid and consistent on disk,
  not that a specific implementation ever chooses to occupy them; a
  reader (including stock `sqlite3`'s `PRAGMA integrity_check`) doesn't
  care whether a given freed range became a freeblock that was later
  reused or one that just sat there until the next full rebuild.

## Consequences

- `insert_single`/`update_pk`/`delete_pk` in `sqlite-rs's former tests/performance/crud.rs`
  should show a measurable improvement, most directly for `delete_pk`
  (always O(1) now, no fallback case) and for `update_pk`/`insert_single`
  whenever the leaf page has enough contiguous gap space (the common
  case for an append-mostly or steady-state workload).
- A workload that repeatedly deletes then re-inserts differently-sized
  cells on the same page can still degrade toward the O(n) fallback more
  often than a full freeblock-reuse allocator would, since insert never
  looks at the freeblock chain. This is the accepted cost of the
  narrower scope above.
- `.openspec/specs/002-btree/spec.md`'s byte-layout contract already
  described the freeblock/fragmented-bytes header fields in the abstract
  (per `fileformat2.html`); this PR is the first code that actually
  populates and maintains them, rather than always writing them `0`.
