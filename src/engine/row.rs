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
use crate::codegen::row::{
    compile_statement, leading_keywords, output_column_names, resolve_from_table_schema,
};
use crate::parser::ast::Select;
use crate::parser::row::error::{parse_select, ParseOutcome};
use crate::parser::row::tokenizer::split_statements;
use crate::schema::{TableSchema, ViewSchema};
use crate::storage::row::btree::TableCursor;
use crate::storage::row::header::{DatabaseHeader, HEADER_LEN};
use crate::storage::row::pager::Pager;
use crate::storage::row::schema::read_schema_and_views;
use crate::storage::row::vfs::{PageSource, UnixVfs, Vfs};
use crate::vm::row::{execute, explain, Program, Vm};

use super::{
    single_statement, Cell, ColumnInfo, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, TableInfo,
};

pub mod adapter;
pub mod stats;

use adapter::{BtreeSchemaStorage, PagerTransaction, StorageFactory};

type Catalog = (Vec<TableSchema>, Vec<ViewSchema>);

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

    fn is_select(stmt: &str) -> bool {
        leading_keywords(stmt)
            .first()
            .is_some_and(|kw| kw.as_str() == "SELECT")
    }

    /// Conservative catalog dirty flag: any `CREATE`/`DROP`/`ALTER`
    /// invalidates, even one that fails or is a no-op.
    fn is_schema_changing(stmt: &str) -> bool {
        let head = stmt.trim_start();
        ["CREATE", "DROP", "ALTER"].iter().any(|kw| {
            head.get(..kw.len())
                .is_some_and(|h| h.eq_ignore_ascii_case(kw))
        })
    }

    fn read_only_vm(&self) -> Vm {
        let source: Rc<dyn PageSource> = Rc::clone(&self.pager) as Rc<dyn PageSource>;
        let mut vm = Vm::new();
        vm.set_text_encoding(self.header.text_encoding);
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

    /// Column labels for a `SELECT`: schema-derived for a single-table
    /// query, `column1..N` otherwise (sqlite-rs's REPL rule).
    fn derive_headers(select: &Select, schemas: &[TableSchema]) -> Vec<String> {
        let single_table = select.compound.is_empty()
            && select
                .from
                .as_ref()
                .is_some_and(|from| from.joins.is_empty());
        if single_table {
            if let Some(from) = &select.from {
                if let Ok(schema) = resolve_from_table_schema(&from.first, schemas) {
                    return output_column_names(select, &schema);
                }
            }
        }
        let count = select.columns.len().max(1);
        (1..=count).map(|i| format!("column{i}")).collect()
    }

    fn run_select(&self, select: &Select, catalog: &Catalog) -> Result<QueryResult, EngineError> {
        let (schemas, views) = catalog;
        let stats_by_table = self.stats_by_table(schemas);
        let program = compile_select_statement(select, schemas, views, &stats_by_table)
            .map_err(|e| EngineError::new(ErrorKind::Compile, e))?;
        let columns = Self::derive_headers(select, schemas);
        let mut vm = self.read_only_vm();
        let rows =
            execute(&mut vm, &program).map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
        Ok(QueryResult {
            columns,
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(Cell::from).collect())
                .collect(),
        })
    }

    /// One write/DDL/transaction statement on the shared pager. Outside an
    /// explicit transaction a clean `Halt` flushes the pager (every
    /// statement is its own transaction); inside `BEGIN ... COMMIT` the
    /// `AutoCommit` opcode flushes instead.
    fn run_statement(&mut self, program: &Program) -> Result<QueryResult, EngineError> {
        let mut vm = self.writable_vm();
        let rows =
            execute(&mut vm, program).map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
        if vm.autocommit() {
            self.pager.borrow_mut().flush().map_err(|e| {
                EngineError::new(
                    ErrorKind::Execute,
                    format!("failed to flush pending writes on statement commit: {e}"),
                )
            })?;
        }
        self.autocommit = vm.autocommit();
        Ok(QueryResult {
            columns: Vec::new(),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(Cell::from).collect())
                .collect(),
        })
    }

    /// This table's schema, or an error if no such table exists. Crate-
    /// internal: `engine::cross_mode`'s row→batch adapter needs the table's
    /// shape (root page, rowid alias) to scan it directly, without going
    /// through `vm::row` -- deliberately outside this module's tree (ADR
    /// 0000 §(c): the SQLite side never names `vm::batch`), so it takes
    /// only mode-agnostic types (`schema::TableSchema`) from here.
    pub(crate) fn table_schema(&self, table: &str) -> Result<TableSchema, EngineError> {
        let (schemas, _views) = self.catalog()?;
        schemas
            .into_iter()
            .find(|t| t.name == table)
            .ok_or_else(|| EngineError::new(ErrorKind::Compile, format!("no such table: {table}")))
    }

    /// Runs `f` with this engine's shared pager and header -- the only
    /// storage access `engine::cross_mode`'s adapter needs for a read-only
    /// whole-table scan.
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
                self.run_select(&select, &catalog)?
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
            })
            .collect();
        Ok(vec![OpcodeSection {
            label: "main".to_string(),
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
