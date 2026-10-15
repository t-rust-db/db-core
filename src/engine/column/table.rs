//! An ingested table directory as one batch table (ADR 0027, #572).
//!
//! A query runs over a [`Snapshot`]: one segment per row group of each
//! flushed file, then one per page above `flushed_lsn`, in LSN order, so
//! every record is read exactly once. Columns are the snapshot's
//! [`schema`](Snapshot::schema). A segment that lacks a column loads it as
//! NULL, and one that stores it with another type than the table's (a file
//! flushed before the type widened, or a page) casts it with [`cast`], the
//! rule a flush writes by: a record reads the same before and after its
//! flush.
//!
//! A row group whose columns all have the table's type is the plain
//! [`RowGroupSegment`], with footer-statistics pruning, top-N bounds and
//! late materialization. One that needs a NULL or cast column decodes its
//! columns eagerly and fixes them up; its statistics still prune, but only
//! for columns whose stored type is the table's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    engine, leaf_columns, opcode_sections, parse_select, plan_err, plan_rows, planner,
    query_result, single_statement, table_row_groups, type_name, ColumnInfo, Engine, EngineError,
    ErrorKind, FileStats, Leaf, Mode, OpcodeSection, ParquetFile, PlanRow, Program, QueryResult,
    RowGroupSegment, Segment, Select, TableInfo, TableSegment, TableStats, Value, VmError,
};
use crate::storage::ingest::snapshot::{cast, physical_type};
use crate::storage::ingest::{PageBody, RowPage, Snapshot};
use crate::storage::parquet::writer::{ColumnSpec, ColumnType};
use crate::storage::stream::adapter::stored_column_values;
use crate::storage::stream::detect::Format;
use crate::storage::stream::segment::Segment as StreamSegment;
use crate::vm::batch::Batch;

/// One column a program loads: the batch key, the table column and its
/// type.
#[derive(Debug, Clone)]
pub(super) struct Request {
    pub(super) key: String,
    pub(super) name: String,
    pub(super) column_type: ColumnType,
}

/// What a row group must do to a loaded column to give the table's.
#[derive(Debug, Clone)]
pub(super) enum Fixup {
    /// The file has no such column: all NULL.
    Null(String),
    /// The file stores it with another type: cast to this one.
    Cast(String, ColumnType),
}

impl Fixup {
    fn key(&self) -> &str {
        match self {
            Fixup::Null(key) | Fixup::Cast(key, _) => key,
        }
    }
}

/// `key`'s values in `batch`, removed from it.
fn take_values(batch: &mut Batch, key: &str) -> Vec<Value> {
    if let Some(values) = batch.columns.remove(key) {
        return Arc::unwrap_or_clone(values);
    }
    match batch.typed_columns.remove(key) {
        Some(column) => (0..batch.num_rows).map(|i| column.get(i)).collect(),
        None => vec![Value::Null; batch.num_rows],
    }
}

/// `inner`'s `columns` (those the file has), fixed up to the table's.
fn load_fixed(
    inner: &RowGroupSegment<'_, '_>,
    fixups: &[Fixup],
    columns: &[String],
) -> Result<Arc<Batch>, VmError> {
    let present: Vec<String> = inner
        .columns
        .iter()
        .map(|(key, _, _)| key.clone())
        .filter(|key| columns.contains(key))
        .collect();
    let mut batch = Arc::unwrap_or_clone(inner.load_columns(&present)?);
    for fixup in fixups
        .iter()
        .filter(|f| columns.iter().any(|c| c == f.key()))
    {
        let values = match fixup {
            Fixup::Null(_) => vec![Value::Null; batch.num_rows],
            Fixup::Cast(key, t) => take_values(&mut batch, key)
                .into_iter()
                .map(|v| cast(v, *t))
                .collect(),
        };
        batch = batch.with_column(fixup.key(), values);
    }
    Ok(Arc::new(batch))
}

/// A page's columns as its table stores them, so a record reads the same
/// before and after it is flushed (#583).
fn load_page(segment: &StreamSegment, requests: &[Request], format: Format) -> Batch {
    let rows = segment.len();
    requests.iter().fold(Batch::new(rows), |batch, r| {
        let values = stored_column_values(segment, &r.name, format).map_or_else(
            || vec![Value::Null; rows],
            |values| values.into_iter().map(|v| cast(v, r.column_type)).collect(),
        );
        batch.with_column(r.key.clone(), values)
    })
}

/// A record table's page as the engine reads the same rows once flushed
/// (#594): each column through the decode rules of its Parquet type.
fn load_rows(rows: &RowPage, requests: &[Request]) -> Result<Batch, VmError> {
    requests
        .iter()
        .try_fold(Batch::new(rows.len()), |batch, r| {
            let values = match rows.engine_column(&r.name) {
                Some(values) => values.map_err(|reason| VmError::SegmentLoad {
                    reason: format!("page column `{}`: {reason}", r.name),
                })?,
                None => vec![Value::Null; rows.len()],
            };
            Ok(batch.with_column(r.key.clone(), values))
        })
}

impl Segment for TableSegment<'_, '_> {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        match self {
            TableSegment::RowGroup { inner, fixups } if fixups.is_empty() => inner.load(),
            TableSegment::RowGroup { inner, fixups } => {
                let all: Vec<String> = inner
                    .columns
                    .iter()
                    .map(|(key, _, _)| key.clone())
                    .chain(fixups.iter().map(|f| f.key().to_string()))
                    .collect();
                load_fixed(inner, fixups, &all)
            }
            TableSegment::Page {
                segment,
                requests,
                format,
            } => Ok(Arc::new(load_page(segment, requests, *format))),
            TableSegment::Rows { rows, requests } => load_rows(rows, requests).map(Arc::new),
        }
    }

    fn order_key_bound(&self, descending: bool) -> Option<Value> {
        match self {
            TableSegment::RowGroup { inner, .. } => inner.order_key_bound(descending),
            TableSegment::Page { .. } | TableSegment::Rows { .. } => None,
        }
    }

    fn load_columns(&self, columns: &[String]) -> Result<Arc<Batch>, VmError> {
        match self {
            TableSegment::RowGroup { inner, fixups } if fixups.is_empty() => {
                inner.load_columns(columns)
            }
            TableSegment::RowGroup { inner, fixups } => load_fixed(inner, fixups, columns),
            TableSegment::Page { .. } | TableSegment::Rows { .. } => self.load(),
        }
    }

    fn gather_at(&self, columns: &[String], positions: &[u32]) -> Option<Result<Batch, VmError>> {
        match self {
            TableSegment::RowGroup { inner, fixups } if fixups.is_empty() => {
                inner.gather_at(columns, positions)
            }
            TableSegment::RowGroup { .. }
            | TableSegment::Page { .. }
            | TableSegment::Rows { .. } => None,
        }
    }
}

/// The program's `columns_to_load()` against the table's columns. A name
/// that is itself a column (a flattened `a.b` field) wins over reading
/// it as `table.column`.
fn resolve(schema: &[ColumnSpec], names: &[String]) -> Result<Vec<Request>, EngineError> {
    names
        .iter()
        .map(|name| {
            let find = |n: &str| schema.iter().find(|s| s.name == n);
            let spec = find(name)
                .or_else(|| find(planner::split_qualified(name).1))
                .ok_or_else(|| {
                    EngineError::new(ErrorKind::Compile, format!("unknown column: {name}"))
                })?;
            Ok(Request {
                key: name.clone(),
                name: spec.name.clone(),
                column_type: spec.column_type,
            })
        })
        .collect()
}

/// How a file serves `requests`: the leaves to decode (keyed by request),
/// the leaves stored with the table's type, whose statistics bound the
/// values a query sees, and the fix-ups the rest need.
fn file_plan(
    leaves: &[Leaf],
    schema: &[ColumnSpec],
    requests: &[Request],
) -> (Vec<Leaf>, Vec<Leaf>, Vec<Fixup>) {
    let comparable: Vec<Leaf> = leaves
        .iter()
        .filter(|(name, _, pt)| {
            schema
                .iter()
                .any(|s| s.name == *name && physical_type(s.column_type) == *pt)
        })
        .cloned()
        .collect();
    let mut columns = Vec::new();
    let mut fixups = Vec::new();
    for r in requests {
        match leaves.iter().find(|(name, _, _)| *name == r.name) {
            Some((_, index, pt)) => {
                columns.push((r.key.clone(), *index, *pt));
                if physical_type(r.column_type) != *pt {
                    fixups.push(Fixup::Cast(r.key.clone(), r.column_type));
                }
            }
            None => fixups.push(Fixup::Null(r.key.clone())),
        }
    }
    (columns, comparable, fixups)
}

fn open_error(e: impl std::fmt::Display) -> EngineError {
    EngineError::new(ErrorKind::Open, e)
}

/// Runs one `SELECT` over `snapshot` as table `table`. The table's
/// columns are [`Snapshot::schema`]; joins, `IN (SELECT ...)` and window
/// functions are unsupported, as in [`BatchEngine`](super::BatchEngine).
pub fn query_snapshot(
    snapshot: &Snapshot,
    table: &str,
    sql: &str,
) -> Result<QueryResult, EngineError> {
    let schema = snapshot.schema().map_err(open_error)?;
    let stmt = single_statement(sql)?;
    let select = parse_select(&stmt, table, &column_names(&schema), "this directory")?;
    let program: Program = planner::compile(&select).map_err(plan_err)?;
    let requests = resolve(&schema, &program.columns_to_load())?;
    let files = snapshot
        .files()
        .map(|(entry, bytes)| {
            ParquetFile::open(bytes).map_err(|e| open_error(format!("{}: {e}", entry.path)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut segments: Vec<TableSegment<'_, '_>> = files
        .iter()
        .flat_map(|file| {
            let (columns, comparable, fixups) = file_plan(&leaf_columns(file), &schema, &requests);
            table_row_groups(file, &columns, &comparable, &select, &program, &fixups)
        })
        .collect();
    segments.extend(snapshot.pages().iter().map(|page| match &page.body {
        PageBody::Lines(segment) => TableSegment::Page {
            segment: Arc::clone(segment),
            requests: requests.clone(),
            format: snapshot.format(),
        },
        PageBody::Rows(rows) => TableSegment::Rows {
            rows: Arc::clone(rows),
            requests: requests.clone(),
        },
    }));
    let output =
        engine::run(&segments, &program).map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
    Ok(query_result(&select, output))
}

fn column_names(schema: &[ColumnSpec]) -> Vec<String> {
    schema.iter().map(|s| s.name.clone()).collect()
}

/// A table directory written by
/// [`TableWriter`](crate::storage::ingest::TableWriter), read from any
/// process. Each query reads a fresh [`Snapshot`], so it sees every record
/// written before it started.
pub struct TableEngine {
    dir: PathBuf,
    table: String,
    snapshot: Snapshot,
    schema: Vec<ColumnSpec>,
}

impl std::fmt::Debug for TableEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableEngine")
            .field("dir", &self.dir)
            .field("table", &self.table)
            .field("snapshot", &self.snapshot)
            .finish_non_exhaustive()
    }
}

impl TableEngine {
    /// The table directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The table name, the directory's name.
    #[must_use]
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// The snapshot the last query (or the open) read.
    #[must_use]
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Reads a fresh snapshot.
    pub fn refresh(&mut self) -> Result<(), EngineError> {
        let snapshot = Snapshot::open(&self.dir).map_err(open_error)?;
        self.schema = snapshot.schema().map_err(open_error)?;
        self.snapshot = snapshot;
        Ok(())
    }

    fn parse(&self, sql: &str) -> Result<Select, EngineError> {
        let stmt = single_statement(sql)?;
        parse_select(
            &stmt,
            &self.table,
            &column_names(&self.schema),
            "this directory",
        )
    }

    /// Row groups plus pages.
    fn segment_count(&self) -> usize {
        let row_groups: usize = self
            .snapshot
            .files()
            .filter_map(|(_, bytes)| ParquetFile::open(bytes).ok())
            .map(|f| f.num_row_groups())
            .sum();
        row_groups.saturating_add(self.snapshot.pages().len())
    }

    fn rows(&self) -> i64 {
        i64::try_from(self.snapshot.rows()).unwrap_or(i64::MAX)
    }
}

impl Engine for TableEngine {
    fn open(path: &Path) -> Result<Self, EngineError> {
        let snapshot = Snapshot::open(path).map_err(open_error)?;
        let schema = snapshot.schema().map_err(open_error)?;
        let table = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("data")
            .to_string();
        Ok(TableEngine {
            dir: path.to_path_buf(),
            table,
            snapshot,
            schema,
        })
    }

    fn mode(&self) -> Mode {
        Mode::Batch
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        self.refresh()?;
        query_snapshot(&self.snapshot, &self.table, sql)
    }

    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        let select = self.parse(sql)?;
        let stats = TableStats {
            row_groups: self.segment_count(),
            rows: self.rows(),
            source: None,
        };
        plan_rows(&select, &stats)
    }

    fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError> {
        opcode_sections(&self.parse(sql)?)
    }

    fn stats(&self) -> FileStats {
        FileStats::Batch {
            row_groups: self.segment_count(),
            rows: self.rows(),
        }
    }

    fn tables(&self) -> Result<Vec<TableInfo>, EngineError> {
        Ok(vec![TableInfo {
            name: self.table.clone(),
            columns: self
                .schema
                .iter()
                .map(|s| ColumnInfo {
                    name: s.name.clone(),
                    type_name: type_name(physical_type(s.column_type)),
                })
                .collect(),
        }])
    }
}

#[cfg(test)]
mod tests;
