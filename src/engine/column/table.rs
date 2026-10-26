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
//!
//! Before any footer is read, a `WHERE` clause skips the files whose
//! manifest key ranges (#589) prove it matches none of their rows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    engine, leaf_columns, opcode_sections, parse_select, plan_err, plan_rows, planner, prune,
    query_result, single_statement, table_row_groups, timestamp_literals, type_name, ColumnInfo,
    Engine, EngineError, ErrorKind, FileStats, Leaf, Mode, OpcodeSection, ParquetFile, PlanRow,
    Program, QueryResult, RowGroupSegment, Segment, Select, TableInfo, TableSegment, TableStats,
    Value, VmError,
};
use crate::parser::ast::{Expr, Literal};
use crate::storage::ingest::manifest::column_bounds;
use crate::storage::ingest::snapshot::{cast, physical_type};
use crate::storage::ingest::{
    FileEntry, KeyBound, KeyRange, PageBody, ReaderStats, RowPage, SessionKey, Snapshot,
    TableReader,
};
use crate::storage::parquet::footer::PhysicalType;
use crate::storage::parquet::schema::{ColumnSpec, ColumnType};
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
pub(super) fn resolve(
    schema: &[ColumnSpec],
    names: &[String],
) -> Result<Vec<Request>, EngineError> {
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
pub(super) fn file_plan(
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
    let (schema, select) = parsed_select(snapshot, table, sql)?;
    let program: Program = planner::compile(&select).map_err(plan_err)?;
    let requests = resolve(&schema, &program.columns_to_load())?;
    let keep = file_flags(snapshot, &schema, select.where_clause.as_ref());
    let files = snapshot
        .files()
        .zip(keep)
        .filter(|(_, keep)| *keep)
        .map(|(file, _)| file)
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
    segments.extend(page_segments(
        snapshot,
        &schema,
        &requests,
        select.where_clause.as_ref(),
    ));
    let output =
        engine::run(&segments, &program).map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
    Ok(query_result(&select, output))
}

/// How many of `snapshot`'s files [`query_snapshot`] opens for `sql`: those
/// its `WHERE` clause may match a row of (#777).
pub fn query_snapshot_files(
    snapshot: &Snapshot,
    table: &str,
    sql: &str,
) -> Result<usize, EngineError> {
    let (schema, select) = parsed_select(snapshot, table, sql)?;
    Ok(file_flags(snapshot, &schema, select.where_clause.as_ref())
        .into_iter()
        .filter(|keep| *keep)
        .count())
}

fn parsed_select(
    snapshot: &Snapshot,
    table: &str,
    sql: &str,
) -> Result<(Vec<ColumnSpec>, Select), EngineError> {
    let schema = snapshot.schema().map_err(open_error)?;
    let stmt = single_statement(sql)?;
    let mut select = parse_select(&stmt, table, &column_names(&schema), "this directory")?;
    timestamp_literals(&mut select, &schema_timestamps(&schema))?;
    Ok((schema, select))
}

/// For each file of `snapshot`, oldest first, whether `where_clause` may
/// match a row of it.
pub(super) fn file_flags(
    snapshot: &Snapshot,
    schema: &[ColumnSpec],
    where_clause: Option<&Expr>,
) -> Vec<bool> {
    snapshot
        .files()
        .map(|(entry, _)| {
            may_match(
                entry,
                schema,
                snapshot.manifest().session_key.as_ref(),
                where_clause,
            )
        })
        .collect()
}

/// Whether `where_clause` may match a row of `entry`'s file: false when
/// its key ranges, or the arrival window of the session keys it names
/// (#599), prove it matches none.
pub(super) fn may_match(
    entry: &FileEntry,
    schema: &[ColumnSpec],
    session_key: Option<&SessionKey>,
    where_clause: Option<&Expr>,
) -> bool {
    where_clause.is_none_or(|w| {
        let session = session_key.and_then(|key| SessionWindow::of(key, entry, schema));
        !(prune::definitely_empty_in(w, &(entry.keys.as_slice(), schema))
            || session.is_some_and(|s| prune::definitely_empty_in(w, &s)))
    })
}

/// The keys of a session key column that can have rows in one file, as
/// pruning bounds. A file whose rows arrived in `[lo, hi]` holds rows of
/// sessions that started in `[lo - max_session, hi]`, whose keys lie in
/// `[(lo_s - max) << 32, (hi_s << 32) | 0xFFFF_FFFF]`.
struct SessionWindow {
    column: String,
    min: i64,
    max: i64,
    physical: PhysicalType,
}

impl SessionWindow {
    /// `None` when the file has no arrival bounds or the key column is not
    /// an integer of the schema (a file then bounds nothing).
    fn of(key: &SessionKey, entry: &FileEntry, schema: &[ColumnSpec]) -> Option<Self> {
        const NS: i64 = 1_000_000_000;
        let (lo, hi) = entry.observed_ts?;
        let spec = schema.iter().find(|s| s.name == key.column)?;
        if !matches!(spec.column_type, ColumnType::Int64 | ColumnType::UInt64) {
            return None;
        }
        // Saturating, so a bound that does not fit i64 stays a bound that
        // keeps the file ("may match") rather than wrapping.
        let key_of = |secs: i64| secs.saturating_mul(1 << 32);
        let first = lo
            .div_euclid(NS)
            .saturating_sub(i64::from(key.max_session_secs));
        Some(SessionWindow {
            column: key.column.clone(),
            min: key_of(first),
            max: key_of(hi.div_euclid(NS)).saturating_add(0xFFFF_FFFF),
            physical: physical_type(spec.column_type),
        })
    }
}

impl prune::Bounds for SessionWindow {
    fn bounds(
        &self,
        column: &str,
        literal: &Literal,
    ) -> Option<(
        Option<prune::StatValue>,
        Option<prune::StatValue>,
        prune::StatValue,
    )> {
        if column != self.column {
            return None;
        }
        let literal = prune::literal_stat_value(literal, self.physical)?;
        Some((
            Some(prune::StatValue::Int(self.min)),
            Some(prune::StatValue::Int(self.max)),
            literal,
        ))
    }
}

/// A file's manifest key ranges and the table's columns, as pruning
/// bounds. A range bounds a column only if its kind matches the table's
/// type for it: a file flushed before the column widened (an integer
/// column now a string) bounds nothing.
impl prune::Bounds for (&[KeyRange], &[ColumnSpec]) {
    fn bounds(
        &self,
        column: &str,
        literal: &Literal,
    ) -> Option<(
        Option<prune::StatValue>,
        Option<prune::StatValue>,
        prune::StatValue,
    )> {
        let (keys, schema) = *self;
        let range = keys.iter().find(|k| k.column == column)?;
        let spec = schema.iter().find(|s| s.name == column)?;
        let integer = matches!(
            spec.column_type,
            ColumnType::Int32
                | ColumnType::Int64
                | ColumnType::TimestampMicros
                | ColumnType::TimestampNanos
                | ColumnType::UInt8
                | ColumnType::UInt16
                | ColumnType::UInt32
                | ColumnType::UInt64
        );
        let bound = |b: &KeyBound| match b {
            KeyBound::Int(i) if integer => Some(prune::StatValue::Int(*i)),
            KeyBound::Str(s) if spec.column_type == ColumnType::Utf8 => {
                Some(prune::StatValue::Str(s.clone()))
            }
            KeyBound::Int(_) | KeyBound::Str(_) => None,
        };
        let literal = prune::literal_stat_value(literal, physical_type(spec.column_type))?;
        Some((Some(bound(&range.min)?), Some(bound(&range.max)?), literal))
    }
}

/// A record page's column ranges, taken when a predicate names the column
/// (a page keeps none), with the table's columns, as pruning bounds.
impl prune::Bounds for (&RowPage, &[ColumnSpec]) {
    fn bounds(
        &self,
        column: &str,
        literal: &Literal,
    ) -> Option<(
        Option<prune::StatValue>,
        Option<prune::StatValue>,
        prune::StatValue,
    )> {
        let (rows, schema) = *self;
        let at = rows.schema().iter().position(|s| s.name == column)?;
        let (min, max) = column_bounds(rows.columns().get(at)?)??;
        let range = KeyRange {
            column: column.to_string(),
            min,
            max,
        };
        prune::Bounds::bounds(&(std::slice::from_ref(&range), schema), column, literal)
    }
}

/// One segment per page of `snapshot`, loading `requests`, without the
/// record pages whose column ranges prove `where_clause` matches none of
/// their rows. A lines page has no ranges and is always read.
pub(super) fn page_segments(
    snapshot: &Snapshot,
    schema: &[ColumnSpec],
    requests: &[Request],
    where_clause: Option<&Expr>,
) -> Vec<TableSegment<'static, 'static>> {
    snapshot
        .pages()
        .iter()
        .filter(|page| match (&page.body, where_clause) {
            (PageBody::Rows(rows), Some(w)) => {
                !prune::definitely_empty_in(w, &(rows.as_ref(), schema))
            }
            _ => true,
        })
        .map(|page| match &page.body {
            PageBody::Lines(segment) => TableSegment::Page {
                segment: Arc::clone(segment),
                requests: requests.to_vec(),
                format: snapshot.format(),
            },
            PageBody::Rows(rows) => TableSegment::Rows {
                rows: Arc::clone(rows),
                requests: requests.to_vec(),
            },
        })
        .collect()
}

/// `snapshot`'s `columns` as one batch, every row of every file and page in
/// LSN order, keyed and fixed up as [`snapshot_segments`] does: the whole
/// table, for a join's build side or a window's input
/// (t-rust-db/tools#13).
///
/// [`snapshot_segments`]: super::snapshot_segments
pub fn read_snapshot(snapshot: &Snapshot, columns: &[String]) -> Result<Batch, EngineError> {
    let files = super::snapshot_files(snapshot)?;
    let program = Program::from_opcodes([]);
    let segments = super::snapshot_segments(snapshot, &files, columns, &program)?;
    merge_segments(&segments, columns)
}

/// A predicate over a table's columns, parsed once, that readers use to
/// skip the files and record pages it excludes (#777): by the manifest
/// ranges of the sort key and `stat_columns`, and a page's own column
/// ranges. It only skips; the caller still applies the predicate to the
/// rows it gets.
pub struct SnapshotFilter {
    schema: Vec<ColumnSpec>,
    expr: Expr,
}

impl SnapshotFilter {
    /// `predicate` as a `WHERE` clause over `snapshot`'s table `table`.
    pub fn parse(snapshot: &Snapshot, table: &str, predicate: &str) -> Result<Self, EngineError> {
        let schema = snapshot.schema().map_err(open_error)?;
        let stmt = single_statement(&format!("SELECT * FROM {table} WHERE {predicate}"))?;
        let mut select = parse_select(&stmt, table, &column_names(&schema), "this directory")?;
        timestamp_literals(&mut select, &schema_timestamps(&schema))?;
        let expr = select
            .where_clause
            .ok_or_else(|| open_error("the predicate is empty"))?;
        Ok(SnapshotFilter { schema, expr })
    }

    pub(super) fn schema(&self) -> &[ColumnSpec] {
        &self.schema
    }

    pub(super) fn expr(&self) -> &Expr {
        &self.expr
    }
}

/// [`read_snapshot`] without the files and record pages `filter` excludes
/// (#777): for a join build side or window input whose key predicate is
/// known, the rows that can match it, and possibly others.
pub fn read_snapshot_where(
    snapshot: &Snapshot,
    columns: &[String],
    filter: &SnapshotFilter,
) -> Result<Batch, EngineError> {
    let files = super::snapshot_files_where(snapshot, filter)?;
    let program = Program::from_opcodes([]);
    let segments = super::snapshot_segments_where(snapshot, &files, columns, &program, filter)?;
    merge_segments(&segments, columns)
}

fn merge_segments(
    segments: &[super::SnapshotSegment<'_, '_>],
    columns: &[String],
) -> Result<Batch, EngineError> {
    let mut merged: Vec<Vec<Value>> = vec![Vec::new(); columns.len()];
    let mut rows = 0usize;
    for segment in segments {
        let mut batch = Arc::unwrap_or_clone(
            segment
                .load()
                .map_err(|e| EngineError::new(ErrorKind::Execute, e))?,
        );
        rows = rows.saturating_add(batch.num_rows);
        for (values, key) in merged.iter_mut().zip(columns) {
            values.extend(take_values(&mut batch, key));
        }
    }
    Ok(columns
        .iter()
        .zip(merged)
        .fold(Batch::new(rows), |batch, (key, values)| {
            batch.with_column(key.clone(), values)
        }))
}

/// The schema's TIMESTAMP columns and their units per second, for
/// [`timestamp_literals`] (#679).
fn schema_timestamps(schema: &[ColumnSpec]) -> Vec<(String, i64)> {
    schema
        .iter()
        .filter_map(|s| match s.column_type {
            ColumnType::TimestampMicros => Some((s.name.clone(), 1_000_000)),
            ColumnType::TimestampNanos => Some((s.name.clone(), 1_000_000_000)),
            _ => None,
        })
        .collect()
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
    reader: TableReader,
    schema: Vec<ColumnSpec>,
}

impl std::fmt::Debug for TableEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableEngine")
            .field("dir", &self.dir)
            .field("table", &self.table)
            .field("reader", &self.reader)
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
        self.reader.snapshot()
    }

    /// What the reader behind the queries has done: how many records it
    /// parsed, and how often it read the whole tail again (#593).
    #[must_use]
    pub fn reader_stats(&self) -> ReaderStats {
        self.reader.stats()
    }

    /// Reads what the writer added since the last read, keeping the
    /// records it already parsed.
    pub fn refresh(&mut self) -> Result<(), EngineError> {
        let snapshot = self.reader.refresh().map_err(open_error)?;
        self.schema = snapshot.schema().map_err(open_error)?;
        Ok(())
    }

    fn parse(&self, sql: &str) -> Result<Select, EngineError> {
        let stmt = single_statement(sql)?;
        let mut select = parse_select(
            &stmt,
            &self.table,
            &column_names(&self.schema),
            "this directory",
        )?;
        timestamp_literals(&mut select, &schema_timestamps(&self.schema))?;
        Ok(select)
    }

    /// Row groups plus pages.
    fn segment_count(&self) -> usize {
        let row_groups: usize = self
            .snapshot()
            .files()
            .filter_map(|(_, bytes)| ParquetFile::open(bytes).ok())
            .map(|f| f.num_row_groups())
            .sum();
        row_groups.saturating_add(self.snapshot().pages().len())
    }

    fn rows(&self) -> i64 {
        i64::try_from(self.snapshot().rows()).unwrap_or(i64::MAX)
    }
}

/// A table directory's name in SQL: the directory's name.
fn table_name(dir: &Path) -> String {
    dir.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("data")
        .to_string()
}

#[cfg(feature = "ingest")]
impl crate::storage::ingest::Table {
    /// Runs one `SELECT` over a snapshot of everything appended so far,
    /// the table named after its directory, as
    /// [`query_snapshot`] does.
    pub fn query(&self, sql: &str) -> Result<QueryResult, EngineError> {
        let snapshot = self.snapshot().map_err(open_error)?;
        query_snapshot(&snapshot, &table_name(self.dir()), sql)
    }
}

impl Engine for TableEngine {
    fn open(path: &Path) -> Result<Self, EngineError> {
        let reader = TableReader::open(path).map_err(open_error)?;
        let schema = reader.snapshot().schema().map_err(open_error)?;
        Ok(TableEngine {
            dir: path.to_path_buf(),
            table: table_name(path),
            reader,
            schema,
        })
    }

    fn mode(&self) -> Mode {
        Mode::Batch
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        self.refresh()?;
        query_snapshot(self.reader.snapshot(), &self.table, sql)
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
