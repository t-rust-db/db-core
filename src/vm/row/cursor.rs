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
#[derive(Default)]
pub struct EphemeralTableCursor {
    rows: Vec<(i64, Vec<Value>)>,
    pos: Option<usize>,
}

impl EphemeralTableCursor {
    pub fn new() -> Self {
        EphemeralTableCursor::default()
    }
}

impl Cursor for EphemeralTableCursor {
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
            .and_then(|(_, values)| values.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        #[allow(
            clippy::expect_used,
            reason = "Cursor contract: column/rowid are only read after a successful rewind/next"
        )]
        let pos = self.pos.expect("rowid read with no current row");
        self.rows.get(pos).map_or(0, |(rowid, _)| *rowid)
    }

    fn insert(&mut self, rowid: i64, values: Vec<Value>) -> bool {
        self.rows.push((rowid, values));
        true
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

/// `ORDER BY` buffering and sort, backing `Opcode::SorterOpen`/
/// `Insert`/`Sort`/`Next`/`Data` (db-core#69). **Single-key, no
/// LIMIT/bound** -- see this module's own doc and db-core#69's scope
/// note; multi-key sort and bounded top-K maintenance are follow-ups.
///
/// Rows buffer as raw record bytes (`SorterData` hands them back
/// unchanged) paired with their already-decoded sort-key value (so
/// `SorterSort`'s comparisons never re-decode); the key column is
/// decoded once at `sorter_insert` time via [`decode_column`], not the
/// whole row, matching sqlite-rs's "decode only what comparisons need"
/// design (its own `#507`/`#631`).
pub struct SorterCursor {
    key: SortKeyColumn,
    buffer: Vec<(Rc<[u8]>, Value)>,
    sorted: bool,
    pos: Option<usize>,
}

impl SorterCursor {
    pub fn new(key: SortKeyColumn) -> Self {
        SorterCursor {
            key,
            buffer: Vec::new(),
            sorted: false,
            pos: None,
        }
    }
}

/// The sort order two already-decoded key values fall in, per `key`'s
/// direction/collation/NULLS placement.
fn compare_keys(a: &Value, b: &Value, key: &SortKeyColumn) -> std::cmp::Ordering {
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

impl Cursor for SorterCursor {
    /// Sorts the buffer (if not already sorted since the last insert)
    /// and positions at the first row -- `SorterSort`/`Sort`'s dispatch
    /// target.
    fn rewind(&mut self) -> bool {
        if !self.sorted {
            let key = self.key;
            self.buffer
                .sort_by(|(_, a), (_, b)| compare_keys(a, b, &key));
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
        let key_value =
            decode_column(&blob, self.key.index, TextEncoding::Utf8).unwrap_or(Value::Null);
        self.buffer.push((blob, key_value));
        self.sorted = false;
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
        let mut c = SorterCursor::new(ascending_key(0));
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
        let mut c = SorterCursor::new(key);
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
        let mut c = SorterCursor::new(ascending_key(0));
        assert!(!c.rewind());
        assert!(c.current_blob().is_none());
    }

    #[test]
    fn sorter_current_blob_returns_the_full_encoded_row() {
        let mut c = SorterCursor::new(ascending_key(0));
        let blob = super::super::record::encode_record(
            &[Value::Integer(1), Value::Text("payload".to_string().into())],
            TextEncoding::Utf8,
        );
        c.sorter_insert(blob.clone().into());
        c.rewind();
        assert_eq!(c.current_blob(), Some(Value::Blob(blob.into())));
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
