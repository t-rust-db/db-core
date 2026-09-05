//! The storage-agnostic cursor trait ADR 0008 calls for: `vm::row`
//! defines this boundary itself and an adapter crate (e.g. `db-storage`)
//! implements it over a real b-tree, rather than `vm::row` depending on
//! `db-storage` directly (ADR 0006's storage-agnostic stance for
//! `db-core`). [`InMemoryCursor`] is the mock implementation this
//! phase's tests exercise the dispatch loop against; real
//! `db-storage::row::btree::TableCursor` wiring is a follow-up (the
//! largest remaining sqlite-rs VDBE file, `cursor.rs`, 4,498 lines).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use super::aggregate::{AggState, AggregateError};
use super::program::{GroupKeyColumn, SortKeyColumn};
use super::record::{decode_column, decode_record, encode_record};
use super::value::{Collation, TextEncoding, Value};

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

    /// Seeks directly to the row with `rowid`, without a linear
    /// `rewind`/`next` scan (`Opcode::SeekRowid`). Returns `true` (and
    /// positions the cursor there) if it exists; `false` (and leaves
    /// the cursor with no current row) on a miss. Default `false` --
    /// unsupported, same rationale as [`Cursor::last`] -- so a real
    /// b-tree-backed adapter (db-core#81) is the one expected to
    /// override this with an actual key-based seek; this crate's own
    /// in-memory fixtures ([`InMemoryCursor`], [`EphemeralTableCursor`])
    /// override it too, cheaply, since they already index by rowid.
    fn seek(&mut self, _rowid: i64) -> bool {
        false
    }

    /// The current row's raw, still-encoded record bytes, for an
    /// adapter that wants to decode lazily rather than eagerly on
    /// every [`Cursor::column`] call (db-core#81) -- e.g. a real
    /// b-tree cursor already holds a page's raw bytes and would rather
    /// decode once, on demand, than per column. `None` if this cursor
    /// kind doesn't retain raw bytes (every cursor kind in this crate
    /// decodes eagerly at `insert` time instead, so none override this)
    /// or has no current row. Not consumed by any opcode dispatch in
    /// this crate today -- unlike [`Cursor::current_blob`] (the
    /// sorter's own record blob, which `Opcode::SorterData` does
    /// dispatch), this exists purely as a hook for a real adapter.
    fn payload(&self) -> Option<Rc<[u8]>> {
        None
    }

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

    /// Locates (creating on first sight) the group `blob` -- this
    /// row's `MakeRecord`-encoded bytes -- belongs to, making it the
    /// current group (`Opcode::HashAggFind`'s dispatch target); `blob`
    /// itself is retained only if this is the group's first row.
    /// Returns `false` if this cursor kind isn't a hash-aggregation
    /// cursor.
    fn hash_agg_find(&mut self, _blob: Rc<[u8]>) -> bool {
        false
    }

    /// Folds `args` into accumulator `slot` of the current group
    /// (`Opcode::HashAggStep`'s dispatch target). `Ok(false)` if this
    /// cursor kind isn't a hash-aggregation cursor, or no group is
    /// current yet.
    fn hash_agg_step(
        &mut self,
        _slot: usize,
        _name: &str,
        _args: &[Value],
        _collation: Collation,
    ) -> Result<bool, AggregateError> {
        Ok(false)
    }

    /// The current group's accumulators, indexed by `Opcode::
    /// HashAggStep`'s `p1` slot -- `Opcode::HashAggData`'s dispatch
    /// target installs these into `Vm`'s aggregate-context table so
    /// the ordinary `AggFinal` opcodes that follow need no
    /// hash-specific case at all. `None` if this cursor kind isn't a
    /// hash-aggregation cursor, or there is no current group.
    fn hash_agg_group_accumulators(&self) -> Option<&[Option<AggState>]> {
        None
    }

    /// A fast, O(1) row count for this cursor's table, if this cursor
    /// kind tracks one (`Opcode::Count`'s fast path, db-core#127). `None`
    /// (the default) tells the dispatcher to fall back to a full
    /// `rewind`/`next` scan instead.
    fn count(&self) -> Option<i64> {
        None
    }

    /// Appends `rowid` under `key` into this automatic-index cursor
    /// (`Opcode::AutoIndexInsert`, db-core#127) -- the transient index a
    /// join plan builds over one side's rows in memory, never touching
    /// storage. Returns `false` if this cursor kind isn't an
    /// automatic-index cursor.
    fn auto_index_insert(
        &mut self,
        _key: Vec<Value>,
        _collations: &[Collation],
        _rowid: i64,
    ) -> bool {
        false
    }

    /// Seeks this automatic-index cursor to the first entry equal to
    /// `key` (`Opcode::AutoIndexSeek`). Returns `false` (and leaves no
    /// current entry) on a miss, or if this cursor kind isn't an
    /// automatic-index cursor.
    fn auto_index_seek(&mut self, _key: &[Value], _collations: &[Collation]) -> bool {
        false
    }

    /// Advances this automatic-index cursor to the next entry sharing
    /// the current entry's key (`Opcode::AutoIndexNext`). Returns
    /// `false` if there wasn't one, or this cursor kind isn't an
    /// automatic-index cursor.
    fn auto_index_next(&mut self) -> bool {
        false
    }

    /// Seeks this index cursor to the first entry whose key equals
    /// `key` exactly (`Opcode::SeekIndexEq`/`Found`/`NoConflict`,
    /// db-core#126). Returns `false` (and leaves no current entry) on a
    /// miss, or if this cursor kind isn't an index cursor.
    fn seek_index_eq(&mut self, _key: &[Value], _collations: &[Collation]) -> bool {
        false
    }

    /// Seeks this index cursor to the first entry whose key is `>=`
    /// `key` (`Opcode::SeekIndexGE`, db-core#126). Returns `false` (and
    /// leaves no current entry) if no such entry exists, or this cursor
    /// kind isn't an index cursor.
    fn seek_index_ge(&mut self, _key: &[Value], _collations: &[Collation]) -> bool {
        false
    }

    /// Compares this index cursor's current entry's key against `key`
    /// (`Opcode::IdxCompareGT`/`IdxLE`, db-core#126). `None` if this
    /// cursor kind isn't an index cursor, or there is no current entry.
    fn idx_compare(&self, _key: &[Value], _collations: &[Collation]) -> Option<std::cmp::Ordering> {
        None
    }

    /// This index cursor's current entry's trailing rowid column
    /// (`Opcode::IdxRowid`, db-core#126) -- distinct from
    /// [`Cursor::rowid`] since an index entry's "rowid" is itself one
    /// more key column, not the entry's own identity the way a table
    /// cursor's rowid is. `None` if this cursor kind isn't an index
    /// cursor, or there is no current entry.
    fn idx_rowid(&self) -> Option<i64> {
        None
    }

    /// `Found`/`NoConflict` on an ephemeral index (sqlite-rs
    /// `CursorSlot::Ephemeral`): membership of the normalized, encoded
    /// key, remembering it as the cursor's `last_key`. `None` means "not
    /// an ephemeral index" and the dispatcher falls back to
    /// [`Self::seek_index_eq`] (#134).
    fn found(&mut self, _key: &[Value], _collations: &[Collation]) -> Option<bool> {
        None
    }

    /// `IdxInsert` on an ephemeral index: store `stored` (the key columns
    /// plus `p5` trailing payload columns) under the normalized, encoded
    /// `key`. `None` means "not an ephemeral index" and the dispatcher
    /// falls back to [`Self::idx_insert`] (#134).
    fn ephemeral_idx_insert(
        &mut self,
        _key: &[Value],
        _collations: &[Collation],
        _stored: Vec<Value>,
    ) -> Option<bool> {
        None
    }

    /// `Insert` with the exact record bytes `MakeRecord` produced, so a
    /// storage-backed cursor writes them byte-for-byte instead of
    /// re-encoding decoded values. `None` means "decode and use
    /// [`Self::insert`]" (the in-memory cursors) (#134).
    fn insert_payload(&mut self, _rowid: i64, _payload: &Rc<[u8]>) -> Option<bool> {
        None
    }

    /// `OpenDup`: a second cursor over the same underlying rows with a
    /// fresh position (sqlite-rs: ephemeral tables only). `None` means
    /// the dispatcher re-opens through the cursor factory instead (#134).
    fn dup(&self) -> Option<Box<dyn Cursor>> {
        None
    }

    /// First value `Sequence` hands out for this cursor kind — 1 for an
    /// ephemeral table, 0 otherwise (sqlite-rs) (#134).
    fn sequence_base(&self) -> i64 {
        0
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

    fn seek(&mut self, rowid: i64) -> bool {
        let idx = usize::try_from(rowid.saturating_sub(1)).ok();
        match idx.filter(|&idx| idx < self.rows.len()) {
            Some(idx) => {
                self.pos = Some(idx);
                true
            }
            None => {
                self.pos = None;
                false
            }
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
/// One materialized row of an ephemeral table: `(rowid, values)`.
type EphemeralRow = (i64, Vec<Value>);
/// Rows shared between an ephemeral table and its `OpenDup` siblings.
type SharedRows = Rc<RefCell<Vec<EphemeralRow>>>;

pub struct EphemeralTableCursor {
    /// Sorted ascending by rowid (the first element of each tuple);
    /// shared with any `OpenDup` sibling (sqlite-rs
    /// `EphemeralTableState::rows`, #134).
    rows: SharedRows,
    /// The rowid last positioned at, kept even after that row is
    /// deleted (a fence-post `next()`/`prev()` resume from) --
    /// [`Self::at_row`] says whether it still names a live row.
    current_rowid: Option<i64>,
    at_row: bool,
}

impl Default for EphemeralTableCursor {
    fn default() -> Self {
        EphemeralTableCursor {
            rows: Rc::new(RefCell::new(Vec::new())),
            current_rowid: None,
            at_row: false,
        }
    }
}

impl EphemeralTableCursor {
    pub fn new() -> Self {
        EphemeralTableCursor::default()
    }

    fn row_index(&self, rowid: i64) -> Option<usize> {
        self.rows
            .borrow()
            .binary_search_by_key(&rowid, |(r, _)| *r)
            .ok()
    }

    fn position_at(&mut self, rowid: Option<i64>) -> bool {
        match rowid {
            Some(rowid) => {
                self.current_rowid = Some(rowid);
                self.at_row = true;
                true
            }
            None => {
                self.at_row = false;
                false
            }
        }
    }
}

impl Cursor for EphemeralTableCursor {
    fn rewind(&mut self) -> bool {
        let first = self.rows.borrow().first().map(|(r, _)| *r);
        if first.is_none() {
            self.current_rowid = None;
        }
        self.position_at(first)
    }

    fn next(&mut self) -> bool {
        let after = self.current_rowid.unwrap_or(i64::MIN);
        let found = {
            let rows = self.rows.borrow();
            let pos = rows.partition_point(|(r, _)| *r <= after);
            rows.get(pos).map(|(r, _)| *r)
        };
        self.position_at(found)
    }

    fn last(&mut self) -> bool {
        let last = self.rows.borrow().last().map(|(r, _)| *r);
        if last.is_none() {
            self.current_rowid = None;
        }
        self.position_at(last)
    }

    fn prev(&mut self) -> bool {
        let before = self.current_rowid.unwrap_or(i64::MAX);
        let found = {
            let rows = self.rows.borrow();
            let pos = rows.partition_point(|(r, _)| *r < before);
            pos.checked_sub(1)
                .and_then(|p| rows.get(p))
                .map(|(r, _)| *r)
        };
        self.position_at(found)
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
        self.rows
            .borrow()
            .get(idx)
            .and_then(|(_, values)| values.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        assert!(self.at_row, "rowid read with no current row");
        #[allow(
            clippy::expect_used,
            reason = "Cursor contract: column/rowid are only read after a successful rewind/next"
        )]
        self.current_rowid.expect("rowid read with no current row")
    }

    fn seek(&mut self, rowid: i64) -> bool {
        let hit = self.row_index(rowid).map(|_| rowid);
        self.position_at(hit)
    }

    fn insert(&mut self, rowid: i64, values: Vec<Value>) -> bool {
        let mut rows = self.rows.borrow_mut();
        let pos = rows.partition_point(|(r, _)| *r < rowid);
        if let Some(existing) = rows.get_mut(pos).filter(|(r, _)| *r == rowid) {
            existing.1 = values;
        } else {
            if rows.len() >= MAX_EPHEMERAL_ROWS {
                return false;
            }
            rows.insert(pos, (rowid, values));
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
        self.rows.borrow_mut().remove(idx);
        self.at_row = false;
        true
    }

    fn next_rowid(&self) -> i64 {
        self.rows
            .borrow()
            .last()
            .map_or(0, |(rowid, _)| *rowid)
            .saturating_add(1)
    }

    fn idx_insert(&mut self, key: Vec<Value>) -> bool {
        self.rows.borrow_mut().push((0, key));
        true
    }

    fn idx_delete(&mut self, key: &[Value]) -> bool {
        let mut rows = self.rows.borrow_mut();
        let Some(pos) = rows.iter().position(|(_, values)| values == key) else {
            return false;
        };
        rows.remove(pos);
        true
    }

    fn dup(&self) -> Option<Box<dyn Cursor>> {
        Some(Box::new(EphemeralTableCursor {
            rows: Rc::clone(&self.rows),
            current_rowid: None,
            at_row: false,
        }))
    }

    fn sequence_base(&self) -> i64 {
        1
    }
}

/// Row-count ceiling on an in-memory ephemeral table/index (sqlite-rs
/// #269): neither spills to disk, so an unbounded subquery could otherwise
/// grow memory without limit.
pub const MAX_EPHEMERAL_ROWS: usize = 1_000_000;

/// Folds `values` under `collations` the way sqlite-rs does before
/// encoding an ephemeral-index or auto-index key, so NOCASE/RTRIM keys
/// compare equal by byte comparison of the encoded record (#134).
pub fn normalize_key_values(values: &[Value], collations: &[Collation]) -> Vec<Value> {
    values
        .iter()
        .zip(
            collations
                .iter()
                .chain(std::iter::repeat(&Collation::Binary)),
        )
        .map(|(v, collation)| match (v, collation) {
            (Value::Text(s), Collation::NoCase) => {
                Value::Text(Rc::from(s.to_ascii_lowercase().as_str()))
            }
            (Value::Text(s), Collation::RTrim) => Value::Text(Rc::from(s.trim_end_matches(' '))),
            _ => v.clone(),
        })
        .collect()
}

fn encode_key(values: &[Value], collations: &[Collation]) -> Vec<u8> {
    encode_record(
        &normalize_key_values(values, collations),
        TextEncoding::Utf8,
    )
}

/// sqlite-rs's `CursorSlot::Ephemeral` (`OpenEphemeral p5 = 0`, #134): the
/// in-memory index behind DISTINCT and `IN (SELECT …)` — entries keyed by
/// the encoded, collation-normalized key record, each holding the stored
/// values (key columns plus any trailing payload columns `IdxInsert`'s
/// `p5` asked for). `Found`/`IdxInsert` remember the key they touched so
/// a following `Delete` or `Column` acts on it.
#[derive(Default)]
pub struct EphemeralIndexCursor {
    entries: BTreeMap<Vec<u8>, Vec<Value>>,
    last_key: Option<Vec<u8>>,
}

impl EphemeralIndexCursor {
    pub fn new() -> Self {
        EphemeralIndexCursor::default()
    }
}

impl Cursor for EphemeralIndexCursor {
    // Never scanned: sqlite-rs drives this kind only through
    // Found/IdxInsert/IdxDelete/Delete/Column/Sequence.
    fn rewind(&mut self) -> bool {
        false
    }

    fn next(&mut self) -> bool {
        false
    }

    fn column(&self, col: usize) -> Value {
        self.last_key
            .as_ref()
            .and_then(|k| self.entries.get(k))
            .and_then(|values| values.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        0
    }

    fn found(&mut self, key: &[Value], collations: &[Collation]) -> Option<bool> {
        let encoded = encode_key(key, collations);
        let present = self.entries.contains_key(&encoded);
        self.last_key = Some(encoded);
        Some(present)
    }

    fn ephemeral_idx_insert(
        &mut self,
        key: &[Value],
        collations: &[Collation],
        stored: Vec<Value>,
    ) -> Option<bool> {
        let encoded = encode_key(key, collations);
        if !self.entries.contains_key(&encoded) && self.entries.len() >= MAX_EPHEMERAL_ROWS {
            return Some(false);
        }
        self.entries.insert(encoded.clone(), stored);
        self.last_key = Some(encoded);
        Some(true)
    }

    fn idx_delete(&mut self, key: &[Value]) -> bool {
        let encoded = encode_record(key, TextEncoding::Utf8);
        self.entries.remove(&encoded);
        if self.last_key.as_ref() == Some(&encoded) {
            self.last_key = None;
        }
        true
    }

    fn delete(&mut self) -> bool {
        if let Some(key) = self.last_key.take() {
            self.entries.remove(&key);
        }
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

/// One `GROUP BY` group's accumulated state: the row retained to
/// answer plain (non-aggregate) column references, that row's already
/// -decoded key values (kept only to order the groups at
/// `HashAggRewind`), and one accumulator per `HashAggStep` slot.
struct GroupSlot {
    /// The `MakeRecord`-encoded *first* row seen for this group,
    /// handed back verbatim by `HashAggData`. First rather than last
    /// on purpose, matching SQLite's "arbitrary row" semantics for a
    /// plain column and the sort strategy's stable-sort tie-break.
    row: Rc<[u8]>,
    /// This group's key values, in key order.
    key_values: Vec<Value>,
    /// One accumulator per `HashAggStep` slot; `None` for a slot this
    /// group never stepped.
    accumulators: Vec<Option<AggState>>,
}

/// `GROUP BY` aggregation, backing `Opcode::HashAggOpen`/`Find`/`Step`/
/// `Rewind`/`Data`/`Next` (db-core#86). The O(n) alternative to
/// [`SorterCursor`]'s sort-then-group strategy -- see this module's own
/// scope note.
///
/// **Group lookup is a linear scan over `groups`, not an actual
/// hash table** -- a correctness-equivalent, simpler stand-in (same
/// tradeoff [`SorterCursor`]'s bound truncation makes): building a
/// `Hash` impl consistent with [`super::compare::compare`]'s
/// collation-aware equality (so `1` and `1.0` hash alike) is
/// non-trivial and not worth it for a reference VM that is not
/// perf-critical. `HashAggRewind` still only sorts the `K` distinct
/// groups, not all `n` rows, so the strategy's O(rows) find + O(K log
/// K) order still beats the sort strategy's O(n log n) whenever groups
/// are few relative to rows.
pub struct HashAggCursor {
    keys: Vec<GroupKeyColumn>,
    groups: Vec<GroupSlot>,
    /// Group positions in output order, filled in by `rewind` (freezing
    /// the group set -- no further `hash_agg_find` after this).
    order: Vec<usize>,
    pos: Option<usize>,
    frozen: bool,
    /// The group `hash_agg_find` most recently located, which
    /// `hash_agg_step` folds into. `None` before the first find.
    current_group: Option<usize>,
}

impl HashAggCursor {
    pub fn new(keys: Vec<GroupKeyColumn>) -> Self {
        HashAggCursor {
            keys,
            groups: Vec::new(),
            order: Vec::new(),
            pos: None,
            frozen: false,
            current_group: None,
        }
    }

    fn keys_equal(&self, a: &[Value], b: &[Value]) -> bool {
        self.keys.iter().enumerate().all(|(i, key)| {
            matches!(
                (a.get(i), b.get(i)),
                (Some(av), Some(bv)) if super::compare::compare(av, bv, key.collation) == std::cmp::Ordering::Equal
            )
        })
    }

    fn find_group(&self, key_values: &[Value]) -> Option<usize> {
        self.groups
            .iter()
            .position(|g| self.keys_equal(&g.key_values, key_values))
    }
}

impl Cursor for HashAggCursor {
    /// Freezes the group set, orders groups by key (ascending, per
    /// column, matching the sort strategy's group-boundary order), and
    /// positions at the first -- `HashAggRewind`'s dispatch target.
    fn rewind(&mut self) -> bool {
        if !self.frozen {
            let mut order: Vec<usize> = (0..self.groups.len()).collect();
            let keys = &self.keys;
            let groups = &self.groups;
            order.sort_by(|&a, &b| {
                let (ka, kb) = (&groups[a].key_values, &groups[b].key_values);
                for (i, key) in keys.iter().enumerate() {
                    let ord = super::compare::compare(&ka[i], &kb[i], key.collation);
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });
            self.order = order;
            self.frozen = true;
        }
        self.pos = if self.order.is_empty() { None } else { Some(0) };
        self.pos.is_some()
    }

    fn next(&mut self) -> bool {
        let next = self.pos.map_or(0, |p| p.saturating_add(1));
        if next < self.order.len() {
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
        self.order
            .get(pos)
            .and_then(|&i| self.groups.get(i))
            .and_then(|g| decode_record(&g.row, TextEncoding::Utf8).ok())
            .and_then(|values| values.get(col).cloned())
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        0 // groups have no rowid concept, same as SorterCursor
    }

    fn current_blob(&self) -> Option<Value> {
        let pos = self.pos?;
        let i = *self.order.get(pos)?;
        self.groups.get(i).map(|g| Value::Blob(g.row.clone()))
    }

    fn hash_agg_find(&mut self, blob: Rc<[u8]>) -> bool {
        let key_values: Vec<Value> = self
            .keys
            .iter()
            .map(|k| {
                let mut value =
                    decode_column(&blob, k.index, TextEncoding::Utf8).unwrap_or(Value::Null);
                super::affinity::apply_affinity(
                    &mut value,
                    super::affinity::Affinity::from_p4_byte(k.affinity),
                );
                value
            })
            .collect();
        let idx = match self.find_group(&key_values) {
            Some(idx) => idx,
            None => {
                self.groups.push(GroupSlot {
                    row: blob,
                    key_values,
                    accumulators: Vec::new(),
                });
                self.groups.len() - 1
            }
        };
        self.current_group = Some(idx);
        true
    }

    fn hash_agg_step(
        &mut self,
        slot: usize,
        name: &str,
        args: &[Value],
        collation: Collation,
    ) -> Result<bool, AggregateError> {
        let Some(idx) = self.current_group else {
            return Ok(false);
        };
        let Some(group) = self.groups.get_mut(idx) else {
            return Ok(false);
        };
        if group.accumulators.len() <= slot {
            group.accumulators.resize(slot + 1, None);
        }
        let current = group.accumulators[slot].take();
        let updated = super::aggregate::step(name, current, args, collation)?;
        group.accumulators[slot] = Some(updated);
        Ok(true)
    }

    fn hash_agg_group_accumulators(&self) -> Option<&[Option<AggState>]> {
        // Before `rewind` freezes the group set, "current" means the
        // group `hash_agg_find` most recently located (the build
        // phase); after, it follows `pos`/`order` like `column`/
        // `current_blob` do (the output phase) -- `HashAggStep` and
        // `HashAggData` never interleave per sqlite-rs's own opcode
        // shape, so this never needs to serve both at once.
        let idx = if self.frozen {
            *self.order.get(self.pos?)?
        } else {
            self.current_group?
        };
        self.groups.get(idx).map(|g| g.accumulators.as_slice())
    }
}

/// A one-row cursor over an already-`MakeRecord`-encoded blob
/// (`Opcode::OpenPseudo`, db-core#125) -- sqlite-rs's `PseudoCursor`,
/// used to run a subquery's/join's inner side's result row back through
/// ordinary `Column` reads without a real table backing it.
///
/// **Simplification**: sqlite-rs's pseudo-cursor re-reads its content
/// register live on every `Column`, so it reflects whatever that
/// register holds *at read time* (the register is typically
/// overwritten once per outer-loop iteration by a preceding
/// `MakeRecord`). This type instead decodes the blob once, at
/// `OpenPseudo` time, and serves that same row for as long as the
/// cursor stays open -- correct for `OpenPseudo`'s common one-shot use
/// (open, read once, done) but not for a pseudo-cursor a codegen
/// re-populates across loop iterations without re-opening it. A real
/// live-register pseudo-cursor needs `Vm` access `Cursor` doesn't have;
/// revisit if a corpus test needs it.
pub struct PseudoCursor {
    values: Vec<Value>,
}

impl PseudoCursor {
    /// Builds a pseudo-cursor over `blob`'s already-`MakeRecord`-encoded
    /// row, decoded once up front (see this type's own doc).
    pub fn new(blob: &[u8]) -> Self {
        let values = decode_record(blob, TextEncoding::Utf8).unwrap_or_default();
        PseudoCursor { values }
    }
}

impl Cursor for PseudoCursor {
    fn rewind(&mut self) -> bool {
        true
    }

    fn next(&mut self) -> bool {
        false
    }

    fn column(&self, col: usize) -> Value {
        self.values.get(col).cloned().unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        0
    }
}

/// A transient, in-memory join index (`Opcode::AutoIndexInsert`/`Seek`/
/// `Rowid`/`Next`, db-core#127) -- sqlite-rs's `AutoIndexState`, built
/// by a join plan over one side's rows when no real index serves the
/// join condition, and thrown away once the join finishes. Entries are
/// kept sorted by key so duplicate keys land contiguously
/// (`auto_index_next`'s "next entry sharing this key" walk).
#[derive(Default)]
pub struct AutoIndexCursor {
    /// Encoded, collation-normalized key -> rowids in insertion order
    /// (sqlite-rs `AutoIndexState::entries`, #134).
    entries: HashMap<Vec<u8>, Vec<i64>>,
    /// `(key, position within that key's rowid list)` after a hit.
    current: Option<(Vec<u8>, usize)>,
}

impl AutoIndexCursor {
    pub fn new() -> Self {
        AutoIndexCursor::default()
    }
}

impl Cursor for AutoIndexCursor {
    // An automatic index is only ever driven by `auto_index_*` (never
    // `Rewind`/`Next`, per its own opcodes' doc), so these have no real
    // work to do.
    fn rewind(&mut self) -> bool {
        false
    }

    fn next(&mut self) -> bool {
        false
    }

    fn column(&self, _col: usize) -> Value {
        Value::Null
    }

    fn rowid(&self) -> i64 {
        self.current
            .as_ref()
            .and_then(|(key, pos)| self.entries.get(key).and_then(|r| r.get(*pos)))
            .copied()
            .unwrap_or(0)
    }

    fn auto_index_insert(&mut self, key: Vec<Value>, collations: &[Collation], rowid: i64) -> bool {
        self.entries
            .entry(encode_key(&key, collations))
            .or_default()
            .push(rowid);
        true
    }

    fn auto_index_seek(&mut self, key: &[Value], collations: &[Collation]) -> bool {
        let encoded = encode_key(key, collations);
        let has_match = self
            .entries
            .get(&encoded)
            .is_some_and(|rowids| !rowids.is_empty());
        self.current = has_match.then_some((encoded, 0));
        has_match
    }

    fn auto_index_next(&mut self) -> bool {
        let Some((key, pos)) = self.current.take() else {
            return false;
        };
        let len = self.entries.get(&key).map_or(0, Vec::len);
        let next_pos = pos.saturating_add(1);
        if next_pos < len {
            self.current = Some((key, next_pos));
            true
        } else {
            false
        }
    }
}

/// A storage-agnostic index-mode cursor (db-core#126) -- entries are
/// key columns plus a trailing rowid, kept sorted by key per `key_cols`
/// (reusing [`SortKeyColumn`]'s direction/collation/NULLS shape, the
/// same descriptor [`SorterCursor`] already keys on). The real adapter
/// over a b-tree index lives in the consumer (ADR 0008); this is purely
/// this crate's own test fixture, proven sufficient by
/// `cursor_conformance`'s index-contract checks.
pub struct InMemoryIndexCursor {
    key_cols: Vec<SortKeyColumn>,
    /// Sorted ascending per `key_cols`.
    entries: Vec<(Vec<Value>, i64)>,
    pos: Option<usize>,
}

impl InMemoryIndexCursor {
    pub fn new(key_cols: Vec<SortKeyColumn>) -> Self {
        InMemoryIndexCursor {
            key_cols,
            entries: Vec::new(),
            pos: None,
        }
    }

    fn key_of(&self, values: &[Value]) -> Vec<Value> {
        self.key_cols
            .iter()
            .map(|k| values.get(k.index).cloned().unwrap_or(Value::Null))
            .collect()
    }
}

impl Cursor for InMemoryIndexCursor {
    fn rewind(&mut self) -> bool {
        self.pos = if self.entries.is_empty() {
            None
        } else {
            Some(0)
        };
        self.pos.is_some()
    }

    fn next(&mut self) -> bool {
        let next = self.pos.map_or(0, |p| p.saturating_add(1));
        if next < self.entries.len() {
            self.pos = Some(next);
            true
        } else {
            self.pos = None;
            false
        }
    }

    fn last(&mut self) -> bool {
        self.pos = self.entries.len().checked_sub(1);
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
            reason = "Cursor contract: column/rowid are only read after a successful positioning call"
        )]
        let pos = self.pos.expect("column read with no current entry");
        self.entries
            .get(pos)
            .and_then(|(key, _)| key.get(col))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn rowid(&self) -> i64 {
        #[allow(
            clippy::expect_used,
            reason = "Cursor contract: column/rowid are only read after a successful positioning call"
        )]
        let pos = self.pos.expect("rowid read with no current entry");
        self.entries[pos].1
    }

    /// Inserts `values` (the row's indexed columns; extra columns
    /// beyond `key_cols`'s width are ignored) under `rowid`, keeping
    /// `entries` sorted -- this fixture's only way to build index data
    /// purely through the trait, matching `cursor_conformance`'s
    /// `build` helper.
    fn insert(&mut self, rowid: i64, values: Vec<Value>) -> bool {
        let key = self.key_of(&values);
        let pos = self.entries.partition_point(|(k, _)| {
            compare_keys(k, &key, &self.key_cols) == std::cmp::Ordering::Less
        });
        self.entries.insert(pos, (key, rowid));
        true
    }

    fn seek_index_eq(&mut self, key: &[Value], _collations: &[Collation]) -> bool {
        let pos = self.entries.partition_point(|(k, _)| {
            compare_keys(k, key, &self.key_cols) == std::cmp::Ordering::Less
        });
        match self.entries.get(pos) {
            Some((k, _)) if compare_keys(k, key, &self.key_cols) == std::cmp::Ordering::Equal => {
                self.pos = Some(pos);
                true
            }
            _ => {
                self.pos = None;
                false
            }
        }
    }

    fn seek_index_ge(&mut self, key: &[Value], _collations: &[Collation]) -> bool {
        let pos = self.entries.partition_point(|(k, _)| {
            compare_keys(k, key, &self.key_cols) == std::cmp::Ordering::Less
        });
        if pos < self.entries.len() {
            self.pos = Some(pos);
            true
        } else {
            self.pos = None;
            false
        }
    }

    fn idx_compare(&self, key: &[Value], _collations: &[Collation]) -> Option<std::cmp::Ordering> {
        let pos = self.pos?;
        let (current, _) = self.entries.get(pos)?;
        Some(compare_keys(current, key, &self.key_cols))
    }

    fn idx_rowid(&self) -> Option<i64> {
        let pos = self.pos?;
        self.entries.get(pos).map(|(_, rowid)| *rowid)
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
    fn seek_positions_on_exact_rowid() {
        let mut c = InMemoryCursor::new(vec![
            vec![Value::Integer(10)],
            vec![Value::Integer(20)],
            vec![Value::Integer(30)],
        ]);
        assert!(c.seek(2));
        assert_eq!(c.column(0), Value::Integer(20));
    }

    #[test]
    fn seek_misses_out_of_range_rowid() {
        let mut c = InMemoryCursor::new(vec![vec![Value::Integer(10)]]);
        assert!(!c.seek(2));
        assert!(!c.seek(0));
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

    #[test]
    fn ephemeral_table_cursor_seek_positions_on_exact_rowid() {
        let mut c = EphemeralTableCursor::new();
        c.insert(10, vec![Value::Integer(1)]);
        c.insert(20, vec![Value::Integer(2)]);
        assert!(c.seek(20));
        assert_eq!(c.column(0), Value::Integer(2));
    }

    #[test]
    fn ephemeral_table_cursor_seek_misses_absent_rowid() {
        let mut c = EphemeralTableCursor::new();
        c.insert(10, vec![Value::Integer(1)]);
        assert!(!c.seek(99));
    }

    fn group_key(index: usize) -> GroupKeyColumn {
        GroupKeyColumn {
            index,
            collation: super::super::value::Collation::Binary,
            affinity: b'A',
        }
    }

    fn make_row(values: &[Value]) -> Rc<[u8]> {
        super::super::record::encode_record(values, TextEncoding::Utf8).into()
    }

    #[test]
    fn hash_agg_cursor_groups_by_key_and_orders_by_key_on_rewind() {
        let mut c = HashAggCursor::new(vec![group_key(0)]);
        for group in ["b", "a", "b", "c"] {
            assert!(c.hash_agg_find(make_row(&[Value::Text(group.to_string().into())])));
        }
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Text("a".to_string().into()));
        assert!(c.next());
        assert_eq!(c.column(0), Value::Text("b".to_string().into()));
        assert!(c.next());
        assert_eq!(c.column(0), Value::Text("c".to_string().into()));
        assert!(!c.next());
    }

    #[test]
    fn hash_agg_cursor_retains_first_row_seen_per_group() {
        let mut c = HashAggCursor::new(vec![group_key(0)]);
        assert!(c.hash_agg_find(make_row(&[Value::Integer(1), Value::Integer(100)])));
        assert!(c.hash_agg_find(make_row(&[Value::Integer(1), Value::Integer(200)])));
        assert!(c.rewind());
        assert_eq!(c.column(1), Value::Integer(100));
    }

    #[test]
    fn hash_agg_cursor_steps_fold_into_the_current_group_only() {
        let mut c = HashAggCursor::new(vec![group_key(0)]);
        assert!(c.hash_agg_find(make_row(&[Value::Text("x".to_string().into())])));
        c.hash_agg_step(
            0,
            "count",
            &[Value::Integer(1)],
            super::super::value::Collation::Binary,
        )
        .unwrap();
        c.hash_agg_step(
            0,
            "count",
            &[Value::Integer(1)],
            super::super::value::Collation::Binary,
        )
        .unwrap();
        assert!(c.hash_agg_find(make_row(&[Value::Text("y".to_string().into())])));
        c.hash_agg_step(
            0,
            "count",
            &[Value::Integer(1)],
            super::super::value::Collation::Binary,
        )
        .unwrap();

        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Text("x".to_string().into()));
        assert_eq!(
            c.hash_agg_group_accumulators(),
            Some([Some(AggState::Count(2))].as_slice())
        );
        assert!(c.next());
        assert_eq!(c.column(0), Value::Text("y".to_string().into()));
        assert_eq!(
            c.hash_agg_group_accumulators(),
            Some([Some(AggState::Count(1))].as_slice())
        );
        assert!(!c.next());
    }

    #[test]
    fn hash_agg_cursor_empty_rewind_reports_no_row() {
        let mut c = HashAggCursor::new(vec![group_key(0)]);
        assert!(!c.rewind());
        assert!(c.current_blob().is_none());
    }

    fn asc_key(index: usize) -> SortKeyColumn {
        SortKeyColumn {
            index,
            descending: false,
            collation: super::super::value::Collation::Binary,
            nulls_first: false,
        }
    }

    #[test]
    fn in_memory_index_cursor_seek_eq_finds_an_exact_key() {
        let mut c = InMemoryIndexCursor::new(vec![asc_key(0)]);
        c.insert(1, vec![Value::Integer(10)]);
        c.insert(2, vec![Value::Integer(20)]);
        assert!(c.seek_index_eq(&[Value::Integer(20)], &[]));
        assert_eq!(c.idx_rowid(), Some(2));
        assert!(!c.seek_index_eq(&[Value::Integer(99)], &[]));
    }

    #[test]
    fn in_memory_index_cursor_seek_ge_positions_at_the_first_not_less_key() {
        let mut c = InMemoryIndexCursor::new(vec![asc_key(0)]);
        c.insert(1, vec![Value::Integer(10)]);
        c.insert(2, vec![Value::Integer(30)]);
        assert!(c.seek_index_ge(&[Value::Integer(20)], &[]));
        assert_eq!(c.column(0), Value::Integer(30));
        assert!(!c.seek_index_ge(&[Value::Integer(999)], &[]));
    }

    #[test]
    fn in_memory_index_cursor_idx_compare_reports_current_vs_given_key() {
        let mut c = InMemoryIndexCursor::new(vec![asc_key(0)]);
        c.insert(1, vec![Value::Integer(10)]);
        c.rewind();
        assert_eq!(
            c.idx_compare(&[Value::Integer(5)], &[]),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            c.idx_compare(&[Value::Integer(10)], &[]),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            c.idx_compare(&[Value::Integer(15)], &[]),
            Some(std::cmp::Ordering::Less)
        );
    }

    #[test]
    fn in_memory_index_cursor_satisfies_the_conformance_suite() {
        super::super::cursor_conformance::assert_forward_scan_matches_insertion_order(|| {
            InMemoryIndexCursor::new(vec![asc_key(0)])
        });
    }

    #[test]
    fn auto_index_cursor_seeks_and_reads_rowid() {
        let mut c = AutoIndexCursor::new();
        assert!(c.auto_index_insert(vec![Value::Integer(1)], &[], 100));
        assert!(c.auto_index_insert(vec![Value::Integer(2)], &[], 200));
        assert!(c.auto_index_seek(&[Value::Integer(2)], &[]));
        assert_eq!(c.rowid(), 200);
        assert!(!c.auto_index_seek(&[Value::Integer(99)], &[]));
    }

    #[test]
    fn auto_index_cursor_next_walks_duplicate_keys_only() {
        let mut c = AutoIndexCursor::new();
        assert!(c.auto_index_insert(vec![Value::Integer(1)], &[], 100));
        assert!(c.auto_index_insert(vec![Value::Integer(1)], &[], 200));
        assert!(c.auto_index_insert(vec![Value::Integer(2)], &[], 300));
        assert!(c.auto_index_seek(&[Value::Integer(1)], &[]));
        let first = c.rowid();
        assert!(c.auto_index_next());
        let second = c.rowid();
        assert_eq!(
            [first, second]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
        assert!(!c.auto_index_next());
    }

    #[test]
    fn pseudo_cursor_serves_the_decoded_row_repeatedly() {
        let blob = super::super::record::encode_record(
            &[Value::Integer(7), Value::Text("x".to_string().into())],
            TextEncoding::Utf8,
        );
        let mut c = PseudoCursor::new(&blob);
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Integer(7));
        assert_eq!(c.column(1), Value::Text("x".to_string().into()));
        assert!(!c.next());
        assert!(c.rewind());
        assert_eq!(c.column(0), Value::Integer(7));
    }

    #[test]
    fn hash_agg_cursor_multi_key_groups_on_every_key_column() {
        let mut c = HashAggCursor::new(vec![group_key(0), group_key(1)]);
        assert!(c.hash_agg_find(make_row(&[Value::Integer(1), Value::Integer(1)])));
        assert!(c.hash_agg_find(make_row(&[Value::Integer(1), Value::Integer(2)])));
        assert!(c.hash_agg_find(make_row(&[Value::Integer(1), Value::Integer(1)])));
        c.hash_agg_step(
            0,
            "count",
            &[Value::Integer(1)],
            super::super::value::Collation::Binary,
        )
        .unwrap();
        assert!(c.rewind());
        assert_eq!(c.column(1), Value::Integer(1));
        assert!(c.next());
        assert_eq!(c.column(1), Value::Integer(2));
        assert!(!c.next());
    }

    /// MC/DC vector (obligation `cursor_698`, `EphemeralIndexCursor::
    /// ephemeral_idx_insert`'s decision `!entries.contains_key(&encoded)
    /// && entries.len() >= MAX_EPHEMERAL_ROWS`): both leaves true --
    /// a genuinely new key once the table is already at capacity is
    /// rejected. Constructs the cursor with `entries` pre-filled to
    /// `MAX_EPHEMERAL_ROWS` directly (rather than inserting a million
    /// real rows through the public API) since only `entries.len()`
    /// matters here, not the entries' actual content.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__cursor_698__v1_new_key_at_capacity_is_rejected() {
        let mut c = EphemeralIndexCursor {
            entries: (0..MAX_EPHEMERAL_ROWS)
                .map(|i| (i.to_le_bytes().to_vec(), Vec::new()))
                .collect(),
            last_key: None,
        };
        let key = [Value::Integer(-1)];
        let collations = [Collation::Binary];
        assert_eq!(
            c.ephemeral_idx_insert(&key, &collations, vec![Value::Integer(-1)]),
            Some(false)
        );
    }

    /// MC/DC vector (obligation `cursor_698`): leaf A (`!contains_key`)
    /// true, leaf B (`len() >= MAX_EPHEMERAL_ROWS`) false -- an ordinary
    /// insert under capacity succeeds. Independence pair for B against
    /// `mcdc__cursor_698__v1_new_key_at_capacity_is_rejected`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__cursor_698__v2_new_key_under_capacity_is_accepted() {
        let mut c = EphemeralIndexCursor::new();
        let key = [Value::Integer(1)];
        let collations = [Collation::Binary];
        assert_eq!(
            c.ephemeral_idx_insert(&key, &collations, vec![Value::Integer(1)]),
            Some(true)
        );
    }

    /// MC/DC vector (obligation `cursor_698`): leaf A false -- an
    /// already-present key is accepted (an update, not a new row) even
    /// with the table at capacity, since leaf B is never reached.
    /// Independence pair for A against
    /// `mcdc__cursor_698__v1_new_key_at_capacity_is_rejected`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__cursor_698__v3_existing_key_at_capacity_is_still_accepted() {
        let key = [Value::Integer(-1)];
        let collations = [Collation::Binary];
        let encoded = encode_key(&key, &collations);
        let mut entries: BTreeMap<Vec<u8>, Vec<Value>> = (0..MAX_EPHEMERAL_ROWS)
            .map(|i| (i.to_le_bytes().to_vec(), Vec::new()))
            .collect();
        entries.insert(encoded, vec![Value::Integer(-1)]);
        let mut c = EphemeralIndexCursor {
            entries,
            last_key: None,
        };
        assert_eq!(
            c.ephemeral_idx_insert(&key, &collations, vec![Value::Integer(-2)]),
            Some(true)
        );
    }
}
