//! Trait-level conformance checks for [`super::cursor::Cursor`]
//! implementors (db-core#81) -- exercises the baseline contract
//! (`insert`/`rewind`/`next`/`column`/`rowid`/`seek`/`delete`) purely
//! through the trait, so a real storage-backed adapter (e.g.
//! t-rust-db/sqlite-rs's `db_storage::row::btree::TableCursor` wiring)
//! can run the same checks this crate runs against its own fixtures in
//! its own test suite, proving it satisfies the same contract without
//! this crate ever depending on the adapter (ADR 0008).
//!
//! Every check takes a factory (`impl FnMut() -> C`) rather than a
//! single instance, since each needs a *fresh* cursor built from its
//! own fixture rows to test one behavior in isolation.

use super::cursor::Cursor;
use super::value::Value;

/// Builds `cursor` up from `rows` via [`Cursor::insert`] -- every check
/// in this module needs an insert-capable cursor to construct its
/// fixture data purely through the trait, so a read-only cursor kind
/// (like this crate's own [`super::cursor::InMemoryCursor`] test
/// fixture) can't be run through this suite; that's fine, since a real
/// adapter is writable.
fn build(cursor: &mut dyn Cursor, rows: &[(i64, Vec<Value>)]) {
    for (rowid, values) in rows {
        assert!(
            cursor.insert(*rowid, values.clone()),
            "conformance suite requires an insert-capable Cursor"
        );
    }
}

/// `rewind`/`next` must visit every inserted row, in ascending-rowid
/// order, with `column`/`rowid` reading back exactly what was
/// inserted.
pub fn assert_forward_scan_matches_insertion_order<C: Cursor>(mut make: impl FnMut() -> C) {
    let rows = vec![
        (10, vec![Value::Integer(1)]),
        (20, vec![Value::Integer(2)]),
        (30, vec![Value::Integer(3)]),
    ];
    let mut cursor = make();
    build(&mut cursor, &rows);

    let mut seen = Vec::new();
    let mut has_row = cursor.rewind();
    while has_row {
        seen.push((cursor.rowid(), cursor.column(0)));
        has_row = cursor.next();
    }
    assert_eq!(
        seen,
        vec![
            (10, Value::Integer(1)),
            (20, Value::Integer(2)),
            (30, Value::Integer(3)),
        ]
    );
}

/// `seek` positions directly on an exact rowid match.
pub fn assert_seek_finds_an_exact_rowid<C: Cursor>(mut make: impl FnMut() -> C) {
    let rows = vec![(10, vec![Value::Integer(1)]), (20, vec![Value::Integer(2)])];
    let mut cursor = make();
    build(&mut cursor, &rows);

    assert!(cursor.seek(20));
    assert_eq!(cursor.column(0), Value::Integer(2));
}

/// `seek` reports `false` on a miss.
pub fn assert_seek_misses_an_absent_rowid<C: Cursor>(mut make: impl FnMut() -> C) {
    let rows = vec![(10, vec![Value::Integer(1)])];
    let mut cursor = make();
    build(&mut cursor, &rows);

    assert!(!cursor.seek(999));
}

/// `delete` removes exactly the row currently positioned at, leaving
/// every other row's scan order intact.
pub fn assert_delete_removes_only_the_current_row<C: Cursor>(mut make: impl FnMut() -> C) {
    let rows = vec![
        (10, vec![Value::Integer(1)]),
        (20, vec![Value::Integer(2)]),
        (30, vec![Value::Integer(3)]),
    ];
    let mut cursor = make();
    build(&mut cursor, &rows);

    assert!(cursor.seek(20));
    assert!(cursor.delete());

    let mut seen = Vec::new();
    let mut has_row = cursor.rewind();
    while has_row {
        seen.push(cursor.rowid());
        has_row = cursor.next();
    }
    assert_eq!(seen, vec![10, 30]);
}

/// Runs every check in this module against `make` -- the convenience
/// entry point a consumer's own test typically wants.
pub fn assert_cursor_conformance<C: Cursor>(mut make: impl FnMut() -> C) {
    assert_forward_scan_matches_insertion_order(&mut make);
    assert_seek_finds_an_exact_rowid(&mut make);
    assert_seek_misses_an_absent_rowid(&mut make);
    assert_delete_removes_only_the_current_row(&mut make);
}

/// `seek_index_eq` positions on an exact key match and reports the
/// entry's trailing rowid via `idx_rowid` (db-core#126); `column`
/// reads back the indexed value at that entry.
pub fn assert_seek_index_eq_finds_an_exact_key<C: Cursor>(mut make: impl FnMut() -> C) {
    let rows = vec![(10, vec![Value::Integer(1)]), (20, vec![Value::Integer(2)])];
    let mut cursor = make();
    build(&mut cursor, &rows);

    assert!(cursor.seek_index_eq(&[Value::Integer(2)], &[]));
    assert_eq!(cursor.column(0), Value::Integer(2));
    assert_eq!(cursor.idx_rowid(), Some(20));
}

/// `seek_index_eq` reports `false` on a key with no matching entry.
pub fn assert_seek_index_eq_misses_an_absent_key<C: Cursor>(mut make: impl FnMut() -> C) {
    let rows = vec![(10, vec![Value::Integer(1)])];
    let mut cursor = make();
    build(&mut cursor, &rows);

    assert!(!cursor.seek_index_eq(&[Value::Integer(999)], &[]));
}

/// `seek_index_ge` positions at the first entry whose key is not less
/// than the given key, even when no entry matches it exactly.
pub fn assert_seek_index_ge_positions_at_the_first_not_less_key<C: Cursor>(
    mut make: impl FnMut() -> C,
) {
    let rows = vec![(10, vec![Value::Integer(1)]), (20, vec![Value::Integer(3)])];
    let mut cursor = make();
    build(&mut cursor, &rows);

    assert!(cursor.seek_index_ge(&[Value::Integer(2)], &[]));
    assert_eq!(cursor.column(0), Value::Integer(3));
}

/// `idx_compare` reports how the current entry's key orders against an
/// arbitrary key, in both directions.
pub fn assert_idx_compare_orders_the_current_entry_against_a_key<C: Cursor>(
    mut make: impl FnMut() -> C,
) {
    let rows = vec![(10, vec![Value::Integer(5)])];
    let mut cursor = make();
    build(&mut cursor, &rows);

    assert!(cursor.rewind());
    assert_eq!(
        cursor.idx_compare(&[Value::Integer(1)], &[]),
        Some(std::cmp::Ordering::Greater)
    );
    assert_eq!(
        cursor.idx_compare(&[Value::Integer(9)], &[]),
        Some(std::cmp::Ordering::Less)
    );
}

/// Runs every index-cursor check in this module against `make` -- the
/// entry point a real index-cursor adapter's own test wants (db-core#126).
pub fn assert_index_cursor_conformance<C: Cursor>(mut make: impl FnMut() -> C) {
    assert_forward_scan_matches_insertion_order(&mut make);
    assert_seek_index_eq_finds_an_exact_key(&mut make);
    assert_seek_index_eq_misses_an_absent_key(&mut make);
    assert_seek_index_ge_positions_at_the_first_not_less_key(&mut make);
    assert_idx_compare_orders_the_current_entry_against_a_key(&mut make);
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
    use std::rc::Rc;

    use super::super::cursor::EphemeralTableCursor;
    use super::super::record::{decode_column, decode_record, encode_record};
    use super::super::value::TextEncoding;
    use super::*;

    #[test]
    fn ephemeral_table_cursor_satisfies_the_conformance_suite() {
        assert_cursor_conformance(EphemeralTableCursor::new);
    }

    /// A `TableCursor`-shaped mock: unlike [`EphemeralTableCursor`]
    /// (which decodes eagerly at insert time), this retains each row's
    /// raw encoded bytes and overrides [`Cursor::payload`] too --
    /// proving the trait is sufficient for a real adapter that wants
    /// that hook, per db-core#81's acceptance criteria.
    #[derive(Default)]
    struct MockTableCursor {
        rows: Vec<(i64, Rc<[u8]>)>,
        pos: Option<usize>,
    }

    impl Cursor for MockTableCursor {
        fn rewind(&mut self) -> bool {
            self.pos = if self.rows.is_empty() { None } else { Some(0) };
            self.pos.is_some()
        }

        fn next(&mut self) -> bool {
            let next = self.pos.map_or(0, |p| p + 1);
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
            let (_, blob) = &self.rows[pos];
            decode_column(blob, col, TextEncoding::Utf8).unwrap_or(Value::Null)
        }

        fn rowid(&self) -> i64 {
            let pos = self.pos.expect("rowid read with no current row");
            self.rows[pos].0
        }

        fn seek(&mut self, rowid: i64) -> bool {
            match self.rows.iter().position(|(r, _)| *r == rowid) {
                Some(pos) => {
                    self.pos = Some(pos);
                    true
                }
                None => {
                    self.pos = None;
                    false
                }
            }
        }

        fn payload(&self) -> Option<Rc<[u8]>> {
            let pos = self.pos?;
            Some(self.rows[pos].1.clone())
        }

        fn insert(&mut self, rowid: i64, values: Vec<Value>) -> bool {
            let blob: Rc<[u8]> = encode_record(&values, TextEncoding::Utf8).into();
            let pos = self.rows.partition_point(|(r, _)| *r < rowid);
            if self.rows.get(pos).is_some_and(|(r, _)| *r == rowid) {
                self.rows[pos] = (rowid, blob);
            } else {
                self.rows.insert(pos, (rowid, blob));
            }
            true
        }

        fn delete(&mut self) -> bool {
            let Some(pos) = self.pos else {
                return false;
            };
            self.rows.remove(pos);
            self.pos = None;
            true
        }
    }

    #[test]
    fn mock_table_cursor_satisfies_the_conformance_suite() {
        assert_cursor_conformance(MockTableCursor::default);
    }

    #[test]
    fn in_memory_index_cursor_satisfies_the_index_conformance_suite() {
        use super::super::cursor::InMemoryIndexCursor;
        use super::super::program::SortKeyColumn;
        use super::super::value::Collation;

        assert_index_cursor_conformance(|| {
            InMemoryIndexCursor::new(vec![SortKeyColumn {
                index: 0,
                descending: false,
                collation: Collation::Binary,
                nulls_first: false,
            }])
        });
    }

    #[test]
    fn mock_table_cursor_payload_exposes_raw_encoded_bytes() {
        let mut cursor = MockTableCursor::default();
        cursor.insert(1, vec![Value::Integer(42)]);
        cursor.rewind();
        let payload = cursor.payload().unwrap();
        assert_eq!(
            decode_record(&payload, TextEncoding::Utf8).unwrap(),
            vec![Value::Integer(42)]
        );
    }
}
