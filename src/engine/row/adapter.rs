// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! db-storage behind db-core's `vm::row` hooks (t-rust-db/sqlite-rs#18):
//! the consumer-side adapter ADR 0008 places here rather than in either
//! shared crate. Semantics are those of this crate's former
//! `src/vdbe/cursor.rs` (Lab271/sqlite-rs), which db-core's dispatcher
//! now expects (db-core#134).
//!
//! This is the one place the VDBE meets the file format, so it is the one
//! `src/vdbe` file that names the pager — always by full path, never through
//! an import of the pager module (`tests/unit/layer_isolation.rs`).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use crate::value::Collation;
use crate::vm::row::cursor_factory::{CursorFactory, CursorFactoryError};
use crate::vm::row::schema_storage::{SchemaStorage, SchemaStorageError};
use crate::vm::row::transaction::{Transaction, TransactionError};
use crate::vm::row::{
    compare, AnalyzeTarget, Cursor, SortKeyColumn, JOURNAL_MODE_WAL, SYNCHRONOUS_NORMAL,
    SYNCHRONOUS_OFF,
};

use crate::storage::row::btree::{self, IndexCursor, IndexRow, Payload, TableCursor};
use crate::storage::row::header::{DatabaseHeader, JournalMode, SynchronousMode};
use crate::storage::row::record::{decode_record, decode_serial_value, parse_header_into, Value};
use crate::storage::row::vfs::{PageError, PageSource};

type SharedPager = Rc<RefCell<crate::storage::row::pager::Pager>>;

/// A positioned row's header entries -- `(serial_type, body_offset)` per
/// column, from [`parse_header_into`].
type HeaderEntries = Vec<(u64, usize)>;

fn storage_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ---------------------------------------------------------------- tables

/// A table b-tree cursor (`OpenRead`/`OpenWrite` with `p5 = 0`).
pub struct TableCursorAdapter {
    cursor: TableCursor<Rc<dyn PageSource>>,
    source: Rc<dyn PageSource>,
    writer: Option<SharedPager>,
    header: DatabaseHeader,
    root_page: u32,
    current_rowid: Option<i64>,
    /// The current row's payload, set on the first `column()`/`payload()`
    /// after positioning. `column()` takes `&self` (the `Cursor` trait's
    /// signature), so this is filled in through a `RefCell` rather than
    /// up front in `position()`.
    payload: RefCell<Option<Payload>>,
    /// Header entries (serial type, body offset per column) for the
    /// current row, parsed once alongside `payload`. Caching offsets
    /// rather than decoded `Value`s means the header is walked once per
    /// row regardless of how many columns a projection reads
    /// (db-core#485), while `column()` still only decodes the one column
    /// body it's asked for. Kept as a separate field from `payload` (not
    /// folded into one `Option<(Payload, HeaderEntries)>`) so its backing
    /// allocation survives across rows: `position()` only clears
    /// `payload` to mark "not parsed"; `ensure_cached()` reuses this
    /// `Vec`'s capacity via `parse_header_into`'s `clear()` instead of
    /// allocating a fresh one every row (db-core#533).
    entries: RefCell<HeaderEntries>,
}

impl TableCursorAdapter {
    fn new(
        source: Rc<dyn PageSource>,
        writer: Option<SharedPager>,
        header: DatabaseHeader,
        root_page: u32,
    ) -> Self {
        TableCursorAdapter {
            cursor: TableCursor::new(Rc::clone(&source), &header, root_page),
            source,
            writer,
            header,
            root_page,
            current_rowid: None,
            payload: RefCell::new(None),
            entries: RefCell::new(Vec::new()),
        }
    }

    fn position(&mut self, rowid: Result<Option<i64>, btree::BtreeError>) -> bool {
        self.current_rowid = rowid.ok().flatten();
        *self.payload.borrow_mut() = None;
        self.current_rowid.is_some()
    }

    /// Ensures `payload`/`entries` hold the current row's payload and
    /// header offsets, parsing the header at most once per positioned
    /// row. `entries`' backing allocation is reused across rows.
    fn ensure_cached(&self) -> Option<()> {
        if self.payload.borrow().is_some() {
            return Some(());
        }
        self.current_rowid?;
        let payload = self.cursor.current_payload().ok()?;
        parse_header_into(&payload, &mut self.entries.borrow_mut()).ok()?;
        *self.payload.borrow_mut() = Some(payload);
        Some(())
    }

    fn max_rowid(&self) -> i64 {
        let mut probe = TableCursor::new(Rc::clone(&self.source), &self.header, self.root_page);
        probe.last().ok().flatten().unwrap_or(0)
    }
}

impl Cursor for TableCursorAdapter {
    fn rewind(&mut self) -> bool {
        let r = self.cursor.first();
        self.position(r)
    }

    fn next(&mut self) -> bool {
        let r = self.cursor.next();
        self.position(r)
    }

    fn last(&mut self) -> bool {
        let r = self.cursor.last();
        self.position(r)
    }

    fn prev(&mut self) -> bool {
        let r = self.cursor.prev();
        self.position(r)
    }

    fn seek(&mut self, rowid: i64) -> bool {
        let r = self
            .cursor
            .seek(rowid)
            .map(|found| found.filter(|f| *f == rowid));
        self.position(r)
    }

    fn column(&self, col: usize) -> Option<Value> {
        // The header (serial types/offsets) is parsed at most once per
        // positioned row via `ensure_cached`, regardless of how many
        // columns a projection reads (db-core#485); only the requested
        // column's body is decoded here. `None` is "no current row"
        // (db-core#231); a column index past the record's end is
        // `Some(Null)`, SQLite's short-record rule.
        self.ensure_cached()?;
        let payload = self.payload.borrow();
        let payload = payload.as_ref()?;
        let entries = self.entries.borrow();
        match entries.get(col) {
            Some(&(serial_type, offset)) => {
                let (value, _) =
                    decode_serial_value(serial_type, payload, offset, self.header.text_encoding)
                        .ok()?;
                Some(value)
            }
            None => Some(Value::Null),
        }
    }

    fn rowid(&self) -> Option<i64> {
        self.current_rowid
    }

    fn payload(&self) -> Option<Rc<[u8]>> {
        self.ensure_cached()?;
        let payload = self.payload.borrow();
        let payload = payload.as_ref()?;
        Some(Rc::from(&payload[..]))
    }

    fn insert_payload(&mut self, rowid: i64, payload: &Rc<[u8]>) -> Option<bool> {
        let writer = self.writer.as_ref()?;
        let ok = btree::insert_row(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            rowid,
            payload,
        )
        .is_ok();
        *self.payload.borrow_mut() = None;
        Some(ok)
    }

    fn update_payload(&mut self, rowid: i64, payload: &Rc<[u8]>) -> Option<bool> {
        let writer = self.writer.as_ref()?;
        let ok = btree::update_row(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            rowid,
            payload,
        )
        .is_ok();
        // The rowid is unchanged (codegen only emits `Update` for that
        // case); only the cached payload/header, not `current_rowid`,
        // needs invalidating.
        *self.payload.borrow_mut() = None;
        Some(ok)
    }

    fn insert(&mut self, rowid: i64, values: Vec<Value>) -> bool {
        let payload =
            crate::storage::row::record::encode_record(&values, self.header.text_encoding);
        self.insert_payload(rowid, &Rc::from(payload))
            .unwrap_or(false)
    }

    fn delete(&mut self) -> bool {
        let (Some(writer), Some(rowid)) = (self.writer.as_ref(), self.current_rowid) else {
            return false;
        };
        let ok = btree::delete_row(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            rowid,
        )
        .is_ok();
        self.current_rowid = None;
        *self.payload.borrow_mut() = None;
        ok
    }

    fn next_rowid(&self) -> i64 {
        self.max_rowid().saturating_add(1)
    }

    fn count(&self) -> Option<i64> {
        btree::count_table_rows(&self.source, self.root_page).ok()
    }
}

// --------------------------------------------------------------- indexes

/// A secondary-index b-tree cursor (`OpenRead`/`OpenWrite` with `p5 = 1`).
pub struct IndexCursorAdapter {
    cursor: IndexCursor<Rc<dyn PageSource>>,
    writer: Option<SharedPager>,
    header: DatabaseHeader,
    root_page: u32,
    /// The entry most recently positioned on and its decoded key record
    /// (key columns then the trailing rowid), `None` after a miss.
    current: Option<(IndexRow, Vec<Value>)>,
}

impl IndexCursorAdapter {
    fn new(
        source: Rc<dyn PageSource>,
        writer: Option<SharedPager>,
        header: DatabaseHeader,
        root_page: u32,
    ) -> Self {
        IndexCursorAdapter {
            cursor: IndexCursor::new(source, header.usable_page_size(), root_page),
            writer,
            header,
            root_page,
            current: None,
        }
    }

    fn set_current(&mut self, row: Result<Option<IndexRow>, btree::BtreeError>) -> bool {
        self.current = row.ok().flatten().and_then(|row| {
            let values = decode_record(&row.payload, self.header.text_encoding).ok()?;
            Some((row, values))
        });
        self.current.is_some()
    }

    /// sqlite-rs `decode_leading_columns`: the entry's first `probe.len()`
    /// columns compared to `probe` under `collations`, or `None` when the
    /// entry has no trailing rowid column beyond them.
    fn compare_leading(&self, probe: &[Value], collations: &[Collation]) -> Option<Ordering> {
        let (_, values) = self.current.as_ref()?;
        if values.len() <= probe.len() {
            return None;
        }
        Some(
            values
                .iter()
                .zip(probe.iter())
                .zip(
                    collations
                        .iter()
                        .chain(std::iter::repeat(&Collation::Binary)),
                )
                .map(|((k, p), &c)| compare(k, p, c))
                .find(|o| !o.is_eq())
                .unwrap_or(Ordering::Equal),
        )
    }
}

impl Cursor for IndexCursorAdapter {
    fn rewind(&mut self) -> bool {
        let r = self.cursor.first();
        self.set_current(r)
    }

    fn next(&mut self) -> bool {
        let r = self.cursor.next();
        self.set_current(r)
    }

    fn last(&mut self) -> bool {
        let r = self.cursor.last();
        self.set_current(r)
    }

    fn prev(&mut self) -> bool {
        let r = self.cursor.prev();
        self.set_current(r)
    }

    fn column(&self, col: usize) -> Option<Value> {
        let (_, values) = self.current.as_ref()?;
        Some(values.get(col).cloned().unwrap_or(Value::Null))
    }

    fn rowid(&self) -> Option<i64> {
        self.idx_rowid()
    }

    fn idx_rowid(&self) -> Option<i64> {
        let (_, values) = self.current.as_ref()?;
        match values.last()? {
            Value::Integer(rowid) => Some(*rowid),
            _ => None,
        }
    }

    fn seek_index_eq(&mut self, key: &[Value], collations: &[Collation]) -> bool {
        let r = self.cursor.seek(key, self.header.text_encoding);
        if !self.set_current(r) {
            return false;
        }
        let matched = self.compare_leading(key, collations) == Some(Ordering::Equal);
        if !matched {
            self.current = None;
        }
        matched
    }

    fn seek_index_ge(&mut self, key: &[Value], _collations: &[Collation]) -> bool {
        let r = self.cursor.seek(key, self.header.text_encoding);
        self.set_current(r)
    }

    fn idx_compare(&self, key: &[Value], collations: &[Collation]) -> Option<Ordering> {
        self.current.as_ref()?;
        // No trailing rowid beyond the probe: sqlite-rs treats it as
        // "not greater".
        Some(
            self.compare_leading(key, collations)
                .unwrap_or(Ordering::Equal),
        )
    }

    fn idx_insert(&mut self, key: Vec<Value>) -> bool {
        let Some(writer) = self.writer.as_ref() else {
            return false;
        };
        btree::insert_entry(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            &key,
            self.header.text_encoding,
        )
        .is_ok()
    }

    fn idx_delete(&mut self, key: &[Value]) -> bool {
        let Some(writer) = self.writer.as_ref() else {
            return false;
        };
        btree::delete_entry(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            key,
            self.header.text_encoding,
        )
        .is_ok()
    }
}

// --------------------------------------------------------------- factory

/// Type-erases any shared page source into the `Rc<dyn PageSource>` this
/// module stores. Kept here — the one `dyn` storage boundary (ADR-0013,
/// `MVL_LIMIT_EXCLUDE`) — so `src/vdbe.rs` and every caller stay generic
/// and limit-clean. The extra `Rc` hop is one pointer chase per page
/// read; `P` may itself be unsized (an `Rc<dyn PageSource>` a caller
/// already holds).
struct ErasedSource<P: ?Sized>(Rc<P>);

impl<P: PageSource + ?Sized> PageSource for ErasedSource<P> {
    fn read_page(&self, page_num: u32) -> Result<Rc<[u8]>, PageError> {
        self.0.read_page(page_num)
    }
}

fn erase<P: PageSource + ?Sized + 'static>(source: Rc<P>) -> Rc<dyn PageSource> {
    Rc::new(ErasedSource(source))
}

/// Resolves `OpenRead`/`OpenWrite` root pages to the adapters above.
pub struct StorageFactory {
    source: Rc<dyn PageSource>,
    writer: Option<SharedPager>,
    header: DatabaseHeader,
}

impl StorageFactory {
    /// Cursors over `source` only; `OpenWrite` is refused.
    pub fn read_only<P: PageSource + ?Sized + 'static>(
        source: Rc<P>,
        header: DatabaseHeader,
    ) -> Self {
        StorageFactory {
            source: erase(source),
            writer: None,
            header,
        }
    }

    /// Cursors that read through `source` and write through `pager` (the
    /// same `Rc<RefCell<Pager>>` unsized into `source`, ADR-0017).
    pub fn writable<P: PageSource + ?Sized + 'static>(
        source: Rc<P>,
        pager: SharedPager,
        header: DatabaseHeader,
    ) -> Self {
        StorageFactory {
            source: erase(source),
            writer: Some(pager),
            header,
        }
    }
}

impl CursorFactory for StorageFactory {
    fn open_read(&mut self, root: u32) -> Result<Box<dyn Cursor>, CursorFactoryError> {
        Ok(Box::new(TableCursorAdapter::new(
            Rc::clone(&self.source),
            self.writer.clone(),
            self.header,
            root,
        )))
    }

    fn open_write(&mut self, root: u32) -> Result<Box<dyn Cursor>, CursorFactoryError> {
        if self.writer.is_none() {
            return Err(CursorFactoryError(
                "OpenWrite: this connection is read-only (no writable pager)".to_string(),
            ));
        }
        self.open_read(root)
    }

    fn open_index(
        &mut self,
        root: u32,
        _key: &[SortKeyColumn],
    ) -> Result<Box<dyn Cursor>, CursorFactoryError> {
        Ok(Box::new(IndexCursorAdapter::new(
            Rc::clone(&self.source),
            self.writer.clone(),
            self.header,
            root,
        )))
    }
}

// ----------------------------------------------------------- transaction

/// `Transaction`/`AutoCommit`/`SetJournalMode`/`Synchronous`/
/// `IntegrityCheck` against the pager (former `control.rs`/`pragma.rs`).
pub struct PagerTransaction {
    source: Rc<dyn PageSource>,
    writer: Option<SharedPager>,
    header: DatabaseHeader,
}

impl PagerTransaction {
    /// A read-only connection: BEGIN/COMMIT only toggle state; integrity
    /// check still reads `source`.
    pub fn read_only<P: PageSource + ?Sized + 'static>(
        source: Rc<P>,
        header: DatabaseHeader,
    ) -> Self {
        PagerTransaction {
            source: erase(source),
            writer: None,
            header,
        }
    }

    /// A writable connection over `pager`.
    pub fn writable<P: PageSource + ?Sized + 'static>(
        source: Rc<P>,
        pager: SharedPager,
        header: DatabaseHeader,
    ) -> Self {
        PagerTransaction {
            source: erase(source),
            writer: Some(pager),
            header,
        }
    }
}

impl Transaction for PagerTransaction {
    fn begin(&mut self, mode: i32) -> Result<(), TransactionError> {
        let Some(writer) = self.writer.as_ref() else {
            return Ok(());
        };
        let mut pager = writer.borrow_mut();
        match mode {
            crate::vm::row::TRANSACTION_MODE_IMMEDIATE => pager.begin_immediate(),
            crate::vm::row::TRANSACTION_MODE_EXCLUSIVE => pager.begin_exclusive(),
            _ => Ok(()),
        }
        .map_err(|e| TransactionError(storage_err(e)))
    }

    fn commit(&mut self) -> Result<(), TransactionError> {
        match self.writer.as_ref() {
            Some(writer) => writer
                .borrow_mut()
                .flush()
                .map_err(|e| TransactionError(storage_err(e))),
            None => Ok(()),
        }
    }

    fn rollback(&mut self) -> Result<(), TransactionError> {
        match self.writer.as_ref() {
            Some(writer) => writer
                .borrow_mut()
                .rollback()
                .map_err(|e| TransactionError(storage_err(e))),
            None => Ok(()),
        }
    }

    fn set_journal_mode(&mut self, mode: i32) -> Result<(), TransactionError> {
        let Some(writer) = self.writer.as_ref() else {
            return Ok(());
        };
        let mode = if mode == JOURNAL_MODE_WAL {
            JournalMode::Wal
        } else {
            JournalMode::Legacy
        };
        writer
            .borrow_mut()
            .set_journal_mode(mode)
            .map_err(|e| TransactionError(storage_err(e)))
    }

    fn synchronous(&self) -> Option<i32> {
        let writer = self.writer.as_ref()?;
        Some(writer.borrow().synchronous() as i32)
    }

    fn set_synchronous(&mut self, level: i32) -> Result<(), TransactionError> {
        if let Some(writer) = self.writer.as_ref() {
            let mode = match level {
                SYNCHRONOUS_OFF => SynchronousMode::Off,
                SYNCHRONOUS_NORMAL => SynchronousMode::Normal,
                _ => SynchronousMode::Full,
            };
            writer.borrow_mut().set_synchronous(mode);
        }
        Ok(())
    }

    fn integrity_check(&mut self, quick: bool) -> Option<Result<Vec<String>, TransactionError>> {
        Some(Ok(crate::storage::row::integrity::run_integrity_check(
            Rc::clone(&self.source),
            &self.header,
            quick,
        )))
    }
}

// --------------------------------------------------------- schema writes

/// DDL and `ANALYZE` against `sqlite_master`/`sqlite_stat1`/
/// `sqlite_sequence` (former `cursor.rs::{create_table, …, analyze}`).
pub struct BtreeSchemaStorage {
    pager: SharedPager,
    header: DatabaseHeader,
}

impl BtreeSchemaStorage {
    /// Schema writes through `pager`.
    pub fn new(pager: SharedPager, header: DatabaseHeader) -> Self {
        BtreeSchemaStorage { pager, header }
    }
}

fn schema_err(e: impl std::fmt::Display) -> SchemaStorageError {
    SchemaStorageError(e.to_string())
}

/// `ANALYZE`'s index statistic: `(entries, avg_eq)` where `avg_eq` is the
/// average number of entries sharing a leading-column value.
fn count_index_entries_and_avg_eq(
    pager: &crate::storage::row::pager::Pager,
    header: &DatabaseHeader,
    root_page: u32,
) -> Result<(u64, u64), SchemaStorageError> {
    let mut cursor = IndexCursor::new(pager, header.usable_page_size(), root_page);
    let mut total = 0u64;
    let mut distinct_groups = 0u64;
    let mut prev_leading: Option<Value> = None;
    let mut row = cursor.first().map_err(schema_err)?;
    while let Some(r) = row {
        let values = decode_record(&r.payload, header.text_encoding).map_err(schema_err)?;
        let leading = values.first().cloned();
        if prev_leading.as_ref() != leading.as_ref() {
            distinct_groups = distinct_groups.saturating_add(1);
            prev_leading = leading;
        }
        total = total.saturating_add(1);
        row = cursor.next().map_err(schema_err)?;
    }
    Ok((total, total.checked_div(distinct_groups).unwrap_or(0)))
}

impl SchemaStorage for BtreeSchemaStorage {
    fn create_table_root(&mut self) -> Result<u32, SchemaStorageError> {
        btree::create_empty_table_root(&mut self.pager.borrow_mut()).map_err(schema_err)
    }

    fn create_index_root(&mut self) -> Result<u32, SchemaStorageError> {
        btree::create_empty_index_root(&mut self.pager.borrow_mut()).map_err(schema_err)
    }

    fn populate_index(
        &mut self,
        index_root: u32,
        table_root: u32,
        column_indices: &[usize],
    ) -> Result<(), SchemaStorageError> {
        btree::populate_index_from_table(
            &mut self.pager.borrow_mut(),
            &self.header,
            table_root,
            index_root,
            column_indices,
        )
        .map_err(schema_err)
    }

    fn free_root(&mut self, root: u32) -> Result<(), SchemaStorageError> {
        btree::free_btree_pages(&mut self.pager.borrow_mut(), &self.header, root)
            .map_err(schema_err)
    }

    fn insert_master_row(
        &mut self,
        kind: &str,
        name: &str,
        tbl_name: &str,
        root_page: u32,
        sql: &str,
    ) -> Result<(), SchemaStorageError> {
        btree::insert_master_row(
            &mut self.pager.borrow_mut(),
            &self.header,
            &btree::MasterEntry {
                kind: kind.to_string(),
                name: name.to_string(),
                tbl_name: tbl_name.to_string(),
                rootpage: root_page,
                sql: sql.to_string(),
            },
        )
        .map_err(schema_err)
    }

    fn delete_master_row(&mut self, name: &str) -> Result<(), SchemaStorageError> {
        btree::delete_master_row(&mut self.pager.borrow_mut(), &self.header, name)
            .map_err(schema_err)
    }

    fn bump_schema_cookie(&mut self) -> Result<(), SchemaStorageError> {
        btree::bump_schema_cookie(&mut self.pager.borrow_mut())
            .map(|_| ())
            .map_err(schema_err)
    }

    fn write_stat1(&mut self, target: &AnalyzeTarget) -> Result<(), SchemaStorageError> {
        let header = self.header;
        let mut pager = self.pager.borrow_mut();
        let stat1_root =
            btree::ensure_sqlite_stat1_table(&mut pager, &header).map_err(schema_err)?;
        btree::delete_stat1_rows_for_table(&mut pager, &header, stat1_root, &target.table_name)
            .map_err(schema_err)?;
        let row_count =
            btree::count_table_rows(&*pager, target.table_root_page).map_err(schema_err)?;
        btree::insert_stat1_row(
            &mut pager,
            &header,
            stat1_root,
            &target.table_name,
            None,
            &row_count.to_string(),
        )
        .map_err(schema_err)?;
        for index in &target.indexes {
            let (idx_rows, avg_eq) =
                count_index_entries_and_avg_eq(&pager, &header, index.root_page)?;
            btree::insert_stat1_row(
                &mut pager,
                &header,
                stat1_root,
                &target.table_name,
                Some(&index.index_name),
                &format!("{idx_rows} {avg_eq}"),
            )
            .map_err(schema_err)?;
        }
        Ok(())
    }

    fn autoincrement_rowid(
        &mut self,
        table: &str,
        max_from_table: i64,
    ) -> Result<i64, SchemaStorageError> {
        let header = self.header;
        let mut pager = self.pager.borrow_mut();
        let seq_root =
            btree::ensure_sqlite_sequence_table(&mut pager, &header).map_err(schema_err)?;
        let mut tracked_seq = 0i64;
        {
            let mut seq_cursor = TableCursor::new(&*pager, &header, seq_root);
            let mut row = seq_cursor.first_row().map_err(schema_err)?;
            while let Some(r) = row {
                let values = decode_record(&r.payload, header.text_encoding).map_err(schema_err)?;
                if let (Some(Value::Text(n)), Some(Value::Integer(seq))) =
                    (values.first(), values.get(1))
                {
                    if &**n == table {
                        tracked_seq = *seq;
                        break;
                    }
                }
                row = seq_cursor.next_row().map_err(schema_err)?;
            }
        }
        let candidate = max_from_table.max(tracked_seq).saturating_add(1);
        btree::update_sequence(&mut pager, &header, table, candidate).map_err(schema_err)?;
        Ok(candidate)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new(label: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "db-core-adapter-{label}-{}-{}.db",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
            std::fs::copy(FIXTURE, &path).expect("copy fixture");
            TempDb(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).ok();
            std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
        }
    }

    /// `OpenWrite` on a read-only connection (`execute_with_db`) is refused
    /// by the factory — the former `Vm::writer("OpenWrite")` check.
    #[test]
    fn open_write_on_a_read_only_connection_is_refused() {
        let (vfs, header) = crate::storage::row::btree::test_minimal_db(512);
        let source: Rc<dyn PageSource> = Rc::new(
            crate::storage::row::vfs::VfsPageSource::open(
                &vfs,
                std::path::Path::new("/test.db"),
                512,
            )
            .unwrap(),
        );
        let mut factory = StorageFactory::read_only(source, header);
        assert!(factory.open_read(1).is_ok());
        assert!(factory.open_write(1).is_err());
    }

    /// `TableCursorAdapter::last`/`prev`/`column` (lazily-cached header) and
    /// `IndexCursorAdapter::last`/`prev`/`idx_compare`/`idx_delete`: a
    /// reverse table scan, a reverse index scan, and a delete through a
    /// secondary index, none of which the forward-scan-only tests
    /// elsewhere exercise.
    #[test]
    fn reverse_scans_and_indexed_delete_exercise_last_and_prev() {
        use crate::engine::{row::RowEngine, Engine};

        let db = TempDb::new("adapter-reverse-scan");
        let mut e = RowEngine::open(db.path()).unwrap();
        e.run_query(
            "CREATE TABLE rs(a INTEGER, b INTEGER); \
             CREATE INDEX rs_b ON rs(b); \
             INSERT INTO rs(a, b) VALUES (1, 10), (2, 20), (3, 30)",
        )
        .unwrap();

        // ORDER BY a DESC without an index -- exercises the table
        // cursor's last()/prev() in reverse, and column() parsing the
        // header lazily on an entry with nothing cached yet.
        let rows = e
            .run_query("SELECT a FROM rs ORDER BY a DESC")
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 3);

        // ORDER BY b DESC over the index -- the index cursor's own
        // last()/prev().
        let rows = e
            .run_query("SELECT b FROM rs ORDER BY b DESC")
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 3);

        // A ranged WHERE over the indexed column reaches idx_compare
        // (the "past the upper bound" check a range scan makes).
        let rows = e
            .run_query("SELECT a FROM rs WHERE b > 10 AND b < 30")
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1);

        // DELETE keyed by the indexed column reaches idx_delete.
        e.run_query("DELETE FROM rs WHERE b = 20").unwrap();
        let rows = e.run_query("SELECT count(*) FROM rs").unwrap().rows;
        assert_eq!(rows[0][0].to_string(), "2");
    }

    /// A `PageSource` wrapper counting `read_page` calls, to check that
    /// `TableCursorAdapter::column()` doesn't re-fetch/re-decode a row's
    /// payload once per projected column (db-core#485).
    struct CountingSource {
        inner: Rc<dyn PageSource>,
        reads: RefCell<usize>,
    }

    impl PageSource for CountingSource {
        fn read_page(&self, page_num: u32) -> Result<Rc<[u8]>, PageError> {
            *self.reads.borrow_mut() += 1;
            self.inner.read_page(page_num)
        }
    }

    /// Reading every column of a row must not cost more page reads than
    /// reading just one -- if `column()` re-fetched/re-decoded the payload
    /// per requested column, each extra column read would cost at least
    /// one more `read_page` per row (db-core#485).
    #[test]
    fn reading_more_columns_of_a_row_does_not_multiply_page_reads() {
        use crate::engine::{row::RowEngine, Engine};
        use crate::storage::row::vfs::{UnixVfs, VfsPageSource};

        // A large text column forces the row's payload past the b-tree
        // cell's local-storage limit onto an overflow-page chain, so
        // reassembling it (`TableCursor::current_payload`) costs multiple
        // `read_page` calls -- exactly what would multiply per projected
        // column under the old always-re-decode `column()`.
        let big = "x".repeat(20_000);
        let db = TempDb::new("adapter-page-read-count");
        let root_page = {
            let mut e = RowEngine::open(db.path()).unwrap();
            e.run_query(&format!(
                "CREATE TABLE pc(a INTEGER, b INTEGER, c INTEGER, d INTEGER, big TEXT); \
                 INSERT INTO pc(a, b, c, d, big) VALUES (1, 10, 100, 1000, '{big}')",
            ))
            .unwrap();
            e.table_schema("pc").unwrap().root_page
        };

        let header = {
            let e = RowEngine::open(db.path()).unwrap();
            e.with_storage(|_pager, header| *header)
        };

        fn count_reads(
            db_path: &std::path::Path,
            header: DatabaseHeader,
            root_page: u32,
            cols: usize,
        ) -> usize {
            let inner: Rc<dyn PageSource> =
                Rc::new(VfsPageSource::open(&UnixVfs, db_path, header.page_size).unwrap());
            let counting = Rc::new(CountingSource {
                inner,
                reads: RefCell::new(0),
            });
            let mut factory = StorageFactory::read_only(Rc::clone(&counting), header);
            let mut cursor = factory.open_read(root_page).unwrap();
            assert!(cursor.rewind());
            for col in 0..cols {
                assert!(cursor.column(col).is_some());
            }
            let reads = *counting.reads.borrow();
            reads
        }

        let one_col = count_reads(db.path(), header, root_page, 1);
        let five_col = count_reads(db.path(), header, root_page, 5);
        assert!(
            one_col > 1,
            "the overflowing text column should force more than one page read: got {one_col}"
        );
        assert_eq!(
            one_col, five_col,
            "reading 5 columns instead of 1 should not change the page-read count"
        );
    }

    /// `PagerTransaction::begin` (IMMEDIATE/EXCLUSIVE), `set_journal_mode`,
    /// `synchronous`/`set_synchronous`, and `integrity_check` -- all
    /// reached only through real PRAGMA/BEGIN SQL, never by the
    /// mock-hook opcode-dispatch tests in `vm/row/vm.rs`.
    #[test]
    fn pragma_and_explicit_transaction_modes_reach_the_real_pager_adapter() {
        use crate::engine::{row::RowEngine, Engine};

        let db = TempDb::new("adapter-pragma");
        let mut e = RowEngine::open(db.path()).unwrap();
        e.run_query("CREATE TABLE pt(a INTEGER)").unwrap();

        e.run_query("PRAGMA journal_mode = WAL").unwrap();
        e.run_query("PRAGMA synchronous = OFF").unwrap();
        let rows = e.run_query("PRAGMA synchronous").unwrap().rows;
        assert_eq!(rows[0][0].to_string(), "0");
        e.run_query("PRAGMA synchronous = NORMAL").unwrap();

        e.run_query("BEGIN IMMEDIATE; INSERT INTO pt VALUES (1); COMMIT")
            .unwrap();
        e.run_query("BEGIN EXCLUSIVE; INSERT INTO pt VALUES (2); COMMIT")
            .unwrap();

        let rows = e.run_query("PRAGMA integrity_check").unwrap().rows;
        assert_eq!(rows[0][0].to_string(), "ok");
    }

    /// `BtreeSchemaStorage::create_index_root`/`populate_index`/
    /// `free_root`/`write_stat1` (including its per-index
    /// `count_index_entries_and_avg_eq` loop): `CREATE INDEX` over a
    /// non-empty table, `ANALYZE` of that table (an index present this
    /// time, unlike the existing empty-table ANALYZE tests), and
    /// `DROP INDEX` to free the root.
    #[test]
    fn create_index_analyze_and_drop_index_exercise_schema_storage() {
        use crate::engine::{row::RowEngine, Engine};

        let db = TempDb::new("adapter-schema-storage");
        let mut e = RowEngine::open(db.path()).unwrap();
        e.run_query(
            "CREATE TABLE ss(a INTEGER, b INTEGER); \
             INSERT INTO ss(a, b) VALUES (1, 1), (2, 1), (3, 2)",
        )
        .unwrap();
        // CREATE INDEX on a non-empty table: create_index_root + populate_index.
        e.run_query("CREATE INDEX ss_b ON ss(b)").unwrap();
        // ANALYZE with a real index present: write_stat1's per-index loop.
        e.run_query("ANALYZE ss").unwrap();
        // DROP INDEX: free_root.
        e.run_query("DROP INDEX ss_b").unwrap();
    }
}
