//! The storage-agnostic cursor trait ADR 0008 calls for: `vm::row`
//! defines this boundary itself and an adapter crate (e.g. `db-storage`)
//! implements it over a real b-tree, rather than `vm::row` depending on
//! `db-storage` directly (ADR 0006's storage-agnostic stance for
//! `db-core`). [`InMemoryCursor`] is the mock implementation this
//! phase's tests exercise the dispatch loop against; real
//! `db-storage::row::btree::TableCursor` wiring is a follow-up (the
//! largest remaining sqlite-rs VDBE file, `cursor.rs`, 4,498 lines).

use super::value::Value;

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

    fn column(&self, col: usize) -> Value {
        let pos = self.pos.expect("column read with no current row");
        self.rows
            .get(pos)
            .and_then(|row| row.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        let pos = self.pos.expect("rowid read with no current row");
        #[allow(clippy::cast_possible_wrap)]
        {
            (pos as i64).saturating_add(1)
        }
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

    fn column(&self, col: usize) -> Value {
        let pos = self.pos.expect("column read with no current row");
        self.rows
            .get(pos)
            .and_then(|(_, values)| values.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        let pos = self.pos.expect("rowid read with no current row");
        self.rows.get(pos).map_or(0, |(rowid, _)| *rowid)
    }

    fn insert(&mut self, rowid: i64, values: Vec<Value>) -> bool {
        self.rows.push((rowid, values));
        true
    }
}

#[cfg(test)]
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
}
