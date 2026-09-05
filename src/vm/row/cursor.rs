//! The storage-agnostic cursor trait ADR 0008 calls for: `vm::row`
//! defines this boundary itself and an adapter crate (e.g. `db-storage`)
//! implements it over a real b-tree, rather than `vm::row` depending on
//! `db-storage` directly (ADR 0006's storage-agnostic stance for
//! `db-core`). [`InMemoryCursor`] is the mock implementation this
//! phase's tests exercise the dispatch loop against; real
//! `db-storage::row::btree::TableCursor` wiring is a follow-up (the
//! largest remaining sqlite-rs VDBE file, `cursor.rs`, 4,498 lines).

use std::rc::Rc;

use super::program::SortKeyColumn;
use super::record::{decode_column, decode_record};
use super::value::{TextEncoding, Value};

/// A forward-scanning, row-at-a-time cursor over a table's rows.
/// Mirrors the read-only subset of sqlite-rs's `TableCursor` interface
/// that the `Rewind`/`Next`/`Column`/`Rowid` opcodes need.
pub trait Cursor {
    /// Positions the cursor at the first row. Returns `true` if a row
    /// exists (matches sqlite-rs's `Rewind`: "jump to p2 if empty", so
    /// `false` here is the jump condition).
    fn rewind(&mut self) -> bool;

    /// Advances to the next row. Returns `true` if there was one.
    fn next(&mut self) -> bool;

    /// Positions the cursor at the last row. Returns `true` if a row
    /// exists (matches sqlite-rs's `Last`). Default `false` so cursor
    /// kinds that don't support backward positioning yet don't each
    /// have to repeat the same stub.
    fn last(&mut self) -> bool {
        false
    }

    /// Moves to the previous row. Returns `true` if there was one.
    /// Default `false`, same rationale as [`Cursor::last`].
    fn prev(&mut self) -> bool {
        false
    }

    /// Reads column `col` of the current row. Panics if the cursor has
    /// no current row (`rewind`/`next` last returned `false`) -- callers
    /// (the dispatch loop) never call this except right after a
    /// successful `rewind`/`next`, matching sqlite-rs's own invariant
    /// that `Column` never runs against an empty cursor.
    fn column(&self, col: usize) -> Value;

    /// The current row's rowid.
    fn rowid(&self) -> i64;

    /// Inserts a decoded row under `rowid`. Returns `false` if this
    /// cursor kind doesn't support insertion (e.g. [`InMemoryCursor`],
    /// a read-only fixture) -- `Opcode::Insert`'s dispatch turns that
    /// into an error. Default no-op so read-only cursor kinds don't
    /// each have to repeat the same stub.
    fn insert(&mut self, _rowid: i64, _values: Vec<Value>) -> bool {
        false
    }

    /// Buffers a record-encoded row for later sorting (`Opcode::
    /// SorterInsert`). Returns `false` if this cursor kind isn't a
    /// sorter. Default no-op, matching [`Cursor::insert`]'s convention.
    fn sorter_insert(&mut self, _blob: Rc<[u8]>) -> bool {
        false
    }

    /// The current row's raw record bytes (`Opcode::SorterData`) --
    /// `None` if this cursor kind isn't a sorter, or the sorter has no
    /// current row (unsorted, empty, or exhausted).
    fn current_blob(&self) -> Option<Value> {
        None
    }

    /// Deletes the row at the current position. Returns `false` if this
    /// cursor kind doesn't support deletion. Default no-op, same
    /// rationale as [`Cursor::insert`].
    fn delete(&mut self) -> bool {
        false
    }

    /// The rowid `Opcode::NewRowid` should hand out next for this
    /// cursor's table -- one past the largest rowid currently stored.
    /// Default `1`, matching sqlite-rs's behaviour for a table with no
    /// rows yet; cursor kinds that don't track insertion (e.g.
    /// read-only fixtures) never have `Opcode::NewRowid` run against
    /// them in practice.
    fn next_rowid(&self) -> i64 {
        1
    }

    /// Inserts an index entry (`db-core#96`'s secondary-index
    /// maintenance): `key` is the indexed column values followed by the
    /// row's rowid, matching `IdxInsert`'s register-run convention.
    /// Returns `false` if this cursor kind isn't an index cursor.
    /// Storage-agnostic stand-in for a real b-tree index -- entries are
    /// looked up by exact equality ([`Cursor::idx_delete`]), not ordered
    /// range scan (that's `#94`'s index-scan codegen, a separate
    /// ticket).
    fn idx_insert(&mut self, _key: Vec<Value>) -> bool {
        false
    }

    /// Deletes the index entry equal to `key` (see [`Cursor::idx_insert`]).
    /// Returns `false` if no such entry exists, or this cursor kind
    /// isn't an index cursor.
    fn idx_delete(&mut self, _key: &[Value]) -> bool {
        false
    }
}

/// An in-memory table: a fixed set of rows, each a fixed-width `Vec<
/// Value>`, addressed by position (`rowid` is the 1-based row index).
/// The mock [`Cursor`] implementation this phase's tests scan against.
pub struct InMemoryCursor {
    rows: Vec<Vec<Value>>,
    pos: Option<usize>,
}

impl InMemoryCursor {
    pub fn new(rows: Vec<Vec<Value>>) -> Self {
        InMemoryCursor { rows, pos: None }
    }
}

impl Cursor for InMemoryCursor {
    fn rewind(&mut self) -> bool {
        self.pos = if self.rows.is_empty() { None } else { Some(0) };
        self.pos.is_some()
    }

    fn next(&mut self) -> bool {
        let next = self.pos.map_or(0, |p| p.saturating_add(1));
        if next < self.rows.len() {
            self.pos = Some(next);
            true
        } else {
            self.pos = None;
            false
        }
    }

    fn last(&mut self) -> bool {
        self.pos = self.rows.len().checked_sub(1);
        self.pos.is_some()
    }

    fn prev(&mut self) -> bool {
        match self.pos {
            Some(0) | None => {
                self.pos = None;
                false
            }
            Some(p) => {
                self.pos = Some(p - 1);
                true
            }
        }
    }

    fn column(&self, col: usize) -> Value {
        #[allow(
            clippy::expect_used,
            reason = "Cursor contract: column/rowid are only read after a successful rewind/next"
        )]
        let pos = self.pos.expect("column read with no current row");
        self.rows
            .get(pos)
            .and_then(|row| row.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        #[allow(
            clippy::expect_used,
            reason = "Cursor contract: column/rowid are only read after a successful rewind/next"
        )]
        let pos = self.pos.expect("rowid read with no current row");
        #[allow(clippy::cast_possible_wrap)]
        {
            (pos as i64).saturating_add(1)
        }
    }

    fn delete(&mut self) -> bool {
        let Some(pos) = self.pos else {
            return false;
        };
        if pos >= self.rows.len() {
            return false;
        }
        self.rows.remove(pos);
        self.pos = None;
        true
    }
}

/// An in-memory table materialized by `Opcode::Insert` -- backs
/// `Opcode::OpenEphemeral`'s table-mode cursor (db-core#59), ported
/// from sqlite-rs's `cursor::EphemeralTableState`. Unlike
/// [`InMemoryCursor`]'s test fixture, rows arrive one at a time via
/// [`Cursor::insert`] (`Opcode::Insert`, called after `Opcode::
/// MakeRecord` encodes a row and `vm::row::vm`'s dispatch decodes it
/// straight back -- sqlite-rs's own "decode-once-at-insert" design),
/// each carrying an explicit caller-assigned rowid rather than an
/// implicit position-based one (codegen assigns sequential rowids
/// starting at 1, but nothing here enforces that).
///
/// **Kept sorted by rowid, and positioned by rowid rather than by Vec
/// index** (db-core#96): `DELETE`/`UPDATE` codegen deletes (and, for
/// `UPDATE`, re-inserts) the *current* row mid-scan, exactly like
/// sqlite-rs's own codegen does against a real b-tree cursor -- safe
/// there because a b-tree cursor's traversal is key-based, so a
/// concurrent delete/insert elsewhere in the tree never invalidates it
/// (see `stmt/delete.rs`'s doc comment). An index-based `pos: Option<
/// usize>` would *not* be safe: removing the current element shifts
/// every later index down by one, so `next()` would either skip a row
/// or (worse, since [`Cursor::insert`] appends) revisit an
/// already-updated one. Tracking `current_rowid` instead and searching
/// for "the smallest stored rowid greater than this" on every `next()`
/// reproduces the b-tree cursor's actual invariant with a `Vec`.
#[derive(Default)]
pub struct EphemeralTableCursor {
    /// Sorted ascending by rowid (the first element of each tuple).
    rows: Vec<(i64, Vec<Value>)>,
    /// The rowid last positioned at, kept even after that row is
    /// deleted (a fence-post `next()`/`prev()` resume from) --
    /// [`Self::at_row`] says whether it still names a live row.
    current_rowid: Option<i64>,
    at_row: bool,
}

impl EphemeralTableCursor {
    pub fn new() -> Self {
        EphemeralTableCursor::default()
    }

    fn row_index(&self, rowid: i64) -> Option<usize> {
        self.rows.binary_search_by_key(&rowid, |(r, _)| *r).ok()
    }
}

impl Cursor for EphemeralTableCursor {
    fn rewind(&mut self) -> bool {
        match self.rows.first() {
            Some((rowid, _)) => {
                self.current_rowid = Some(*rowid);
                self.at_row = true;
                true
            }
            None => {
                self.current_rowid = None;
                self.at_row = false;
                false
            }
        }
    }

    fn next(&mut self) -> bool {
        let after = self.current_rowid.unwrap_or(i64::MIN);
        let pos = self.rows.partition_point(|(r, _)| *r <= after);
        match self.rows.get(pos) {
            Some((rowid, _)) => {
                self.current_rowid = Some(*rowid);
                self.at_row = true;
                true
            }
            None => {
                self.at_row = false;
                false
            }
        }
    }

    fn last(&mut self) -> bool {
        match self.rows.last() {
            Some((rowid, _)) => {
                self.current_rowid = Some(*rowid);
                self.at_row = true;
                true
            }
            None => {
                self.current_rowid = None;
                self.at_row = false;
                false
            }
        }
    }

    fn prev(&mut self) -> bool {
        let before = self.current_rowid.unwrap_or(i64::MAX);
        let pos = self.rows.partition_point(|(r, _)| *r < before);
        if pos == 0 {
            self.at_row = false;
            return false;
        }
        self.current_rowid = Some(self.rows[pos - 1].0);
        self.at_row = true;
        true
    }

    fn column(&self, col: usize) -> Value {
        assert!(self.at_row, "column read with no current row");
        #[allow(
            clippy::expect_used,
            reason = "Cursor contract: column/rowid are only read after a successful rewind/next"
        )]
        let rowid = self.current_rowid.expect("column read with no current row");
        #[allow(
            clippy::expect_used,
            reason = "at_row true implies current_rowid still names a live row"
        )]
        let idx = self.row_index(rowid).expect("current row vanished");
        self.rows[idx].1.get(col).cloned().unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        assert!(self.at_row, "rowid read with no current row");
        #[allow(
            clippy::expect_used,
            reason = "Cursor contract: column/rowid are only read after a successful rewind/next"
        )]
        self.current_rowid.expect("rowid read with no current row")
    }

    fn insert(&mut self, rowid: i64, values: Vec<Value>) -> bool {
        let pos = self.rows.partition_point(|(r, _)| *r < rowid);
        if self.rows.get(pos).is_some_and(|(r, _)| *r == rowid) {
            self.rows[pos].1 = values;
        } else {
            self.rows.insert(pos, (rowid, values));
        }
        true
    }

    fn delete(&mut self) -> bool {
        if !self.at_row {
            return false;
        }
        #[allow(
            clippy::expect_used,
            reason = "at_row true implies current_rowid still names a live row"
        )]
        let rowid = self.current_rowid.expect("at_row implies current_rowid");
        let Some(idx) = self.row_index(rowid) else {
            return false;
        };
        self.rows.remove(idx);
        self.at_row = false;
        true
    }

    fn next_rowid(&self) -> i64 {
        self.rows
            .last()
            .map_or(0, |(rowid, _)| *rowid)
            .saturating_add(1)
    }

    /// Doubles as an index cursor for `db-core#96`'s secondary-index
    /// maintenance: `key` (indexed columns + rowid) is appended as an
    /// ordinary row under a placeholder rowid of `0`, bypassing the
    /// rowid-sort/positioning machinery above entirely -- index
    /// cursors are never scanned (`rewind`/`next`) in this ticket's
    /// scope (that's `#94`'s index-scan codegen), only inserted into
    /// and deleted from by key equality.
    fn idx_insert(&mut self, key: Vec<Value>) -> bool {
        self.rows.push((0, key));
        true
    }

    fn idx_delete(&mut self, key: &[Value]) -> bool {
        let Some(pos) = self.rows.iter().position(|(_, values)| values == key) else {
            return false;
        };
        self.rows.remove(pos);
        true
    }
}

/// `ORDER BY` buffering and sort, backing `Opcode::SorterOpen`/
/// `Insert`/`Sort`/`Next`/`Data` (db-core#69, extended to multi-key and
/// an optional top-K bound by db-core#87).
///
/// Rows buffer as raw record bytes (`SorterData` hands them back
/// unchanged) paired with their already-decoded sort-key values (so
/// `SorterSort`'s comparisons never re-decode); each key column is
/// decoded once at `sorter_insert` time via [`decode_column`], not the
/// whole row, matching sqlite-rs's "decode only what comparisons need"
/// design (its own `#507`/`#631`).
///
/// `bound`, when set (`Opcode::SorterOpen`'s `P5`/`P2`), caps the
/// buffer at that many rows: once over it, the buffer is re-sorted and
/// truncated to the bound, keeping only the best-so-far rows. This is
/// a correctness-equivalent, simpler stand-in for sqlite-rs's
/// heap-ordered O(log bound) eviction (`sorter.rs`'s `SorterState`) --
/// still never buffers (past the next truncation) more than a small
/// multiple of `bound` rows, just without that file's tighter
/// per-insert bound.
pub struct SorterCursor {
    keys: Vec<SortKeyColumn>,
    buffer: Vec<(Rc<[u8]>, Vec<Value>)>,
    sorted: bool,
    pos: Option<usize>,
    bound: Option<usize>,
}

impl SorterCursor {
    pub fn new(keys: Vec<SortKeyColumn>, bound: Option<usize>) -> Self {
        SorterCursor {
            keys,
            buffer: Vec::new(),
            sorted: false,
            pos: None,
            bound,
        }
    }

    fn decode_keys(&self, blob: &[u8]) -> Vec<Value> {
        self.keys
            .iter()
            .map(|k| decode_column(blob, k.index, TextEncoding::Utf8).unwrap_or(Value::Null))
            .collect()
    }
}

/// The sort order two already-decoded key values fall in, per `key`'s
/// direction/collation/NULLS placement.
fn compare_key(a: &Value, b: &Value, key: &SortKeyColumn) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if key.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if key.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        _ => {
            let ord = super::compare::compare(a, b, key.collation);
            if key.descending {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// Multi-key comparison: the first non-equal key column decides the
/// order, matching `ORDER BY col1, col2, ...`'s left-to-right tie-break.
fn compare_keys(a: &[Value], b: &[Value], keys: &[SortKeyColumn]) -> std::cmp::Ordering {
    for (i, key) in keys.iter().enumerate() {
        let ord = compare_key(&a[i], &b[i], key);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

impl Cursor for SorterCursor {
    /// Sorts the buffer (if not already sorted since the last insert)
    /// and positions at the first row -- `SorterSort`/`Sort`'s dispatch
    /// target.
    fn rewind(&mut self) -> bool {
        if !self.sorted {
            let keys = self.keys.clone();
            self.buffer
                .sort_by(|(_, a), (_, b)| compare_keys(a, b, &keys));
            self.sorted = true;
        }
        self.pos = if self.buffer.is_empty() {
            None
        } else {
            Some(0)
        };
        self.pos.is_some()
    }

    fn next(&mut self) -> bool {
        let next = self.pos.map_or(0, |p| p.saturating_add(1));
        if next < self.buffer.len() {
            self.pos = Some(next);
            true
        } else {
            self.pos = None;
            false
        }
    }

    fn column(&self, col: usize) -> Value {
        #[allow(
            clippy::expect_used,
            reason = "Cursor contract: column/rowid are only read after a successful rewind/next"
        )]
        let pos = self.pos.expect("column read with no current row");
        self.buffer
            .get(pos)
            .and_then(|(blob, _)| decode_record(blob, TextEncoding::Utf8).ok())
            .and_then(|values| values.get(col).cloned())
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        0 // sorters have no rowid concept; sqlite-rs never calls Rowid on one either
    }

    fn sorter_insert(&mut self, blob: Rc<[u8]>) -> bool {
        let key_values = self.decode_keys(&blob);
        self.buffer.push((blob, key_values));
        self.sorted = false;
        if let Some(bound) = self.bound {
            if self.buffer.len() > bound {
                let keys = self.keys.clone();
                self.buffer
                    .sort_by(|(_, a), (_, b)| compare_keys(a, b, &keys));
                self.buffer.truncate(bound);
                self.sorted = true;
            }
        }
        true
    }

    fn current_blob(&self) -> Option<Value> {
        let pos = self.pos?;
        self.buffer
            .get(pos)
            .map(|(blob, _)| Value::Blob(blob.clone()))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    #[test]
    fn empty_cursor_rewind_reports_no_row() {
        let mut c = InMemoryCursor::new(vec![]);
        assert!(!c.rewind());
    }

    #[test]
    fn rewind_then_next_walks_every_row_in_order() {
        let mut c = InMemoryCursor::new(vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]);
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Integer(1));
        assert_eq!(c.rowid(), 1);
        assert!(c.next());
        assert_eq!(c.column(0), Value::Integer(2));
        assert!(c.next());
        assert_eq!(c.column(0), Value::Integer(3));
        assert!(!c.next());
    }

    #[test]
    fn missing_column_reads_as_null() {
        let mut c = InMemoryCursor::new(vec![vec![Value::Integer(1)]]);
        c.rewind();
        assert_eq!(c.column(5), Value::Null);
    }

    #[test]
    fn in_memory_cursor_does_not_support_insert() {
        let mut c = InMemoryCursor::new(vec![]);
        assert!(!c.insert(1, vec![Value::Integer(1)]));
    }

    #[test]
    fn last_then_prev_walks_every_row_in_reverse() {
        let mut c = InMemoryCursor::new(vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]);
        assert!(c.last());
        assert_eq!(c.column(0), Value::Integer(3));
        assert!(c.prev());
        assert_eq!(c.column(0), Value::Integer(2));
        assert!(c.prev());
        assert_eq!(c.column(0), Value::Integer(1));
        assert!(!c.prev());
    }

    #[test]
    fn empty_cursor_last_reports_no_row() {
        let mut c = InMemoryCursor::new(vec![]);
        assert!(!c.last());
    }

    #[test]
    fn delete_removes_current_row_and_clears_position() {
        let mut c = InMemoryCursor::new(vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
        c.rewind();
        assert!(c.delete());
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Integer(2));
    }

    #[test]
    fn delete_with_no_current_row_returns_false() {
        let mut c = InMemoryCursor::new(vec![vec![Value::Integer(1)]]);
        assert!(!c.delete());
    }

    #[test]
    fn ephemeral_table_cursor_starts_empty() {
        let mut c = EphemeralTableCursor::new();
        assert!(!c.rewind());
    }

    #[test]
    fn ephemeral_table_cursor_scans_inserted_rows_with_explicit_rowids() {
        let mut c = EphemeralTableCursor::new();
        assert!(c.insert(10, vec![Value::Integer(1)]));
        assert!(c.insert(20, vec![Value::Integer(2)]));
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Integer(1));
        assert_eq!(c.rowid(), 10);
        assert!(c.next());
        assert_eq!(c.column(0), Value::Integer(2));
        assert_eq!(c.rowid(), 20);
        assert!(!c.next());
    }

    fn ascending_key(index: usize) -> SortKeyColumn {
        SortKeyColumn {
            index,
            descending: false,
            collation: super::super::value::Collation::Binary,
            nulls_first: false,
        }
    }

    #[test]
    fn sorter_cursor_sorts_buffered_rows_on_rewind() {
        let mut c = SorterCursor::new(vec![ascending_key(0)], None);
        for v in [30i64, 10, 20] {
            let blob =
                super::super::record::encode_record(&[Value::Integer(v)], TextEncoding::Utf8);
            assert!(c.sorter_insert(blob.into()));
        }
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Integer(10));
        assert!(c.next());
        assert_eq!(c.column(0), Value::Integer(20));
        assert!(c.next());
        assert_eq!(c.column(0), Value::Integer(30));
        assert!(!c.next());
    }

    #[test]
    fn sorter_cursor_descending_and_nulls_first() {
        let key = SortKeyColumn {
            index: 0,
            descending: true,
            collation: super::super::value::Collation::Binary,
            nulls_first: true,
        };
        let mut c = SorterCursor::new(vec![key], None);
        for v in [Value::Integer(5), Value::Null, Value::Integer(-7)] {
            let blob = super::super::record::encode_record(&[v], TextEncoding::Utf8);
            c.sorter_insert(blob.into());
        }
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Null);
        assert!(c.next());
        assert_eq!(c.column(0), Value::Integer(5));
        assert!(c.next());
        assert_eq!(c.column(0), Value::Integer(-7));
        assert!(!c.next());
    }

    #[test]
    fn empty_sorter_rewind_reports_no_row() {
        let mut c = SorterCursor::new(vec![ascending_key(0)], None);
        assert!(!c.rewind());
        assert!(c.current_blob().is_none());
    }

    #[test]
    fn sorter_current_blob_returns_the_full_encoded_row() {
        let mut c = SorterCursor::new(vec![ascending_key(0)], None);
        let blob = super::super::record::encode_record(
            &[Value::Integer(1), Value::Text("payload".to_string().into())],
            TextEncoding::Utf8,
        );
        c.sorter_insert(blob.clone().into());
        c.rewind();
        assert_eq!(c.current_blob(), Some(Value::Blob(blob.into())));
    }

    #[test]
    fn sorter_cursor_multi_key_breaks_ties_on_second_column() {
        let mut c = SorterCursor::new(vec![ascending_key(0), ascending_key(1)], None);
        for (a, b) in [(1i64, 20i64), (1, 10), (0, 5)] {
            let blob = super::super::record::encode_record(
                &[Value::Integer(a), Value::Integer(b)],
                TextEncoding::Utf8,
            );
            c.sorter_insert(blob.into());
        }
        assert!(c.rewind());
        assert_eq!(
            (c.column(0), c.column(1)),
            (Value::Integer(0), Value::Integer(5))
        );
        assert!(c.next());
        assert_eq!(
            (c.column(0), c.column(1)),
            (Value::Integer(1), Value::Integer(10))
        );
        assert!(c.next());
        assert_eq!(
            (c.column(0), c.column(1)),
            (Value::Integer(1), Value::Integer(20))
        );
        assert!(!c.next());
    }

    #[test]
    fn sorter_cursor_bound_keeps_only_the_best_rows() {
        let mut c = SorterCursor::new(vec![ascending_key(0)], Some(2));
        for v in [5i64, 1, 4, 2, 3] {
            let blob =
                super::super::record::encode_record(&[Value::Integer(v)], TextEncoding::Utf8);
            c.sorter_insert(blob.into());
        }
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Integer(1));
        assert!(c.next());
        assert_eq!(c.column(0), Value::Integer(2));
        assert!(!c.next());
    }

    #[test]
    fn sorter_cursor_zero_bound_keeps_no_rows() {
        let mut c = SorterCursor::new(vec![ascending_key(0)], Some(0));
        let blob = super::super::record::encode_record(&[Value::Integer(1)], TextEncoding::Utf8);
        c.sorter_insert(blob.into());
        assert!(!c.rewind());
    }

    #[test]
    fn ephemeral_table_cursor_last_then_prev_walks_in_reverse() {
        let mut c = EphemeralTableCursor::new();
        c.insert(10, vec![Value::Integer(1)]);
        c.insert(20, vec![Value::Integer(2)]);
        assert!(c.last());
        assert_eq!(c.rowid(), 20);
        assert!(c.prev());
        assert_eq!(c.rowid(), 10);
        assert!(!c.prev());
    }

    #[test]
    fn ephemeral_table_cursor_delete_removes_current_row() {
        let mut c = EphemeralTableCursor::new();
        c.insert(10, vec![Value::Integer(1)]);
        c.insert(20, vec![Value::Integer(2)]);
        c.rewind();
        assert!(c.delete());
        assert!(c.rewind());
        assert_eq!(c.rowid(), 20);
    }
}
