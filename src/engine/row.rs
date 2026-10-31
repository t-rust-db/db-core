// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The row [`Engine`]: a SQLite-format file driven through
//! `storage::row` (pager, b-tree, header) and `vm::row` (#295, ADR 0017).
//!
//! This is the glue that used to live only in sqlite-rs (`src/vdbe.rs` +
//! `src/vdbe/adapter.rs` + `src/planner.rs`): the ADR 0008 boundary
//! implementors in [`adapter`] hand the pager to the `Vm` as
//! `CursorFactory`/`Transaction`/`SchemaStorage`; [`stats`] reads
//! `sqlite_stat1` for the planner; this module owns the session -- one
//! shared pager, a cached catalog that DDL invalidates, and the autocommit
//! flag carried across statements so `BEGIN` in one call and `COMMIT` in a
//! later one see each other.
//!
//! `run_query` is parse -> compile -> execute per statement, nothing more:
//! no `.dot` commands, no `PRAGMA` query shortcuts, no shell rendering.
//! Those are client concerns (sqlite-rs's REPL keeps its own).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::codegen::row::dispatch::{compile_select_statement, explain_select_statement};
use crate::codegen::row::planner::Stats;
use crate::codegen::row::{compile_statement, leading_keywords, result_column_names};
use crate::parser::ast::Select;
use crate::parser::row::error::{parse_select, ParseOutcome};
use crate::parser::row::tokenizer::split_statements;
use crate::schema::{TableSchema, ViewSchema};
use crate::storage::row::btree::TableCursor;
use crate::storage::row::header::{DatabaseHeader, HEADER_LEN};
use crate::storage::row::pager::Pager;
use crate::storage::row::schema::read_schema_and_views;
use crate::storage::row::vfs::{PageSource, UnixVfs, Vfs};
use crate::vm::row::{execute, explain, ConnectionState, Opcode, Program, Vm, OPFLAG_NCHANGE};

use super::{
    single_statement, Cell, ColumnInfo, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, TableInfo,
};

pub mod adapter;
pub mod stats;

use adapter::{BtreeSchemaStorage, PagerTransaction, StorageFactory};

type Catalog = (Vec<TableSchema>, Vec<ViewSchema>);

/// Journal mode of a database file made by [`RowEngine::create`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateJournal {
    /// Write-ahead log: what `PRAGMA journal_mode=WAL` leaves in the header.
    Wal,
    /// Rollback journal, deleted on commit (SQLite's default).
    Delete,
}

/// How [`RowEngine::create`] lays out a new database file (#915).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateOptions {
    /// Bytes per page: a power of two from 512 to 65536.
    pub page_size: u32,
    /// Journal mode written into the header.
    pub journal: CreateJournal,
    /// Bytes reserved at the end of every page (header byte 20), below
    /// `page_size`.
    pub reserved_bytes: u8,
}

impl Default for CreateOptions {
    fn default() -> Self {
        CreateOptions {
            page_size: crate::storage::row::header::DEFAULT_PAGE_SIZE,
            journal: CreateJournal::Delete,
            reserved_bytes: 0,
        }
    }
}

/// One open SQLite-format file.
pub struct RowEngine {
    path: PathBuf,
    header: DatabaseHeader,
    pager: Rc<RefCell<Pager>>,
    /// `None` after DDL until the next statement re-reads `sqlite_master`.
    catalog: RefCell<Option<Catalog>>,
    /// Carried across statements: outside `BEGIN ... COMMIT` every
    /// statement is its own transaction and flushes on a clean `Halt`.
    autocommit: bool,
    /// Carried across statements too: what `changes()`,
    /// `total_changes()` and `last_insert_rowid()` report (#651).
    connection: ConnectionState,
}

impl std::fmt::Debug for RowEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowEngine")
            .field("path", &self.path)
            .field("page_size", &self.header.page_size)
            .field("page_count", &self.header.page_count)
            .field("autocommit", &self.autocommit)
            .finish_non_exhaustive()
    }
}

impl RowEngine {
    /// Creates the database file `path` and opens it (#915): an empty
    /// schema in the layout `options` asks for, then `schema` -- zero or
    /// more `;`-separated statements, typically `CREATE TABLE`/`CREATE
    /// INDEX` -- run against it. Fails with [`ErrorKind::Open`], leaving
    /// the file untouched, when `path` already exists or `options` is not
    /// a valid layout. When `schema` fails the new file is removed again.
    ///
    /// # Errors
    /// [`ErrorKind::Open`] for an existing file, invalid options or an
    /// I/O failure; the error of the first `schema` statement that fails.
    pub fn create(path: &Path, options: CreateOptions, schema: &str) -> Result<Self, EngineError> {
        let page_size = options.page_size;
        if !page_size.is_power_of_two() || !(512..=65536).contains(&page_size) {
            return Err(EngineError::new(
                ErrorKind::Open,
                format!("page size {page_size} is not a power of two from 512 to 65536"),
            ));
        }
        if u32::from(options.reserved_bytes) >= page_size {
            return Err(EngineError::new(
                ErrorKind::Open,
                format!(
                    "{} reserved bytes leave no usable bytes in a {page_size}-byte page",
                    options.reserved_bytes
                ),
            ));
        }
        let mut page1 = DatabaseHeader::new_empty_page1(page_size);
        if let Some(byte) = page1.get_mut(20) {
            *byte = options.reserved_bytes;
        }
        if options.reserved_bytes > 0 {
            // The empty schema leaf's cell content area ends where the
            // reserved tail starts, not at the end of the page.
            let content_start =
                u16::try_from(page_size.saturating_sub(u32::from(options.reserved_bytes)))
                    .unwrap_or(0);
            for (slot, byte) in page1
                .get_mut(HEADER_LEN + 5..HEADER_LEN + 7)
                .into_iter()
                .flatten()
                .zip(content_start.to_be_bytes())
            {
                *slot = byte;
            }
        }
        if options.journal == CreateJournal::Wal {
            for byte in page1.get_mut(18..20).into_iter().flatten() {
                *byte = 2;
            }
        }
        // `create_new` fails on an existing file without touching it.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| EngineError::new(ErrorKind::Open, format!("{}: {e}", path.display())))?;
        let written = std::io::Write::write_all(&mut file, &page1)
            .and_then(|()| file.sync_all())
            .map_err(|e| EngineError::new(ErrorKind::Open, format!("{}: {e}", path.display())));
        drop(file);
        let engine = written
            .and_then(|()| <Self as Engine>::open(path))
            .and_then(|mut engine| {
                if schema.trim().is_empty() {
                    return Ok(engine);
                }
                engine.run_query(schema)?;
                Ok(engine)
            });
        if engine.is_err() {
            std::fs::remove_file(path).ok();
            for suffix in ["-wal", "-shm", "-journal"] {
                std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
            }
        }
        engine
    }

    /// The 100-byte header as read at open time. Not refreshed after
    /// writes; `stats()` reports it as-is.
    #[must_use]
    pub fn header(&self) -> &DatabaseHeader {
        &self.header
    }

    /// The file this engine was opened on.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn catalog(&self) -> Result<Catalog, EngineError> {
        if let Some(c) = self.catalog.borrow().as_ref() {
            return Ok(c.clone());
        }
        let loaded = {
            let borrowed = self.pager.borrow();
            let mut cursor = TableCursor::new(&*borrowed, &self.header, 1);
            read_schema_and_views(&mut cursor, self.header.text_encoding)
                .map_err(|e| EngineError::new(ErrorKind::Open, e))?
        };
        *self.catalog.borrow_mut() = Some(loaded.clone());
        Ok(loaded)
    }

    fn stats_by_table(&self, schemas: &[TableSchema]) -> HashMap<String, Stats> {
        let borrowed = self.pager.borrow();
        stats::load_stats(&*borrowed, &self.header, schemas)
    }

    fn parse_select_or_err(sql: &str) -> Result<Select, EngineError> {
        match parse_select(sql) {
            ParseOutcome::Accepted(select) => Ok(*select),
            ParseOutcome::Unsupported { message, span } => Err(EngineError::new(
                ErrorKind::Unsupported,
                format!(
                    "not yet supported (line {}, column {}): {message}",
                    span.line, span.column
                ),
            )),
            ParseOutcome::Invalid { message, span } => Err(EngineError::new(
                ErrorKind::Parse,
                format!(
                    "syntax error (line {}, column {}): {message}",
                    span.line, span.column
                ),
            )),
        }
    }

    /// `SELECT`, or `WITH ...` that parses as one (a `WITH` can also
    /// lead an `INSERT`/`UPDATE`/`DELETE`). Decides whether a statement
    /// runs as a query, with sqlite3's column names (#596).
    fn is_select(stmt: &str) -> bool {
        match leading_keywords(stmt).first().map(String::as_str) {
            Some("SELECT") => true,
            Some("WITH") => matches!(parse_select(stmt), ParseOutcome::Accepted(_)),
            _ => false,
        }
    }

    /// Conservative catalog dirty flag: any `CREATE`/`DROP`/`ALTER`
    /// invalidates, even one that fails or is a no-op. `ANALYZE` is
    /// included too -- its first run in a session creates `sqlite_stat1`
    /// (`write_stat1`'s `ensure_sqlite_stat1_table`) via the same
    /// sqlite_master-writing path as `CREATE TABLE`, so the cached
    /// catalog must be invalidated the same way or `stats_by_table`'s
    /// `load_stats` keeps failing to find that table and every
    /// `ANALYZE`-driven cost decision (auto-index probe, index-vs-scan
    /// choice) stays silently switched off for the rest of the session.
    /// `ROLLBACK` counts as well: it undoes the DDL of its transaction, and
    /// the cache would keep the schema the transaction left behind (#650).
    fn is_schema_changing(stmt: &str) -> bool {
        let head = stmt.trim_start();
        ["CREATE", "DROP", "ALTER", "ANALYZE", "ROLLBACK"]
            .iter()
            .any(|kw| {
                head.get(..kw.len())
                    .is_some_and(|h| h.eq_ignore_ascii_case(kw))
            })
    }

    fn read_only_vm(&self) -> Vm {
        let source: Rc<dyn PageSource> = Rc::clone(&self.pager) as Rc<dyn PageSource>;
        let mut vm = Vm::new();
        vm.set_text_encoding(self.header.text_encoding);
        vm.set_connection_state(self.connection);
        vm.set_cursor_factory(Box::new(StorageFactory::read_only(
            Rc::clone(&source),
            self.header,
        )));
        vm.set_transaction_hook(Box::new(PagerTransaction::read_only(source, self.header)));
        vm
    }

    fn writable_vm(&self) -> Vm {
        let pager = Rc::clone(&self.pager);
        let mut vm = Vm::new();
        vm.set_text_encoding(self.header.text_encoding);
        vm.set_autocommit(self.autocommit);
        vm.set_connection_state(self.connection);
        vm.set_cursor_factory(Box::new(StorageFactory::writable(
            Rc::clone(&pager),
            Rc::clone(&pager),
            self.header,
        )));
        vm.set_transaction_hook(Box::new(PagerTransaction::writable(
            Rc::clone(&pager),
            Rc::clone(&pager),
            self.header,
        )));
        vm.set_schema_storage(Box::new(BtreeSchemaStorage::new(pager, self.header)));
        vm
    }

    /// Runs `select`, parsed from `sql`; the column labels are sqlite3's
    /// (#596, `codegen::row::names`).
    fn run_select(
        &self,
        select: &Select,
        sql: &str,
        catalog: &Catalog,
    ) -> Result<QueryResult, EngineError> {
        let (schemas, views) = catalog;
        let stats_by_table = self.stats_by_table(schemas);
        let program = compile_select_statement(select, schemas, views, &stats_by_table)
            .map_err(|e| EngineError::new(ErrorKind::Compile, e))?;
        let columns = result_column_names(select, sql, schemas, views);
        let mut vm = self.read_only_vm();
        let rows =
            execute(&mut vm, &program).map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
        Ok(QueryResult {
            columns,
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(Cell::from).collect())
                .collect(),
            #[cfg(feature = "vm-stream")]
            scope_report: None,
        })
    }

    /// One write/DDL/transaction statement on the shared pager. Outside an
    /// explicit transaction a clean `Halt` flushes the pager (every
    /// statement is its own transaction); inside `BEGIN ... COMMIT` the
    /// `AutoCommit` opcode flushes instead.
    fn run_statement(&mut self, program: &Program) -> Result<QueryResult, EngineError> {
        let mut vm = self.writable_vm();
        let rows = match execute(&mut vm, program) {
            Ok(rows) => rows,
            Err(e) => {
                // A failed statement outside a transaction is undone as a
                // whole: rows it wrote before the error must not be flushed
                // by the next statement.
                if vm.autocommit() {
                    self.pager.borrow_mut().rollback().map_err(|r| {
                        EngineError::new(
                            ErrorKind::Execute,
                            format!("{e}; rollback of the failed statement also failed: {r}"),
                        )
                    })?;
                }
                return Err(EngineError::new(ErrorKind::Execute, e));
            }
        };
        if vm.autocommit() {
            self.pager.borrow_mut().flush().map_err(|e| {
                EngineError::new(
                    ErrorKind::Execute,
                    format!("failed to flush pending writes on statement commit: {e}"),
                )
            })?;
        }
        self.autocommit = vm.autocommit();
        // An INSERT/UPDATE/DELETE is a program with a counted write in it,
        // even one that changed no rows (#651).
        let counts_changes = program.instructions.iter().any(|i| {
            matches!(i.opcode, Opcode::Insert | Opcode::Update | Opcode::Delete)
                && i.p5 & OPFLAG_NCHANGE != 0
        });
        self.connection = vm.connection_state(counts_changes);
        Ok(QueryResult {
            columns: Vec::new(),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(Cell::from).collect())
                .collect(),
            #[cfg(feature = "vm-stream")]
            scope_report: None,
        })
    }

    /// This table's schema, or an error if no such table exists. Crate-
    /// internal: `engine::cross_mode`'s row→batch adapter needs the table's
    /// shape (root page, rowid alias) to scan it directly, without going
    /// through `vm::row` -- deliberately outside this module's tree (ADR
    /// 0000 §(c): the SQLite side never names `vm::batch`), so it takes
    /// only mode-agnostic types (`schema::TableSchema`) from here.
    #[cfg(any(feature = "vm-batch", test))]
    pub(crate) fn table_schema(&self, table: &str) -> Result<TableSchema, EngineError> {
        let (schemas, _views) = self.catalog()?;
        schemas
            .into_iter()
            .find(|t| t.name == table)
            .ok_or_else(|| EngineError::new(ErrorKind::Compile, format!("no such table: {table}")))
    }

    /// How many rows `table` holds, for a cross-mode `EXPLAIN` (#789): the
    /// row count `ANALYZE` recorded in `sqlite_stat1` when there is one
    /// (no scan), else an exact `COUNT(*)`.
    #[cfg(any(feature = "vm-batch", test))]
    pub(crate) fn table_row_count(&self, table: &str) -> Result<i64, EngineError> {
        let catalog = self.catalog()?;
        let analyzed = self
            .stats_by_table(&catalog.0)
            .get(table)
            .and_then(Stats::table_rows);
        if let Some(rows) = analyzed {
            return Ok(i64::try_from(rows).unwrap_or(i64::MAX));
        }
        let sql = format!("SELECT COUNT(*) FROM \"{}\"", table.replace('"', "\"\""));
        let select = Self::parse_select_or_err(&sql)?;
        match self
            .run_select(&select, &sql, &catalog)?
            .rows
            .first()
            .and_then(|r| r.first())
        {
            Some(Cell::Int(n)) => Ok(*n),
            other => Err(EngineError::new(
                ErrorKind::Execute,
                format!("COUNT(*) over `{table}` returned {other:?}, not an integer"),
            )),
        }
    }

    /// Runs `f` with this engine's shared pager and header -- the only
    /// storage access `engine::cross_mode`'s adapter needs for a read-only
    /// whole-table scan.
    #[cfg(any(feature = "vm-batch", test))]
    pub(crate) fn with_storage<T>(&self, f: impl FnOnce(&Pager, &DatabaseHeader) -> T) -> T {
        let pager = self.pager.borrow();
        f(&pager, &self.header)
    }

    fn compile_one(&self, stmt: &str, catalog: &Catalog) -> Result<Program, EngineError> {
        let (schemas, views) = catalog;
        if Self::is_select(stmt) {
            let select = Self::parse_select_or_err(stmt)?;
            let stats_by_table = self.stats_by_table(schemas);
            compile_select_statement(&select, schemas, views, &stats_by_table)
                .map_err(|e| EngineError::new(ErrorKind::Compile, e))
        } else {
            compile_statement(stmt, schemas, views)
                .map_err(|e| EngineError::new(ErrorKind::Compile, e))
        }
    }
}

impl Engine for RowEngine {
    fn open(path: &Path) -> Result<Self, EngineError> {
        let vfs = UnixVfs;
        let file = vfs
            .open_read(path)
            .map_err(|e| EngineError::new(ErrorKind::Open, e))?;
        let mut header_buf = [0u8; HEADER_LEN];
        file.read_at(&mut header_buf, 0)
            .map_err(|e| EngineError::new(ErrorKind::Open, e))?;
        let header =
            DatabaseHeader::parse(&header_buf).map_err(|e| EngineError::new(ErrorKind::Open, e))?;
        let pager = Pager::open(&vfs, path, header.page_size)
            .map_err(|e| EngineError::new(ErrorKind::Open, e))?;
        Ok(RowEngine {
            path: path.to_path_buf(),
            header,
            pager: Rc::new(RefCell::new(pager)),
            catalog: RefCell::new(None),
            autocommit: true,
            connection: ConnectionState::default(),
        })
    }

    fn mode(&self) -> Mode {
        Mode::Row
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        let mut last = QueryResult::default();
        for stmt in split_statements(sql) {
            let catalog = self.catalog()?;
            let result = if Self::is_select(&stmt) {
                let select = Self::parse_select_or_err(&stmt)?;
                self.run_select(&select, &stmt, &catalog)?
            } else {
                let program = self.compile_one(&stmt, &catalog)?;
                self.run_statement(&program)?
            };
            if Self::is_schema_changing(&stmt) {
                *self.catalog.borrow_mut() = None;
            }
            if !result.is_empty() {
                last = result;
            }
        }
        Ok(last)
    }

    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        let stmt = single_statement(sql)?;
        if !Self::is_select(&stmt) {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                "EXPLAIN QUERY PLAN is only available for SELECT",
            ));
        }
        let select = Self::parse_select_or_err(&stmt)?;
        let (schemas, views) = self.catalog()?;
        let stats_by_table = self.stats_by_table(&schemas);
        let rows = explain_select_statement(&select, &schemas, &views, &stats_by_table)
            .map_err(|e| EngineError::new(ErrorKind::Compile, e))?;
        Ok(rows
            .into_iter()
            .map(|r| PlanRow {
                id: i64::from(r.id),
                parent: i64::from(r.parent),
                detail: r.detail,
            })
            .collect())
    }

    fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError> {
        let stmt = single_statement(sql)?;
        let catalog = self.catalog()?;
        let program = self.compile_one(&stmt, &catalog)?;
        let rows = explain(&program)
            .into_iter()
            .map(|r| OpcodeRow {
                addr: r.addr,
                opcode: r.opcode.to_string(),
                operands: format!("{} {} {} {}", r.p1, r.p2, r.p3, r.p4),
                comment: r.comment,
                is_finalize: false,
            })
            .collect();
        Ok(vec![OpcodeSection {
            label: "main".to_string(),
            lane: "row",
            rows,
        }])
    }

    fn stats(&self) -> FileStats {
        FileStats::Row {
            page_size: self.header.page_size,
            page_count: self.header.page_count,
            freelist_pages: self.header.freelist_page_count,
        }
    }

    fn tables(&self) -> Result<Vec<TableInfo>, EngineError> {
        let (schemas, _views) = self.catalog()?;
        Ok(schemas
            .into_iter()
            .map(|t| TableInfo {
                name: t.name,
                columns: t
                    .columns
                    .into_iter()
                    .zip(t.column_types)
                    .map(|(name, type_name)| ColumnInfo { name, type_name })
                    .collect(),
            })
            .collect())
    }
}

#[cfg(test)]
#[allow(non_snake_case, clippy::unwrap_used, clippy::unwrap_in_result)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`).

    use super::{CreateOptions, RowEngine};

    fn create_with_page_size(page_size: u32) -> Result<RowEngine, super::EngineError> {
        let path = std::env::temp_dir().join(format!(
            "db-core-create-mcdc-{}-{page_size}.db",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        let result = RowEngine::create(
            &path,
            CreateOptions {
                page_size,
                ..CreateOptions::default()
            },
            "",
        );
        std::fs::remove_file(&path).ok();
        result
    }

    #[test]
    fn mcdc__engine_row_create_4b191083__v1_a_power_of_two_in_range_is_a_valid_page_size() {
        assert!(create_with_page_size(2048).is_ok());
    }

    #[test]
    fn mcdc__engine_row_create_4b191083__v2_a_size_that_is_not_a_power_of_two_is_refused() {
        assert!(create_with_page_size(1000).is_err());
    }

    #[test]
    fn mcdc__engine_row_create_4b191083__v3_a_power_of_two_outside_the_range_is_refused() {
        assert!(create_with_page_size(256).is_err());
    }
}
