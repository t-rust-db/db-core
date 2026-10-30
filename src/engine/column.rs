// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The batch [`Engine`]: one Parquet file driven through `storage::column`
//! (mmap + Parquet reader) and `vm::batch` (#325, ADR 0017).
//!
//! A port of column-rs's `QueryEngine` single-file case: the file is
//! memory-mapped once, its footer gives the table's columns, and every
//! query is parse -> `expand_star` -> `codegen::batch::compile` ->
//! `vm::engine::run` over one [`Segment`] per row group, each decoding
//! exactly the columns the program loads. The table is named after the
//! file's stem (`orders.parquet` -> `orders`), which is db-studio's model.
//!
//! Single table only: `JOIN`, `IN (SELECT ...)` and window functions need
//! more than one table loaded or a whole-table materialization, and stay
//! in column-rs's multi-table `QueryEngine` -- they report
//! [`ErrorKind::Unsupported`] here rather than a wrong answer.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::codegen::batch::{self as planner, PlanError, TableStats};
use crate::parser::ast::{
    BinaryOp, Expr, ExprKind, Literal, OrderingTerm, ResultColumn, Select, TableRefKind,
};
use crate::parser::ParseError;
use crate::storage::column::posix::PosixFile;
use crate::storage::column::{
    annotation, decode_column_at, decode_column_full, Annotation, Decoded,
};
use crate::storage::parquet::footer::PhysicalType;
use crate::storage::parquet::{Field, FileError};
use crate::storage::{MmapRegion, ParquetFile, PosixVfs, RowGroupReader, Vfs};
use crate::vm::batch::{Batch, Program, Segment, Value, Vm, VmError};
use crate::vm::column::{Bitmap, Column};
use crate::vm::engine;

#[cfg(all(
    feature = "ingest-read",
    feature = "engine-row",
    feature = "engine-stream",
    feature = "vm-batch"
))]
use self::table::TableEngine;
use super::{
    single_statement, Cell, ColumnInfo, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, TableInfo,
};

#[cfg(feature = "ingest-read")]
pub mod table;

/// A leaf column of the file: display name, index in the schema, physical type.
type Leaf = (String, usize, PhysicalType);

/// One open Parquet file, presented as a single table.
pub struct BatchEngine {
    path: PathBuf,
    /// Where the file's bytes are; a fresh [`ParquetFile`] view is opened
    /// over it per query (footer parsing is cheap; the data pages are only
    /// read for the columns a program loads).
    data: Data,
    table: String,
    leaves: Vec<Leaf>,
    /// The TIMESTAMP columns and their units per second (#679).
    timestamps: Vec<(String, i64)>,
    num_rows: i64,
    num_row_groups: usize,
}

/// What a [`BatchEngine`] reads its file from.
enum Data {
    /// Bytes held in memory ([`BatchEngine::from_bytes`]).
    Memory(MmapRegion),
    /// A file on disk, read one column chunk at a time (#841): nothing
    /// stays mapped, so a scan's resident memory is the chunks being
    /// decoded, not every page it has touched.
    Disk(PosixFile),
}

impl std::fmt::Debug for BatchEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchEngine")
            .field("path", &self.path)
            .field("table", &self.table)
            .field("num_rows", &self.num_rows)
            .field("num_row_groups", &self.num_row_groups)
            .finish_non_exhaustive()
    }
}

impl Data {
    fn open(&self) -> Result<ParquetFile<'_>, crate::storage::parquet::FileError> {
        match self {
            Data::Memory(region) => ParquetFile::open(region),
            Data::Disk(file) => ParquetFile::open_file(file),
        }
    }
}

impl BatchEngine {
    /// The file this engine was opened on.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The single table name, derived from the file's stem.
    #[must_use]
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// Opens a Parquet file already held in memory, for a host with no
    /// filesystem (a browser running on `wasm32-unknown-unknown`). `table`
    /// is the name queries use in `FROM`; [`path`](Self::path) returns it
    /// as a relative path since there is no file.
    pub fn from_bytes(table: &str, bytes: impl Into<Arc<[u8]>>) -> Result<Self, EngineError> {
        Self::from_data(
            PathBuf::from(table),
            table.to_string(),
            Data::Memory(MmapRegion::Owned(bytes.into())),
        )
    }

    fn from_data(path: PathBuf, table: String, data: Data) -> Result<Self, EngineError> {
        let (leaves, timestamps, num_rows, num_row_groups) = {
            let file = data
                .open()
                .map_err(|e| EngineError::new(ErrorKind::Open, e))?;
            (
                leaf_columns(&file),
                timestamp_columns(&file),
                file.num_rows(),
                file.num_row_groups(),
            )
        };
        Ok(BatchEngine {
            path,
            data,
            table,
            leaves,
            timestamps,
            num_rows,
            num_row_groups,
        })
    }

    fn file(&self) -> Result<ParquetFile<'_>, EngineError> {
        self.data
            .open()
            .map_err(|e| EngineError::new(ErrorKind::Open, e))
    }

    fn column_names(&self) -> Vec<String> {
        self.leaves.iter().map(|(n, _, _)| n.clone()).collect()
    }

    /// Parses the `SELECT` of a cross-mode join whose probe side is this
    /// file: no `FROM`-table check (the resolver sorts the sides out) and
    /// no `JOIN` rejection, but TIMESTAMP literals resolved as for a
    /// single-table query.
    #[cfg(all(
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    ))]
    pub(crate) fn parse_join(&self, sql: &str) -> Result<Select, EngineError> {
        let mut select = planner_parse(sql)?;
        timestamp_literals(&mut select, &self.timestamps)?;
        Ok(select)
    }

    /// The file's column names, for the resolver's `GROUP BY` lookup.
    /// Row groups and rows of the file, for a cross-mode `EXPLAIN`.
    #[cfg(all(
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    ))]
    pub(crate) fn table_stats(&self) -> TableStats {
        TableStats {
            row_groups: self.num_row_groups,
            rows: self.num_rows,
            source: None,
        }
    }

    #[cfg(all(
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    ))]
    pub(crate) fn columns(&self) -> Vec<String> {
        self.column_names()
    }

    /// Runs `f` over one segment per row group, loading `columns` (`t.col`
    /// names as the join plan qualifies them) eagerly: the probe side of a
    /// cross-mode join. A row group that `prune`'s footer statistics prove
    /// cannot match is left out (the join's own filters still run). The
    /// segments borrow the file view opened here, so
    /// they only live for the call.
    #[cfg(all(
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    ))]
    pub(crate) fn with_probe_segments<T>(
        &self,
        columns: &[String],
        prune: Option<&Expr>,
        bit_pattern: &[String],
        f: impl FnOnce(Vec<FileSegment<'_, '_>>) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        let file = self.file()?;
        let eager = Program::from_opcodes([]);
        f(file_segments_with_bit_patterns(
            &file,
            columns,
            prune,
            &eager,
            bit_pattern,
        )?)
    }

    /// This whole file as one [`Batch`] of `columns` (the names as the join
    /// plan asks for them): the build side of a cross-mode join that has a
    /// second column table (#782). The `bit_pattern` columns read a
    /// `UINT_64` above `i64::MAX` as its signed bit pattern, like the
    /// probe's join keys (#600).
    #[cfg(all(
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    ))]
    pub(crate) fn read_table(
        &self,
        columns: &[String],
        bit_pattern: &[String],
    ) -> Result<Batch, EngineError> {
        self.with_probe_segments(columns, None, bit_pattern, |segments| {
            merge_segments(&segments, columns)
        })
    }

    /// `select`'s quoted timestamps compared with this file's TIMESTAMP
    /// columns, as integers ([`timestamp_literals`]): for a cross-mode
    /// query that names more than one file.
    #[cfg(all(
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    ))]
    pub(crate) fn resolve_timestamp_literals(
        &self,
        select: &mut Select,
    ) -> Result<(), EngineError> {
        timestamp_literals(select, &self.timestamps)
    }

    /// Parses one `SELECT` for this table; see [`parse_select`].
    fn parse_for_table(&self, sql: &str) -> Result<Select, EngineError> {
        let mut select = parse_select(sql, &self.table, &self.column_names(), "this file")?;
        timestamp_literals(&mut select, &self.timestamps)?;
        Ok(select)
    }
}

/// `key`'s values in `batch`, removed from it; `NULL`s for a column it
/// does not have.
#[cfg(any(
    feature = "ingest-read",
    all(
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    )
))]
pub(crate) fn take_values(batch: &mut Batch, key: &str) -> Vec<Value> {
    if let Some(values) = batch.columns.remove(key) {
        return Arc::unwrap_or_clone(values);
    }
    match batch.typed_columns.remove(key) {
        Some(column) => (0..batch.num_rows).map(|i| column.get(i)).collect(),
        None => vec![Value::Null; batch.num_rows],
    }
}

/// The `columns` of every segment, in segment order, as one [`Batch`].
#[cfg(any(
    feature = "ingest-read",
    all(
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    )
))]
pub(crate) fn merge_segments<S: Segment>(
    segments: &[S],
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

/// Parses one `SELECT` for the single table `table` with `columns`: the
/// batch grammar (analytics subset, aliases resolved), `*` expanded
/// against the columns, and the `FROM` table checked against the one table
/// there is (`what` names it in the error: "this file", "this directory").
fn parse_select(
    sql: &str,
    table: &str,
    columns: &[String],
    what: &str,
) -> Result<Select, EngineError> {
    let select = planner_parse(sql)?;
    let from = from_table(&select)?;
    if !from.eq_ignore_ascii_case(table) {
        return Err(EngineError::new(
            ErrorKind::Compile,
            format!("unknown table: {from} ({what} is table `{table}`)"),
        ));
    }
    let table_join = select.from.as_ref().and_then(|f| {
        f.joins
            .iter()
            .find(|j| !matches!(j.table.kind, TableRefKind::Unnest { .. }))
    });
    if let Some(join) = table_join {
        let right = join.table.name().unwrap_or("<subquery>");
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            format!("JOIN with `{right}`: a single-file engine has one table; use column-rs's multi-table session"),
        ));
    }
    // Anywhere in the query, not only as the whole WHERE: elsewhere it
    // used to compile to FALSE (#679).
    if planner::has_subquery_condition(&select) {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            "IN (SELECT ...) and EXISTS are not supported by a single-file engine",
        ));
    }
    if select.columns.iter().any(is_window_column) {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            "window functions are not available through the single-file engine yet",
        ));
    }
    planner::expand_star(&select, columns).map_err(plan_err)
}

/// Resolve the program's `columns_to_load()` against the file's leaves,
/// keeping each name's display form (possibly qualified) as the batch
/// column key.
fn resolve_columns(leaves: &[Leaf], names: &[String]) -> Result<Vec<Leaf>, EngineError> {
    let lookup: HashMap<&str, (usize, PhysicalType)> = leaves
        .iter()
        .map(|(n, i, t)| (n.as_str(), (*i, *t)))
        .collect();
    names
        .iter()
        .map(|name| {
            let (_, col) = planner::split_qualified(name);
            let (index, physical_type) = *lookup.get(col).ok_or_else(|| {
                EngineError::new(ErrorKind::Compile, format!("unknown column: {name}"))
            })?;
            Ok((name.clone(), index, physical_type))
        })
        .collect()
}

/// The file's top-level leaf columns as `(name, column_index,
/// physical_type)`, in file order. `column_index` counts leaves, so a
/// column after a group (struct, list, map) reads its own chunk; a group's
/// leaves are not columns of their own.
fn leaf_columns(file: &ParquetFile<'_>) -> Vec<Leaf> {
    file.fields()
        .into_iter()
        .filter_map(|f| {
            // A MAP (#621) is a column at its first leaf and a LIST (#801)
            // at its one leaf; they decode as map and list values (see
            // `load_leaf`). Other groups are not columns.
            let pt = match f.element.physical_type {
                Some(pt) => pt,
                None if is_map(&f) || is_list(&f) => PhysicalType::ByteArray,
                None => return None,
            };
            Some((f.name, f.leaves.start, pt))
        })
        .collect()
}

/// The file's integer columns (INT32 and INT64 but not a DECIMAL): they
/// decode as `Int` columns, so a `COUNT(DISTINCT)` of one can collect its
/// values (#817).
pub(crate) fn int_columns(file: &ParquetFile<'_>) -> Vec<String> {
    use crate::storage::parquet::footer::ConvertedType;
    file.fields()
        .into_iter()
        .filter(|f| {
            matches!(
                f.element.physical_type,
                Some(PhysicalType::Int32 | PhysicalType::Int64)
            ) && f.element.converted_type != Some(ConvertedType::Decimal)
        })
        .map(|f| f.name)
        .collect()
}

/// The file's UTF-8 string columns (BYTE_ARRAY annotated `UTF8`, not a
/// BLOB): they decode as strings, so a `COUNT(DISTINCT)` of one can
/// collect its values (#820).
pub(crate) fn str_columns(file: &ParquetFile<'_>) -> Vec<String> {
    use crate::storage::parquet::footer::ConvertedType;
    file.fields()
        .into_iter()
        .filter(|f| {
            f.element.physical_type == Some(PhysicalType::ByteArray)
                && f.element.converted_type == Some(ConvertedType::Other(0))
        })
        .map(|f| f.name)
        .collect()
}

/// The file's TIMESTAMP columns (`TIMESTAMP_MILLIS` / `TIMESTAMP_MICROS`)
/// and how many of their units make a second. They decode as plain
/// integers, so a quoted timestamp compared with one must become that
/// integer before planning ([`timestamp_literals`], #679).
fn timestamp_columns(file: &ParquetFile<'_>) -> Vec<(String, i64)> {
    use crate::storage::parquet::footer::ConvertedType;
    file.fields()
        .into_iter()
        .filter_map(|f| match f.element.converted_type {
            Some(ConvertedType::TimestampMillis) => Some((f.name, 1_000)),
            Some(ConvertedType::TimestampMicros) => Some((f.name, 1_000_000)),
            _ => f.element.timestamp_units.map(|units| (f.name, units)),
        })
        .collect()
}

/// Rewrites every comparison of a TIMESTAMP column with a quoted value
/// in WHERE and HAVING into a comparison with the column's integer units
/// (#679): `ts >= '2026-01-02'` compared the raw microseconds with text
/// and matched nothing. A quoted value that is no timestamp is an error,
/// as in DuckDB.
pub(crate) fn timestamp_literals(
    select: &mut Select,
    timestamps: &[(String, i64)],
) -> Result<(), EngineError> {
    if timestamps.is_empty() {
        return Ok(());
    }
    if let Some(e) = select.where_clause.as_mut() {
        rewrite_timestamp_literals(e, timestamps)?;
    }
    if let Some(e) = select.having.as_mut() {
        rewrite_timestamp_literals(e, timestamps)?;
    }
    Ok(())
}

fn rewrite_timestamp_literals(
    expr: &mut Expr,
    timestamps: &[(String, i64)],
) -> Result<(), EngineError> {
    let units = |e: &Expr| match &e.kind {
        ExprKind::Column { name, .. } => timestamps
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(name))
            .map(|(_, per_second)| *per_second),
        _ => None,
    };
    match &mut expr.kind {
        ExprKind::Binary {
            op:
                BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge,
            lhs,
            rhs,
        } => {
            let (lhs_units, rhs_units) = (units(lhs), units(rhs));
            for (per_second, literal) in [(lhs_units, &mut **rhs), (rhs_units, &mut **lhs)] {
                if let (Some(per_second), ExprKind::Literal(Literal::Str(text))) =
                    (per_second, &literal.kind)
                {
                    let value = timestamp_units(text, per_second).ok_or_else(|| {
                        EngineError::new(
                            ErrorKind::Compile,
                            format!("conversion error: '{text}' is not a timestamp"),
                        )
                    })?;
                    literal.kind = ExprKind::Literal(Literal::Integer(value));
                    break;
                }
            }
            Ok(())
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            rewrite_timestamp_literals(lhs, timestamps)?;
            rewrite_timestamp_literals(rhs, timestamps)
        }
        ExprKind::Unary { expr: inner, .. } | ExprKind::Paren(inner) => {
            rewrite_timestamp_literals(inner, timestamps)
        }
        _ => Ok(()),
    }
}

/// `text` (`YYYY-MM-DD`, optionally with `[T ]HH:MM[:SS[.fraction]]`, UTC)
/// in `per_second` units since the epoch; `None` if it is no timestamp.
fn timestamp_units(text: &str, per_second: i64) -> Option<i64> {
    let text = text.trim();
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    if !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // A literal names its own instant, so the clock is read only for
    // `'now'`: a host without one (`wasm32-unknown-unknown`) panics on it.
    let arg = [crate::value::Value::Text(whole.to_string().into())];
    let parsed = if whole.eq_ignore_ascii_case("now") {
        crate::functions::call("unixepoch", &arg)
    } else {
        crate::functions::call_at("unixepoch", &arg, 0)
    };
    let seconds = match parsed {
        Ok(crate::value::Value::Integer(s)) => s,
        _ => return None,
    };
    // The fraction to the unit's precision (truncated past it).
    let digits = per_second.checked_ilog10().unwrap_or(0) as usize;
    let mut sub: String = fraction.chars().take(digits).collect();
    while sub.len() < digits {
        sub.push('0');
    }
    let sub: i64 = if sub.is_empty() { 0 } else { sub.parse().ok()? };
    seconds.checked_mul(per_second)?.checked_add(sub)
}

/// A top-level `MAP` field of two leaves.
fn is_map(field: &crate::storage::parquet::Field) -> bool {
    field.nested
        && field.leaves.len() == 2
        && field.element.converted_type
            == Some(crate::storage::parquet::footer::ConvertedType::Other(1))
}

/// A top-level `LIST` field of one leaf.
fn is_list(field: &crate::storage::parquet::Field) -> bool {
    field.nested
        && field.leaves.len() == 1
        && field.element.converted_type
            == Some(crate::storage::parquet::footer::ConvertedType::Other(3))
}

/// One row group of a Parquet file as a lazily-loaded [`Segment`]: decodes
/// exactly the listed leaf columns into a [`Batch`] on `load()`. A decode
/// failure is an error, never a batch of NULLs (column-rs#27).
struct RowGroupSegment<'a, 'm> {
    file: &'m ParquetFile<'a>,
    row_group_index: usize,
    columns: Vec<Leaf>,
    /// The compiled program driving this scan (ADR-0026): when it has a
    /// `WHERE` clause with projection-only columns deferred past
    /// `Filter`, `load()` decodes the predicate columns first, runs the
    /// filter prefix, and decodes the remaining columns only at the
    /// surviving row positions -- instead of eagerly decoding every
    /// column for the whole row group.
    program: &'m Program,
    /// The single bare-column `ORDER BY` leaf, when the query has one
    /// (#464): lets [`Segment::order_key_bound`] answer from this row
    /// group's footer statistics for that column. `None` = unbounded.
    order_leaf: Option<(usize, PhysicalType)>,
    /// Columns (by batch key) whose `UINT_64` values read as their signed
    /// bit pattern instead of erroring above `i64::MAX` (#600): a join key
    /// is only compared for equality, where the wrapped value equals a
    /// SQLite row key stored the same way. Elsewhere such a value stays an
    /// error (#595).
    bit_pattern: Vec<String>,
}

/// #464: resolves a single bare-column `ORDER BY` term to its leaf so each
/// segment can bound its rows' order keys from footer statistics. Anything
/// else -- an expression, several terms, an unknown column -- leaves the
/// segments unbounded, i.e. always loaded.
fn order_leaf(order_by: &[OrderingTerm], all_leaves: &[Leaf]) -> Option<(usize, PhysicalType)> {
    let [term] = order_by else {
        return None;
    };
    let ExprKind::Column { name, .. } = &term.expr.kind else {
        return None;
    };
    all_leaves
        .iter()
        .find(|(leaf, _, _)| leaf == name)
        .map(|(_, index, physical_type)| (*index, *physical_type))
}

impl RowGroupSegment<'_, '_> {
    /// Whether `name` is a [`Self::bit_pattern`] column.
    fn bits(&self, name: &str) -> bool {
        self.bit_pattern.iter().any(|n| n == name)
    }

    /// Opens this segment's row group reader, or a [`VmError::SegmentLoad`]
    /// if the row group index is out of range.
    fn row_group(&self) -> Result<RowGroupReader<'_, '_>, VmError> {
        self.file
            .row_group(self.row_group_index)
            .ok_or_else(|| VmError::SegmentLoad {
                reason: format!(
                    "row group {} does not exist (file has {})",
                    self.row_group_index,
                    self.file.num_row_groups()
                ),
            })
    }

    /// Every row of `rg`, decoded eagerly: the pre-ADR-0026 behavior, used
    /// when the program has no `Filter` to defer projection-only columns
    /// past (GROUP BY / no-WHERE queries stay out of this ADR's scope).
    fn load_eager(&self, rg: &RowGroupReader<'_, '_>, num_rows: usize) -> Result<Batch, VmError> {
        self.decode_leaves(rg, num_rows, &self.columns)
    }

    /// Every row of `rg` for just `leaves`, decoded eagerly.
    fn decode_leaves(
        &self,
        rg: &RowGroupReader<'_, '_>,
        num_rows: usize,
        leaves: &[Leaf],
    ) -> Result<Batch, VmError> {
        let list_reads = ListReads::of(self.program);
        let mut batch = Batch::new(num_rows);
        for (name, index, physical_type) in leaves {
            let levels_only = list_reads.get(name);
            let decoded = load_leaf(rg, *index, *physical_type, self.bits(name), levels_only)
                .map_err(|e| segment_error(self.row_group_index, name, &e))?;
            batch = apply_loaded(batch, name, decoded);
        }
        Ok(batch)
    }

    /// This row group's row count as a `usize`.
    fn row_count(&self, rg: &RowGroupReader<'_, '_>) -> Result<usize, VmError> {
        usize::try_from(rg.num_rows()).map_err(|_| VmError::SegmentLoad {
            reason: format!(
                "row group {}: negative row count {}",
                self.row_group_index,
                rg.num_rows()
            ),
        })
    }

    /// The subset of this segment's leaves named in `columns`, in the
    /// segment's own column order; a name this segment does not load is
    /// a [`VmError::SegmentLoad`] rather than a silently missing column.
    fn leaves_named(&self, columns: &[String]) -> Result<Vec<Leaf>, VmError> {
        for name in columns {
            if !self.columns.iter().any(|(leaf, _, _)| leaf == name) {
                return Err(VmError::SegmentLoad {
                    reason: format!(
                        "row group {}: column `{name}` is not loaded by this segment",
                        self.row_group_index
                    ),
                });
            }
        }
        Ok(self
            .columns
            .iter()
            .filter(|(leaf, _, _)| columns.contains(leaf))
            .cloned()
            .collect())
    }

    fn gather_leaves_at(&self, columns: &[String], positions: &[u32]) -> Result<Batch, VmError> {
        let rg = self.row_group()?;
        let list_reads = ListReads::of(self.program);
        let mut batch = Batch::new(positions.len());
        for (name, index, physical_type) in self.leaves_named(columns)? {
            let levels_only = list_reads.get(&name);
            let decoded = load_leaf_at(
                &rg,
                index,
                physical_type,
                positions,
                self.bits(&name),
                levels_only,
            )
            .map_err(|e| segment_error(self.row_group_index, &name, &e))?;
            batch = apply_loaded(batch, &name, decoded);
        }
        Ok(batch)
    }
}

impl Segment for RowGroupSegment<'_, '_> {
    /// The `ORDER BY` column's footer `max` (descending) or `min`
    /// (ascending) for this row group, decoded like #458's WHERE pruning
    /// decodes the same bytes. Missing, unparseable or unsupported-type
    /// statistics are `None`: the segment is then simply loaded.
    fn order_key_bound(&self, descending: bool) -> Option<Value> {
        let (index, physical_type) = self.order_leaf?;
        let rg = self.file.row_group(self.row_group_index)?;
        let stats = rg.column_statistics(index).ok()??;
        let bytes = if descending {
            stats.max.as_deref()
        } else {
            stats.min.as_deref()
        }?;
        if rg.nested_field_at(index).is_some() {
            return None;
        }
        prune::decode_stat_bytes(bytes, physical_type, annotation(&rg, index))
            .map(prune::into_value)
    }

    /// #513: only the named leaves, every row, eagerly -- the
    /// late-materializing top-N's first phase runs its own filter prefix
    /// over this batch, so the ADR-0026 split does not apply here.
    fn load_columns(&self, columns: &[String]) -> Result<Arc<Batch>, VmError> {
        let rg = self.row_group()?;
        let num_rows = self.row_count(&rg)?;
        let leaves = self.leaves_named(columns)?;
        self.decode_leaves(&rg, num_rows, &leaves).map(Arc::new)
    }

    /// #513: the named leaves at exactly `positions` (row indices within
    /// this row group), through [`decode_column_at`] -- the same
    /// positional decoder ADR-0026's filtered projection uses, here for
    /// the handful of rows that won a top-N.
    fn gather_at(&self, columns: &[String], positions: &[u32]) -> Option<Result<Batch, VmError>> {
        Some(self.gather_leaves_at(columns, positions))
    }

    fn load(&self) -> Result<Arc<Batch>, VmError> {
        let rg = self.row_group()?;
        let num_rows = self.row_count(&rg)?;

        let predicate_names = self.program.predicate_columns();
        let projection_only_names = self.program.projection_only_columns();
        let prefix_opcodes = self.program.filter_prefix_opcodes();

        // No `Filter`, or no projection-only column deferred past it:
        // nothing to split, keep the single-pass eager decode (ADR-0026,
        // "no WHERE clause" / GROUP BY case explicitly out of scope).
        let Some(prefix_opcodes) = prefix_opcodes.filter(|_| !projection_only_names.is_empty())
        else {
            return self.load_eager(&rg, num_rows).map(Arc::new);
        };

        // Phase 1: decode only the predicate columns, for the whole row
        // group, and run the filter prefix to get the surviving positions.
        let predicate_leaves: Vec<&Leaf> = self
            .columns
            .iter()
            .filter(|(name, ..)| predicate_names.iter().any(|n| n == name))
            .collect();
        let list_reads = ListReads::of(self.program);
        let mut predicate_batch = Batch::new(num_rows);
        for (name, index, physical_type) in &predicate_leaves {
            let levels_only = list_reads.get(name);
            let decoded = load_leaf(&rg, *index, *physical_type, self.bits(name), levels_only)
                .map_err(|e| segment_error(self.row_group_index, name, &e))?;
            predicate_batch = apply_loaded(predicate_batch, name, decoded);
        }

        let mut vm = Vm::new();
        vm.execute(&predicate_batch, &prefix_opcodes)
            .map_err(|e| VmError::SegmentLoad {
                reason: format!("row group {}: predicate phase: {e}", self.row_group_index),
            })?;
        // `prefix_opcodes` always ends in `Opcode::Filter`, which always
        // sets `self.selection = Some(..)` when it runs (see its handler
        // in `vm::batch`); treat a `None` defensively as "every row
        // survived" rather than panicking, since that case is not
        // actually reachable here.
        let indices: Vec<u32> = vm.pending_selection_indices().map_or_else(
            || (0..u32::try_from(num_rows).unwrap_or(u32::MAX)).collect(),
            <[u32]>::to_vec,
        );

        // Phase 2a: gather the already-decoded predicate columns down to
        // the surviving positions (ordinary in-memory indexing).
        let mut batch = Batch::new(indices.len());
        for (name, ..) in &predicate_leaves {
            if let Some(values) = predicate_batch.columns.get(name.as_str()) {
                let gathered: Vec<Value> = indices
                    .iter()
                    .map(|&i| values.get(i as usize).cloned().unwrap_or(Value::Null))
                    .collect();
                batch = batch.with_column((*name).clone(), gathered);
            } else if let Some(column) = predicate_batch.typed_columns.get(name.as_str()) {
                let gathered: Vec<Value> =
                    indices.iter().map(|&i| column.get(i as usize)).collect();
                batch = batch.with_column((*name).clone(), gathered);
            }
        }

        // Phase 2b: decode the projection-only columns directly at the
        // surviving positions -- never materialized for rejected rows.
        let projection_leaves: Vec<&Leaf> = self
            .columns
            .iter()
            .filter(|(name, ..)| !predicate_names.iter().any(|n| n == name))
            .collect();
        for (name, index, physical_type) in &projection_leaves {
            // #704: no row survived, so the column is empty: its pages
            // are never read. It stays in the batch for `LoadColumn`.
            if indices.is_empty() {
                batch = batch.with_column(name.clone(), Vec::new());
                continue;
            }
            let levels_only = list_reads.get(name);
            let decoded = load_leaf_at(
                &rg,
                *index,
                *physical_type,
                &indices,
                self.bits(name),
                levels_only,
            )
            .map_err(|e| segment_error(self.row_group_index, name, &e))?;
            batch = apply_loaded(batch, name, decoded);
        }

        Ok(Arc::new(batch))
    }
}

/// One row group's error, tagged with the row group and column name --
/// shared by the eager and two-phase decode paths.
fn segment_error(row_group_index: usize, column_name: &str, e: &impl std::fmt::Display) -> VmError {
    VmError::SegmentLoad {
        reason: format!("row group {row_group_index}: column `{column_name}`: {e}"),
    }
}

/// A leaf's rows as the engine loads them: a typed column, or values a
/// typed column cannot hold: a MAP field's (#621) or a DECIMAL's (#632).
enum Loaded {
    Typed(Decoded),
    Values(Vec<Value>),
}

/// Leaf `index`'s `UINT_64` values as their signed bit patterns (#600): a
/// value above `i64::MAX` is a negative integer, equal to the SQLite row
/// key stored the same way.
fn uint64_bit_pattern(rg: &RowGroupReader<'_, '_>, index: usize) -> Result<Vec<Value>, FileError> {
    Ok(rg
        .read_int64_column(index)?
        .into_iter()
        .map(|v| v.map_or(Value::Null, Value::Int))
        .collect())
}

/// Leaf `index`'s DECIMAL values, exact (#632).
fn decimal_values(rg: &RowGroupReader<'_, '_>, index: usize) -> Result<Vec<Value>, FileError> {
    rg.read_decimal_column(index)?
        .into_iter()
        .map(|d| {
            let Some(d) = d else {
                return Ok(Value::Null);
            };
            let scale = u8::try_from(d.scale).ok().filter(|s| *s <= 38).ok_or(
                FileError::InvalidMetadata {
                    field: "DECIMAL scale",
                    value: i64::from(d.scale),
                },
            )?;
            Ok(Value::Decimal {
                unscaled: d.unscaled,
                scale,
            })
        })
        .collect()
}

/// How a LIST column is read: all its elements, only whether each list is
/// NULL (#810), or flat -- one offsets vector and one dictionary column of
/// elements, for a column only `UNNEST` reads (#809).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListRead {
    Full,
    LevelsOnly,
    Flat,
}

/// The columns a program reads as [`ListRead::LevelsOnly`] or
/// [`ListRead::Flat`]; every other column reads in full.
struct ListReads {
    null_only: Vec<String>,
    unnest_only: Vec<String>,
}

impl ListReads {
    fn of(program: &Program) -> Self {
        Self {
            null_only: program.null_only_columns(),
            unnest_only: program.unnest_only_columns(),
        }
    }

    fn get(&self, name: &str) -> ListRead {
        if self.null_only.iter().any(|c| c == name) {
            ListRead::LevelsOnly
        } else if self.unnest_only.iter().any(|c| c == name) {
            ListRead::Flat
        } else {
            ListRead::Full
        }
    }
}

/// A flat list column of `there.len()` rows, each empty, valid where
/// `there` is `true` (#846).
fn presence_column(there: &[bool]) -> Column {
    Column::List {
        offsets: vec![0; there.len().saturating_add(1)],
        elements: Box::new(Column::Str {
            offsets: vec![0],
            data: String::new(),
            valid: Bitmap::from_bools(std::iter::empty()),
        }),
        valid: Bitmap::from_bools(there.iter().copied()),
    }
}

/// Leaf `index` decoded for every row: a MAP field (at its first leaf) as
/// map values, a LIST field as list values, a DECIMAL as exact decimals, anything else as its typed
/// column.
fn load_leaf(
    rg: &RowGroupReader<'_, '_>,
    index: usize,
    physical_type: PhysicalType,
    bit_pattern: bool,
    list: ListRead,
) -> Result<Loaded, FileError> {
    match rg.nested_field_at(index) {
        Some(field) if is_list(&field) && list == ListRead::Flat => rg
            .read_list_utf8_flat(&field)
            .map(|column| Loaded::Typed(Decoded::Column(column))),
        // #846: only NULL-ness is read, so the lists are a flat column of
        // empty ones -- offsets and a validity bitmap, about 5 bytes a row
        // rather than a `Value` (32) each.
        Some(field) if is_list(&field) && list == ListRead::LevelsOnly => rg
            .read_list_utf8_presence(&field)
            .map(|there| Loaded::Typed(Decoded::Column(presence_column(&there)))),
        Some(field) => nested_values(rg, &field, list == ListRead::LevelsOnly).map(Loaded::Values),
        None if annotation(rg, index) == Annotation::Decimal => {
            decimal_values(rg, index).map(Loaded::Values)
        }
        None if bit_pattern && annotation(rg, index) == Annotation::UInt64 => {
            uint64_bit_pattern(rg, index).map(Loaded::Values)
        }
        None => decode_column_full(rg, index, physical_type).map(Loaded::Typed),
    }
}

/// [`load_leaf`] at `positions` only.
fn load_leaf_at(
    rg: &RowGroupReader<'_, '_>,
    index: usize,
    physical_type: PhysicalType,
    positions: &[u32],
    bit_pattern: bool,
    list: ListRead,
) -> Result<Loaded, FileError> {
    let all = match rg.nested_field_at(index) {
        Some(field) => Some(nested_values(rg, &field, list == ListRead::LevelsOnly)?),
        None if bit_pattern && annotation(rg, index) == Annotation::UInt64 => {
            Some(uint64_bit_pattern(rg, index)?)
        }
        None if annotation(rg, index) == Annotation::Decimal => Some(decimal_values(rg, index)?),
        None => None,
    };
    match all {
        Some(all) => positions
            .iter()
            .map(|&p| {
                usize::try_from(p)
                    .ok()
                    .and_then(|i| all.get(i).cloned())
                    .ok_or(FileError::RowPositionOutOfRange(p))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Loaded::Values),
        None => decode_column_at(rg, index, physical_type, positions).map(Loaded::Typed),
    }
}

/// Every row of LIST field `field` as [`Value::List`], and of any other
/// nested field as a MAP's [`Value::Map`] (NULL for a NULL list or map).
fn nested_values(
    rg: &RowGroupReader<'_, '_>,
    field: &Field,
    levels_only: bool,
) -> Result<Vec<Value>, FileError> {
    if is_list(field) && levels_only {
        // #810: only NULL-ness is read, so a list that is there is one
        // shared empty list, and no element is decoded.
        let there = Arc::new(Vec::new());
        return Ok(rg
            .read_list_utf8_presence(field)?
            .into_iter()
            .map(|has_list| {
                if has_list {
                    Value::List(Arc::clone(&there))
                } else {
                    Value::Null
                }
            })
            .collect());
    }
    if is_list(field) {
        return Ok(rg
            .read_list_utf8_column(field)?
            .into_iter()
            .map(|l| l.map_or(Value::Null, |items| Value::List(Arc::new(items))))
            .collect());
    }
    Ok(rg
        .read_map_utf8_column(field)?
        .into_iter()
        .map(|m| m.map_or(Value::Null, |entries| Value::Map(Arc::new(entries))))
        .collect())
}

/// Applies one leaf's loaded rows onto `batch` under `name`.
fn apply_loaded(batch: Batch, name: &str, loaded: Loaded) -> Batch {
    match loaded {
        Loaded::Typed(Decoded::Column(column)) => batch.with_typed_column(name.to_string(), column),
        Loaded::Values(values) => batch.with_column(name.to_string(), values),
    }
}

/// Builds one [`RowGroupSegment`] per row group, skipping any row group
/// that `where_clause`'s footer statistics prove cannot contain a matching
/// row (#458). Pruning is correctness-neutral: `prune::definitely_empty`
/// only returns `true` when it can prove no row in the group would survive
/// the filter that `vm::engine::run` would apply anyway, so omitting the
/// segment entirely is equivalent to including it and filtering every row
/// out (already an established invariant -- #404's segment-split
/// invariance test covers dropping all-empty segments).
fn row_group_segments<'f>(
    file: &'f ParquetFile<'f>,
    columns: &[Leaf],
    all_leaves: &[Leaf],
    where_clause: Option<&Expr>,
    program: &'f Program,
    bit_pattern: &[String],
) -> Vec<RowGroupSegment<'f, 'f>> {
    let leaves_by_name: HashMap<&str, (usize, PhysicalType)> = all_leaves
        .iter()
        .map(|(n, i, t)| (n.as_str(), (*i, *t)))
        .collect();
    (0..file.num_row_groups())
        .filter(|&i| match (where_clause, file.row_group(i)) {
            (Some(expr), Some(rg)) => !prune::definitely_empty(expr, &rg, &leaves_by_name),
            _ => true,
        })
        .map(|i| RowGroupSegment {
            file,
            row_group_index: i,
            columns: columns.to_vec(),
            program,
            order_leaf: None,
            bit_pattern: bit_pattern.to_vec(),
        })
        .collect()
}

/// A row group of a Parquet file, for callers that run the batch engine
/// themselves over a file, as column-rs does (#699): `load()` runs
/// `program`'s late materialization (ADR-0026) where it has a `WHERE`,
/// and `gather_at` serves the late-materializing top-N (#513). See
/// [`file_segments`].
pub struct FileSegment<'a, 'm>(RowGroupSegment<'a, 'm>);

impl Segment for FileSegment<'_, '_> {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        self.0.load()
    }

    fn order_key_bound(&self, descending: bool) -> Option<Value> {
        self.0.order_key_bound(descending)
    }

    fn load_columns(&self, columns: &[String]) -> Result<Arc<Batch>, VmError> {
        self.0.load_columns(columns)
    }

    fn gather_at(&self, columns: &[String], positions: &[u32]) -> Option<Result<Batch, VmError>> {
        self.0.gather_at(columns, positions)
    }
}

/// `file`'s segments loading `columns`, one per row group. A name `t.col`
/// is the file's column `col` and keys the batches as given; a name the
/// file has no column for is an error. `program` drives late
/// materialization where it has a `WHERE` (ADR-0026); an empty program
/// loads every column eagerly. A row group that `where_clause`'s footer
/// statistics prove cannot match is left out (#458, #747); `None` keeps
/// them all. Pass the query's own clause: pruning only drops groups the
/// filter would empty anyway.
pub fn file_segments<'f>(
    file: &'f ParquetFile<'f>,
    columns: &[String],
    where_clause: Option<&Expr>,
    program: &'f Program,
) -> Result<Vec<FileSegment<'f, 'f>>, EngineError> {
    file_segments_with_bit_patterns(file, columns, where_clause, program, &[])
}

/// [`file_segments`] where the `bit_pattern` columns (batch keys, a subset
/// of `columns`) read a `UINT_64` above `i64::MAX` as its signed bit
/// pattern rather than an error (#600), for join keys.
pub fn file_segments_with_bit_patterns<'f>(
    file: &'f ParquetFile<'f>,
    columns: &[String],
    where_clause: Option<&Expr>,
    program: &'f Program,
    bit_pattern: &[String],
) -> Result<Vec<FileSegment<'f, 'f>>, EngineError> {
    let all_leaves = leaf_columns(file);
    let leaves = resolve_columns(&all_leaves, columns)?;
    Ok(row_group_segments(
        file,
        &leaves,
        &all_leaves,
        where_clause,
        program,
        bit_pattern,
    )
    .into_iter()
    .map(FileSegment)
    .collect())
}

/// A segment of an ingested table (`table.rs`): a flushed row group or a
/// page. Here, with [`table_row_groups`], because it holds a
/// [`RowGroupSegment`] and its lifetimes.
#[cfg(feature = "ingest-read")]
enum TableSegment<'a, 'm> {
    RowGroup {
        inner: RowGroupSegment<'a, 'm>,
        fixups: Vec<table::Fixup>,
    },
    Page {
        segment: Arc<crate::storage::stream::segment::Segment>,
        requests: Vec<table::Request>,
        /// The table's format: how its strings are stored (#583).
        format: crate::storage::stream::detect::Format,
    },
    /// A record table's page of typed rows (#594).
    Rows {
        rows: Arc<crate::storage::ingest::RowPage>,
        requests: Vec<table::Request>,
    },
}

/// A segment of a table directory's [`Snapshot`](crate::storage::ingest::Snapshot)
/// (ADR 0029): a row group of one of its Parquet files, with the table's
/// NULL-fill and casts applied, or a page of the records above
/// `flushed_lsn`. For callers that run the batch engine themselves over a
/// table directory, as column-rs does for joins and window functions
/// (t-rust-db/tools#13); see [`snapshot_segments`] and
/// [`table::read_snapshot`].
#[cfg(feature = "ingest-read")]
pub struct SnapshotSegment<'a, 'm>(TableSegment<'a, 'm>);

#[cfg(feature = "ingest-read")]
impl SnapshotSegment<'_, '_> {
    /// Whether this is a row group of a file, not a live page.
    #[cfg(all(
        feature = "ingest-read",
        feature = "engine-row",
        feature = "engine-stream",
        feature = "vm-batch"
    ))]
    pub(crate) fn is_row_group(&self) -> bool {
        matches!(self.0, TableSegment::RowGroup { .. })
    }
}

#[cfg(feature = "ingest-read")]
impl Segment for SnapshotSegment<'_, '_> {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        self.0.load()
    }

    fn order_key_bound(&self, descending: bool) -> Option<Value> {
        self.0.order_key_bound(descending)
    }

    fn load_columns(&self, columns: &[String]) -> Result<Arc<Batch>, VmError> {
        self.0.load_columns(columns)
    }

    fn gather_at(&self, columns: &[String], positions: &[u32]) -> Option<Result<Batch, VmError>> {
        self.0.gather_at(columns, positions)
    }
}

/// Every Parquet file of `snapshot`, oldest first, opened: what the
/// segments of [`snapshot_segments`] read.
#[cfg(feature = "ingest-read")]
pub fn snapshot_files(
    snapshot: &crate::storage::ingest::Snapshot,
) -> Result<Vec<ParquetFile<'_>>, EngineError> {
    open_files(snapshot.files())
}

/// The files of `snapshot` that `filter` may match a row of, oldest first,
/// opened (#777). A file it skips holds no row the predicate accepts.
#[cfg(feature = "ingest-read")]
pub fn snapshot_files_where<'s>(
    snapshot: &'s crate::storage::ingest::Snapshot,
    filter: &'s table::SnapshotFilter,
) -> Result<Vec<ParquetFile<'s>>, EngineError> {
    let keep = table::file_flags(snapshot, filter.schema(), Some(filter.expr()));
    open_files(
        snapshot
            .files()
            .zip(keep)
            .filter(|(_, keep)| *keep)
            .map(|(file, _)| file),
    )
}

#[cfg(feature = "ingest-read")]
fn open_files<'s>(
    files: impl Iterator<Item = (&'s crate::storage::ingest::FileEntry, &'s [u8])>,
) -> Result<Vec<ParquetFile<'s>>, EngineError> {
    files
        .map(|(entry, bytes)| {
            ParquetFile::open(bytes)
                .map_err(|e| EngineError::new(ErrorKind::Open, format!("{}: {e}", entry.path)))
        })
        .collect()
}

/// `snapshot`'s segments loading `columns`, in LSN order: one per row
/// group of each of `files` ([`snapshot_files`] of it), then one per page.
/// A column name resolves as in [`table::query_snapshot`] (`t.col` is the
/// column `col` unless the table has one named `t.col`) and keys the
/// batches as given. A row group the file lacks a column for loads it as
/// NULL, one that stores it with another type casts it, so every segment
/// has the table's schema ([`Snapshot::schema`]). `program` drives late
/// materialization where it has a `WHERE` (ADR 0026); an empty program
/// loads every column eagerly. No row group or page is pruned.
///
/// [`Snapshot::schema`]: crate::storage::ingest::Snapshot::schema
#[cfg(feature = "ingest-read")]
pub fn snapshot_segments<'f>(
    snapshot: &'f crate::storage::ingest::Snapshot,
    files: &'f [ParquetFile<'f>],
    columns: &[String],
    program: &'f Program,
) -> Result<Vec<SnapshotSegment<'f, 'f>>, EngineError> {
    segments_of(snapshot, files, columns, program, None, None, &[])
}

/// [`snapshot_segments`] where the `bit_pattern` columns (keys of
/// `columns`) read a `UINT_64` above `i64::MAX` as its signed bit pattern
/// instead of an error (#786): the join keys, which a join only compares
/// for equality against a SQLite row key stored the same way (#600).
#[cfg(feature = "ingest-read")]
pub fn snapshot_segments_with_bit_patterns<'f>(
    snapshot: &'f crate::storage::ingest::Snapshot,
    files: &'f [ParquetFile<'f>],
    columns: &[String],
    program: &'f Program,
    bit_pattern: &[String],
) -> Result<Vec<SnapshotSegment<'f, 'f>>, EngineError> {
    segments_of(snapshot, files, columns, program, None, None, bit_pattern)
}

/// [`snapshot_segments`] without the record pages `filter` proves hold no
/// row it accepts (#777), and without the row groups of the files whose
/// footer statistics prove they hold none (#772): a column a file lacks or
/// stores with another type is never used to prune. Pass the files of
/// [`snapshot_files_where`] for the same `filter`.
#[cfg(feature = "ingest-read")]
pub fn snapshot_segments_where<'f>(
    snapshot: &'f crate::storage::ingest::Snapshot,
    files: &'f [ParquetFile<'f>],
    columns: &[String],
    program: &'f Program,
    filter: &table::SnapshotFilter,
) -> Result<Vec<SnapshotSegment<'f, 'f>>, EngineError> {
    segments_of(
        snapshot,
        files,
        columns,
        program,
        Some(filter.expr()),
        Some(filter.expr()),
        &[],
    )
}

#[cfg(feature = "ingest-read")]
fn segments_of<'f>(
    snapshot: &'f crate::storage::ingest::Snapshot,
    files: &'f [ParquetFile<'f>],
    columns: &[String],
    program: &'f Program,
    row_group_where: Option<&Expr>,
    page_where: Option<&Expr>,
    bit_pattern: &[String],
) -> Result<Vec<SnapshotSegment<'f, 'f>>, EngineError> {
    let schema = snapshot
        .schema()
        .map_err(|e| EngineError::new(ErrorKind::Open, e))?;
    let requests = table::resolve(&schema, columns, bit_pattern)?;
    let mut segments: Vec<SnapshotSegment<'f, 'f>> = Vec::new();
    for file in files {
        let (leaves, comparable, fixups) =
            table::file_plan(&leaf_columns(file), &schema, &requests);
        // Footer statistics prune through the leaves stored with the
        // table's type only; the others never prune (#772).
        segments.extend(
            row_group_segments(
                file,
                &leaves,
                &comparable,
                row_group_where,
                program,
                bit_pattern,
            )
            .into_iter()
            .map(|inner| {
                SnapshotSegment(TableSegment::RowGroup {
                    inner,
                    fixups: fixups.clone(),
                })
            }),
        );
    }
    segments.extend(
        table::page_segments(snapshot, &schema, &requests, page_where)
            .into_iter()
            .map(SnapshotSegment),
    );
    Ok(segments)
}

/// `file`'s row groups as [`TableSegment`]s decoding `columns` and applying
/// `fixups`. Footer statistics prune and bound the top-N through the
/// `comparable` leaves only.
#[cfg(feature = "ingest-read")]
fn table_row_groups<'f>(
    file: &'f ParquetFile<'f>,
    columns: &[Leaf],
    comparable: &[Leaf],
    select: &Select,
    program: &'f Program,
    fixups: &[table::Fixup],
) -> Vec<TableSegment<'f, 'f>> {
    let bound = order_leaf(&select.order_by, comparable);
    row_group_segments(
        file,
        columns,
        comparable,
        select.where_clause.as_ref(),
        program,
        &[],
    )
    .into_iter()
    .map(|mut inner| {
        inner.order_leaf = bound;
        TableSegment::RowGroup {
            inner,
            fixups: fixups.to_vec(),
        }
    })
    .collect()
}

/// Row-group pruning from footer [`Statistics`](footer::Statistics):
/// deciding whether a `WHERE` clause can be proven to match no row in a
/// row group without reading its data pages.
mod prune {
    use super::{
        annotation, Annotation, BinaryOp, Expr, ExprKind, HashMap, Literal, Ordering, PhysicalType,
        RowGroupReader, Value,
    };

    /// A value decoded from footer statistics or a `WHERE`-clause literal,
    /// typed so cross-type comparisons (e.g. an integer literal against a
    /// DOUBLE column) still work.
    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum StatValue {
        Int(i64),
        Float(f64),
        Str(String),
    }

    /// The VM-side [`Value`] of a decoded statistic, so a top-N bound
    /// (#464) compares through the same `compare_for_order` as the heap
    /// keys it is held against.
    pub(super) fn into_value(stat: StatValue) -> Value {
        match stat {
            StatValue::Int(i) => Value::Int(i),
            StatValue::Float(f) => Value::Float(f),
            StatValue::Str(s) => Value::Str(s.into()),
        }
    }

    fn compare(a: &StatValue, b: &StatValue) -> Option<Ordering> {
        match (a, b) {
            (StatValue::Int(x), StatValue::Int(y)) => Some(x.cmp(y)),
            (StatValue::Float(x), StatValue::Float(y)) => x.partial_cmp(y),
            (StatValue::Int(x), StatValue::Float(y)) => (*x as f64).partial_cmp(y),
            (StatValue::Float(x), StatValue::Int(y)) => x.partial_cmp(&(*y as f64)),
            (StatValue::Str(x), StatValue::Str(y)) => Some(x.cmp(y)),
            _ => None,
        }
    }

    /// Decodes one column chunk's raw min/max statistics bytes, typed by
    /// the column's physical type and annotation. `None` for a type this
    /// pruner doesn't interpret (e.g. INT96, FIXED_LEN_BYTE_ARRAY, DECIMAL,
    /// binary), an unsigned bound above `i64::MAX`, or malformed bytes --
    /// always falls back to "cannot prune", never misreads them (#595: an
    /// unsigned bound read signed would wrap negative and prune matches).
    pub(super) fn decode_stat_bytes(
        bytes: &[u8],
        physical_type: PhysicalType,
        annotation: Annotation,
    ) -> Option<StatValue> {
        match annotation {
            Annotation::Plain => {}
            Annotation::UInt32 => {
                let arr: [u8; 4] = bytes.try_into().ok()?;
                return Some(StatValue::Int(i64::from(u32::from_le_bytes(arr))));
            }
            Annotation::UInt64 => {
                let arr: [u8; 8] = bytes.try_into().ok()?;
                return i64::try_from(u64::from_le_bytes(arr))
                    .ok()
                    .map(StatValue::Int);
            }
            Annotation::Decimal | Annotation::Bytes => return None,
        }
        match physical_type {
            PhysicalType::Int32 => {
                let arr: [u8; 4] = bytes.try_into().ok()?;
                Some(StatValue::Int(i64::from(i32::from_le_bytes(arr))))
            }
            PhysicalType::Int64 => {
                let arr: [u8; 8] = bytes.try_into().ok()?;
                Some(StatValue::Int(i64::from_le_bytes(arr)))
            }
            PhysicalType::Float => {
                let arr: [u8; 4] = bytes.try_into().ok()?;
                Some(StatValue::Float(f64::from(f32::from_le_bytes(arr))))
            }
            PhysicalType::Double => {
                let arr: [u8; 8] = bytes.try_into().ok()?;
                Some(StatValue::Float(f64::from_le_bytes(arr)))
            }
            PhysicalType::ByteArray => std::str::from_utf8(bytes)
                .ok()
                .map(str::to_string)
                .map(StatValue::Str),
            _ => None,
        }
    }

    pub(super) fn literal_stat_value(
        lit: &Literal,
        physical_type: PhysicalType,
    ) -> Option<StatValue> {
        match (lit, physical_type) {
            (Literal::Integer(i), PhysicalType::Int32 | PhysicalType::Int64) => {
                Some(StatValue::Int(*i))
            }
            (Literal::Integer(i), PhysicalType::Float | PhysicalType::Double) => {
                Some(StatValue::Float(*i as f64))
            }
            (Literal::Float(f), PhysicalType::Float | PhysicalType::Double) => {
                Some(StatValue::Float(*f))
            }
            (Literal::Str(s), PhysicalType::ByteArray) => Some(StatValue::Str(s.clone())),
            _ => None,
        }
    }

    /// If exactly one side is a bare column reference and the other a
    /// literal, returns `(column_name, literal, flipped)` -- `flipped` is
    /// `true` when the literal was on the left (`5 < amount`), so the
    /// caller can flip the operator to read as `column OP literal`.
    fn as_column_and_literal<'e>(
        lhs: &'e Expr,
        rhs: &'e Expr,
    ) -> Option<(&'e str, &'e Literal, bool)> {
        match (&lhs.kind, &rhs.kind) {
            (ExprKind::Column { name, .. }, ExprKind::Literal(lit)) => Some((name, lit, false)),
            (ExprKind::Literal(lit), ExprKind::Column { name, .. }) => Some((name, lit, true)),
            _ => None,
        }
    }

    fn flip(op: BinaryOp) -> BinaryOp {
        match op {
            BinaryOp::Lt => BinaryOp::Gt,
            BinaryOp::Le => BinaryOp::Ge,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::Ge => BinaryOp::Le,
            other => other,
        }
    }

    /// Where a column's bounds come from: a row group's footer statistics,
    /// or (#589) a file's manifest key ranges.
    pub(super) trait Bounds {
        /// `column`'s smallest and largest value (`None` where unknown), and
        /// `literal` typed to compare with them; `None` if this source
        /// cannot bound the column against the literal at all.
        fn bounds(
            &self,
            column: &str,
            literal: &Literal,
        ) -> Option<(Option<StatValue>, Option<StatValue>, StatValue)>;

        /// Every value `column` can hold, if known exactly (a dictionary).
        fn values(&self, _column: &str) -> Option<Vec<StatValue>> {
            None
        }
    }

    /// A row group's footer statistics and dictionaries.
    struct RowGroupBounds<'a, 'r, 'f> {
        rg: &'a RowGroupReader<'r, 'f>,
        leaves_by_name: &'a HashMap<&'a str, (usize, PhysicalType)>,
    }

    impl Bounds for RowGroupBounds<'_, '_, '_> {
        fn bounds(
            &self,
            column: &str,
            literal: &Literal,
        ) -> Option<(Option<StatValue>, Option<StatValue>, StatValue)> {
            let (index, physical_type) = *self.leaves_by_name.get(column)?;
            let literal = literal_stat_value(literal, physical_type)?;
            // A MAP's leaves are keys and values, a LIST's its elements:
            // their statistics bound neither.
            if self.rg.nested_field_at(index).is_some() {
                return None;
            }
            let annotation = annotation(self.rg, index);
            let statistics = self.rg.column_statistics(index).ok().flatten();
            let decode = |b: &Option<Vec<u8>>| {
                b.as_deref()
                    .and_then(|b| decode_stat_bytes(b, physical_type, annotation))
            };
            let (min, max) = statistics.map_or((None, None), |s| (decode(&s.min), decode(&s.max)));
            Some((min, max, literal))
        }

        /// The chunk's dictionary, for plain strings and integers, if every
        /// data page is dictionary-encoded; any error reads as unknown.
        /// Asked only once [`Self::bounds`] bounds the column, which a MAP
        /// leaf never is.
        fn values(&self, column: &str) -> Option<Vec<StatValue>> {
            let (index, physical_type) = *self.leaves_by_name.get(column)?;
            if annotation(self.rg, index) != Annotation::Plain {
                return None;
            }
            match physical_type {
                PhysicalType::ByteArray => {
                    let values = self.rg.read_string_column_dictionary(index).ok()??;
                    Some(values.into_iter().map(StatValue::Str).collect())
                }
                PhysicalType::Int64 => {
                    let values = self.rg.read_int64_column_dictionary(index).ok()??;
                    Some(values.into_iter().map(StatValue::Int).collect())
                }
                PhysicalType::Int32 => {
                    let values = self.rg.read_int32_column_dictionary(index).ok()??;
                    Some(
                        values
                            .into_iter()
                            .map(|v| StatValue::Int(i64::from(v)))
                            .collect(),
                    )
                }
                _ => None,
            }
        }
    }

    /// Whether no value within `min..=max` satisfies `value op literal`.
    fn range_definitely_empty(
        op: BinaryOp,
        literal: &StatValue,
        min: Option<&StatValue>,
        max: Option<&StatValue>,
    ) -> bool {
        match op {
            BinaryOp::Eq => {
                let below_min = min
                    .and_then(|m| compare(literal, m))
                    .is_some_and(|o| o == Ordering::Less);
                let above_max = max
                    .and_then(|m| compare(literal, m))
                    .is_some_and(|o| o == Ordering::Greater);
                below_min || above_max
            }
            BinaryOp::Lt => min
                .and_then(|m| compare(m, literal))
                .is_some_and(|o| o != Ordering::Less),
            BinaryOp::Le => min
                .and_then(|m| compare(m, literal))
                .is_some_and(|o| o == Ordering::Greater),
            BinaryOp::Gt => max
                .and_then(|m| compare(m, literal))
                .is_some_and(|o| o != Ordering::Greater),
            BinaryOp::Ge => max
                .and_then(|m| compare(m, literal))
                .is_some_and(|o| o == Ordering::Less),
            _ => false,
        }
    }

    /// Whether `column = literal` matches no row: the source knows every
    /// value the column holds, and none equals the literal.
    fn absent_from_values(source: &impl Bounds, column: &str, literal: &StatValue) -> bool {
        source.values(column).is_some_and(|values| {
            !values
                .iter()
                .any(|v| compare(v, literal) == Some(Ordering::Equal))
        })
    }

    fn comparison_definitely_empty(
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        source: &impl Bounds,
    ) -> bool {
        let Some((column, lit, flipped)) = as_column_and_literal(lhs, rhs) else {
            return false;
        };
        let op = if flipped { flip(op) } else { op };
        let Some((min, max, literal)) = source.bounds(column, lit) else {
            return false;
        };
        range_definitely_empty(op, &literal, min.as_ref(), max.as_ref())
            || (op == BinaryOp::Eq && absent_from_values(source, column, &literal))
    }

    /// Whether `expr` can be proven to match no row `source` bounds.
    /// Conservative by construction: any expression shape or missing
    /// bound this function doesn't specifically recognize falls through to
    /// `false` ("may match"), never `true`.
    pub(super) fn definitely_empty_in(expr: &Expr, source: &impl Bounds) -> bool {
        match &expr.kind {
            ExprKind::Paren(inner) => definitely_empty_in(inner, source),
            ExprKind::Binary {
                op: BinaryOp::And,
                lhs,
                rhs,
            } => definitely_empty_in(lhs, source) || definitely_empty_in(rhs, source),
            ExprKind::Binary {
                op: BinaryOp::Or,
                lhs,
                rhs,
            } => definitely_empty_in(lhs, source) && definitely_empty_in(rhs, source),
            ExprKind::Binary { op, lhs, rhs } => comparison_definitely_empty(*op, lhs, rhs, source),
            _ => false,
        }
    }

    /// Whether `expr` can be proven to match no row in `rg`, using only its
    /// footer statistics and, for `=`, its dictionaries.
    pub(super) fn definitely_empty(
        expr: &Expr,
        rg: &RowGroupReader<'_, '_>,
        leaves_by_name: &HashMap<&str, (usize, PhysicalType)>,
    ) -> bool {
        definitely_empty_in(expr, &RowGroupBounds { rg, leaves_by_name })
    }
}

fn planner_parse(sql: &str) -> Result<Select, EngineError> {
    crate::parser::parse(sql).map_err(|e: ParseError| EngineError::new(ErrorKind::Parse, e))
}

/// The plain table name a `SELECT` reads from; anything else (`FROM`-less,
/// subquery in `FROM`) is an unknown table, as in column-rs.
fn from_table(select: &Select) -> Result<&str, EngineError> {
    select
        .from
        .as_ref()
        .and_then(|f| f.first.name())
        .ok_or_else(|| EngineError::new(ErrorKind::Compile, "unknown table: <no table>"))
}

fn is_window_column(column: &ResultColumn) -> bool {
    match column {
        ResultColumn::Expr {
            expr:
                Expr {
                    kind: ExprKind::FunctionCall { tail, .. },
                    ..
                },
            ..
        } => matches!(tail.as_deref(), Some(t) if t.over.is_some()),
        _ => false,
    }
}

/// `PlanError` is the batch planner's; `Internal` is a planner bug, the
/// rest are the user's query.
fn plan_err(e: PlanError) -> EngineError {
    match e {
        PlanError::Internal(msg) => EngineError::new(
            ErrorKind::Execute,
            format!("planner invariant violated: {msg}"),
        ),
        other => EngineError::new(ErrorKind::Compile, other),
    }
}

/// Parquet's physical type as a declared-type text for [`ColumnInfo`].
/// Best-effort (#326): logical types (timestamps, decimals) are not
/// unfolded here.
fn type_name(pt: PhysicalType) -> String {
    match pt {
        PhysicalType::Boolean => "BOOLEAN",
        PhysicalType::Int32 => "INT32",
        PhysicalType::Int64 => "INT64",
        PhysicalType::Int96 => "INT96",
        PhysicalType::Float => "FLOAT",
        PhysicalType::Double => "DOUBLE",
        PhysicalType::ByteArray => "BYTE_ARRAY",
        PhysicalType::FixedLenByteArray => "FIXED_LEN_BYTE_ARRAY",
        PhysicalType::Unknown(_) => "",
    }
    .to_string()
}

/// `EXPLAIN QUERY PLAN` rows for `select` over a table of `stats`.
fn plan_rows(select: &Select, stats: &TableStats) -> Result<Vec<PlanRow>, EngineError> {
    let nodes = planner::explain(select, |_| stats.clone()).map_err(plan_err)?;
    Ok(nodes
        .into_iter()
        .map(|n| PlanRow {
            id: i64::from(n.id),
            parent: i64::from(n.parent),
            detail: n.detail,
        })
        .collect())
}

/// `EXPLAIN` sections for `select`.
fn opcode_sections(select: &Select) -> Result<Vec<OpcodeSection>, EngineError> {
    let sections = planner::explain_opcodes(select).map_err(plan_err)?;
    Ok(sections
        .into_iter()
        .map(|s| OpcodeSection {
            label: s.label,
            lane: "batch",
            rows: s
                .rows
                .into_iter()
                .map(|r| OpcodeRow {
                    addr: r.addr,
                    opcode: r.opcode.to_string(),
                    operands: r.operands,
                    comment: r.comment,
                    is_finalize: r.is_finalize,
                })
                .collect(),
        })
        .collect())
}

/// Query output as a [`QueryResult`] with `select`'s column names.
fn query_result(select: &Select, output: crate::vm::batch::QueryOutput) -> QueryResult {
    QueryResult {
        columns: planner::output_column_names(select),
        rows: output
            .into_rows()
            .into_iter()
            .map(|r| r.into_iter().map(Cell::from).collect())
            .collect(),
        ..Default::default()
    }
}

impl Engine for BatchEngine {
    fn open(path: &Path) -> Result<Self, EngineError> {
        let data = PosixVfs
            .open(path)
            .map_err(|e| EngineError::new(ErrorKind::Open, format!("{}: {e}", path.display())))?;
        let table = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("data")
            .to_string();
        Self::from_data(path.to_path_buf(), table, Data::Disk(data))
    }

    fn mode(&self) -> Mode {
        Mode::Batch
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        let file = self.file()?;
        let program: Program = planner::compile_with_distinct_columns(
            &select,
            &int_columns(&file),
            &str_columns(&file),
        )
        .map_err(plan_err)?;
        let columns = resolve_columns(&self.leaves, &program.columns_to_load())?;
        let mut segments = row_group_segments(
            &file,
            &columns,
            &self.leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        let bound_leaf = order_leaf(&select.order_by, &self.leaves);
        for segment in &mut segments {
            segment.order_leaf = bound_leaf;
        }
        let output = engine::run(&segments, &program)
            .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
        Ok(query_result(&select, output))
    }

    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        let stats = TableStats {
            row_groups: self.num_row_groups,
            rows: self.num_rows,
            source: None,
        };
        plan_rows(&select, &stats)
    }

    fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        opcode_sections(&select)
    }

    fn stats(&self) -> FileStats {
        FileStats::Batch {
            row_groups: self.num_row_groups,
            rows: self.num_rows,
        }
    }

    fn tables(&self) -> Result<Vec<TableInfo>, EngineError> {
        Ok(vec![TableInfo {
            name: self.table.clone(),
            columns: self
                .leaves
                .iter()
                .map(|(name, _, pt)| ColumnInfo {
                    name: name.clone(),
                    type_name: type_name(*pt),
                })
                .collect(),
        }])
    }
}

/// A column table of a cross-mode join (#598, #772): a Parquet file or a
/// table directory. The join entry points are written once over it.
#[cfg(all(
    feature = "engine-row",
    feature = "engine-stream",
    feature = "vm-batch"
))]
#[derive(Clone, Copy)]
pub(crate) enum ColumnSource<'e> {
    File(&'e BatchEngine),
    #[cfg(feature = "ingest-read")]
    Dir(&'e TableEngine),
}

/// A probe segment of either kind, so one join runs over both.
#[cfg(all(
    feature = "engine-row",
    feature = "engine-stream",
    feature = "vm-batch"
))]
pub(crate) enum ProbeSegment<'a, 'm> {
    File(FileSegment<'a, 'm>),
    #[cfg(feature = "ingest-read")]
    Dir(SnapshotSegment<'a, 'm>),
}

#[cfg(all(
    feature = "engine-row",
    feature = "engine-stream",
    feature = "vm-batch"
))]
impl ProbeSegment<'_, '_> {
    /// Whether this is a row group of a file, not a live page: what a
    /// join's footer-statistics pruning can keep or drop.
    pub(crate) fn is_row_group(&self) -> bool {
        match self {
            ProbeSegment::File(_) => true,
            #[cfg(feature = "ingest-read")]
            ProbeSegment::Dir(s) => s.is_row_group(),
        }
    }
}

#[cfg(all(
    feature = "engine-row",
    feature = "engine-stream",
    feature = "vm-batch"
))]
impl Segment for ProbeSegment<'_, '_> {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        match self {
            ProbeSegment::File(s) => s.load(),
            #[cfg(feature = "ingest-read")]
            ProbeSegment::Dir(s) => s.load(),
        }
    }

    fn order_key_bound(&self, descending: bool) -> Option<Value> {
        match self {
            ProbeSegment::File(s) => s.order_key_bound(descending),
            #[cfg(feature = "ingest-read")]
            ProbeSegment::Dir(s) => s.order_key_bound(descending),
        }
    }

    fn load_columns(&self, columns: &[String]) -> Result<Arc<Batch>, VmError> {
        match self {
            ProbeSegment::File(s) => s.load_columns(columns),
            #[cfg(feature = "ingest-read")]
            ProbeSegment::Dir(s) => s.load_columns(columns),
        }
    }

    fn gather_at(&self, columns: &[String], positions: &[u32]) -> Option<Result<Batch, VmError>> {
        match self {
            ProbeSegment::File(s) => s.gather_at(columns, positions),
            #[cfg(feature = "ingest-read")]
            ProbeSegment::Dir(s) => s.gather_at(columns, positions),
        }
    }
}

#[cfg(all(
    feature = "engine-row",
    feature = "engine-stream",
    feature = "vm-batch"
))]
impl ColumnSource<'_> {
    pub(crate) fn parse_join(&self, sql: &str) -> Result<Select, EngineError> {
        match self {
            ColumnSource::File(e) => e.parse_join(sql),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.parse_join(sql),
        }
    }

    pub(crate) fn resolve_timestamp_literals(
        &self,
        select: &mut Select,
    ) -> Result<(), EngineError> {
        match self {
            ColumnSource::File(e) => e.resolve_timestamp_literals(select),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.resolve_timestamp_literals(select),
        }
    }

    pub(crate) fn table_name(&self) -> &str {
        match self {
            ColumnSource::File(e) => e.table_name(),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.table_name(),
        }
    }

    pub(crate) fn columns(&self) -> Vec<String> {
        match self {
            ColumnSource::File(e) => e.columns(),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.columns(),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        match self {
            ColumnSource::File(e) => e.path(),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.path(),
        }
    }

    pub(crate) fn table_stats(&self) -> TableStats {
        match self {
            ColumnSource::File(e) => e.table_stats(),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.table_stats(),
        }
    }

    /// Row groups of the file or the archive files, live pages left out.
    pub(crate) fn row_groups(&self) -> usize {
        match self {
            ColumnSource::File(e) => e.table_stats().row_groups,
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.archive_row_groups(),
        }
    }

    /// The whole table's `columns` as one batch: a join's build side. The
    /// `bit_pattern` columns read a `UINT_64` above `i64::MAX` as its signed
    /// bit pattern (#600).
    pub(crate) fn read_table(
        &self,
        columns: &[String],
        bit_pattern: &[String],
    ) -> Result<Batch, EngineError> {
        match self {
            ColumnSource::File(e) => e.read_table(columns, bit_pattern),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.read_table(columns, bit_pattern),
        }
    }

    pub(crate) fn with_probe_segments<T>(
        &self,
        columns: &[String],
        prune: Option<&Expr>,
        bit_pattern: &[String],
        f: impl FnOnce(Vec<ProbeSegment<'_, '_>>) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        match self {
            ColumnSource::File(e) => {
                e.with_probe_segments(columns, prune, bit_pattern, |segments| {
                    f(segments.into_iter().map(ProbeSegment::File).collect())
                })
            }
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => {
                e.with_probe_segments(columns, prune, bit_pattern, |segments| {
                    f(segments.into_iter().map(ProbeSegment::Dir).collect())
                })
            }
        }
    }
}

/// The column tables of a Parquet session as join sources.
#[cfg(all(
    feature = "engine-row",
    feature = "engine-stream",
    feature = "vm-batch"
))]
pub(crate) fn file_sources<'a>(
    driving: &'a BatchEngine,
    others: &'a [BatchEngine],
) -> Vec<ColumnSource<'a>> {
    std::iter::once(driving)
        .chain(others.iter())
        .map(ColumnSource::File)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        engine, order_leaf, plan_err, planner, resolve_columns, row_group_segments, type_name,
        BatchEngine, Engine as _, ErrorKind, Leaf, ParquetFile, Path, PhysicalType, PlanError,
        Program, RowGroupSegment, Value,
    };
    use super::{file_segments, planner_parse, query_result};
    use super::{timestamp_literals, timestamp_units};
    use crate::parser::ast::Select;

    /// #679: a quoted date or timestamp in a column's units -- micro- and
    /// milliseconds, a fraction kept to the unit's precision -- and `None`
    /// for text that is no timestamp.
    #[test]
    fn timestamp_units_parse_dates_times_and_fractions() {
        let day = 1_767_225_600; // 2026-01-01T00:00:00Z
        assert_eq!(
            timestamp_units("2026-01-01", 1_000_000),
            Some(day * 1_000_000)
        );
        assert_eq!(
            timestamp_units("2026-01-01 10:00:00", 1_000_000),
            Some((day + 36_000) * 1_000_000)
        );
        assert_eq!(
            timestamp_units("2026-01-01T00:00:01.5", 1_000_000),
            Some((day + 1) * 1_000_000 + 500_000)
        );
        assert_eq!(
            timestamp_units("2026-01-01 00:00:00.123456789", 1_000),
            Some(day * 1_000 + 123)
        );
        assert_eq!(timestamp_units("not a time", 1_000_000), None);
        assert_eq!(timestamp_units("2026-01-01 00:00:00.12x", 1_000_000), None);
    }

    /// #679: comparisons of a TIMESTAMP column with a quoted value become
    /// integer comparisons, either side, in WHERE and HAVING; other
    /// columns and other shapes are left alone; a bad timestamp is an
    /// error.
    #[test]
    fn timestamp_literals_rewrite_comparisons_with_timestamp_columns() {
        let ts = [("ts".to_string(), 1_000_000)];
        let rewrite = |q: &str| {
            let mut select = crate::parser::column::parse(q).unwrap();
            timestamp_literals(&mut select, &ts).map(|()| select)
        };
        let select = rewrite(
            "SELECT count(*) FROM t WHERE ts >= '2026-01-01' AND ('2026-01-02' > ts OR s = '2026-01-01')",
        )
        .unwrap();
        let text = format!("{}", select.where_clause.unwrap());
        assert!(text.contains("1767225600000000"), "{text}");
        assert!(text.contains("1767312000000000"), "{text}");
        assert!(
            text.contains("'2026-01-01'"),
            "a non-timestamp column keeps its text: {text}"
        );
        let grouped = rewrite("SELECT s FROM t GROUP BY s HAVING max(ts) > '2026-01-01'").unwrap();
        assert!(
            format!("{}", grouped.having.unwrap()).contains("'2026-01-01'"),
            "an aggregate's column is not a bare column"
        );
        assert!(rewrite("SELECT count(*) FROM t WHERE ts = 'yesterday-ish'").is_err());
        assert!(timestamp_literals(
            &mut crate::parser::column::parse("SELECT a FROM t").unwrap(),
            &[]
        )
        .is_ok());
    }
    use crate::vm::batch::Segment as _;

    #[test]
    fn plan_err_maps_internal_to_a_planner_invariant_execute_error() {
        let e = plan_err(PlanError::Internal("unreachable branch".into()));
        assert_eq!(e.kind, ErrorKind::Execute);
        assert!(e.to_string().contains("planner invariant violated"), "{e}");
    }

    #[test]
    fn plan_err_maps_every_other_variant_to_a_compile_error() {
        let e = plan_err(PlanError::UnknownColumn("missing".into()));
        assert_eq!(e.kind, ErrorKind::Compile);
    }

    /// Minimal Thrift Compact Protocol struct encoder (mirrors
    /// `storage::parquet::footer`'s and `parquet_file`'s own
    /// test-only copies -- each test module keeps its own small one rather
    /// than sharing a `pub(crate)` surface just for tests).
    struct StructWriter {
        buf: Vec<u8>,
        last_field_id: i16,
    }

    impl StructWriter {
        fn new() -> Self {
            StructWriter {
                buf: Vec::new(),
                last_field_id: 0,
            }
        }
        fn write_varint(&mut self, mut v: u64) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                self.buf.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        fn zigzag(v: i64) -> u64 {
            ((v << 1) ^ (v >> 63)) as u64
        }
        fn field_header(&mut self, field_id: i16, ctype: u8) {
            let delta = field_id - self.last_field_id;
            assert!((1..=15).contains(&delta));
            self.buf.push(((delta as u8) << 4) | ctype);
            self.last_field_id = field_id;
        }
        fn i32_field(&mut self, field_id: i16, v: i32) {
            self.field_header(field_id, 0x05);
            self.write_varint(Self::zigzag(v as i64));
        }
        fn i64_field(&mut self, field_id: i16, v: i64) {
            self.field_header(field_id, 0x06);
            self.write_varint(Self::zigzag(v));
        }
        fn string_field(&mut self, field_id: i16, s: &str) {
            self.field_header(field_id, 0x08);
            self.write_varint(s.len() as u64);
            self.buf.extend_from_slice(s.as_bytes());
        }
        fn binary_field(&mut self, field_id: i16, bytes: &[u8]) {
            self.field_header(field_id, 0x08);
            self.write_varint(bytes.len() as u64);
            self.buf.extend_from_slice(bytes);
        }
        fn struct_field(&mut self, field_id: i16, inner: Vec<u8>) {
            self.field_header(field_id, 0x0c);
            self.buf.extend_from_slice(&inner);
        }
        fn list_of_structs_field(&mut self, field_id: i16, items: Vec<Vec<u8>>) {
            self.field_header(field_id, 0x09);
            let n = items.len();
            if n < 15 {
                self.buf.push(((n as u8) << 4) | 0x0c);
            } else {
                self.buf.push((15u8 << 4) | 0x0c);
                self.write_varint(n as u64);
            }
            for item in items {
                self.buf.extend_from_slice(&item);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            self.buf.push(0x00);
            self.buf
        }
    }

    fn build_page_header(num_values: i32, page_size: i32) -> Vec<u8> {
        let mut dph = StructWriter::new();
        dph.i32_field(1, num_values);
        dph.i32_field(2, 0); // encoding = PLAIN
        dph.i32_field(3, 3);
        dph.i32_field(4, 3);
        let dph_bytes = dph.finish();

        let mut w = StructWriter::new();
        w.i32_field(1, 0); // DATA_PAGE
        w.i32_field(2, page_size);
        w.i32_field(3, page_size);
        w.struct_field(5, dph_bytes);
        w.finish()
    }

    /// One DOUBLE row group of column `amount`, with `stats_bytes` (raw
    /// `Statistics` thrift bytes, or `None` for "no statistics") as its
    /// footer statistics -- independent of `values`, so tests can supply
    /// deliberately wrong or malformed statistics (#458's correctness
    /// requirement: pruning must never trust them into a wrong answer).
    fn build_double_row_group(
        values: &[f64],
        base_offset: i64,
        stats_bytes: Option<Vec<u8>>,
    ) -> (Vec<u8>, Vec<u8>) {
        let mut body = Vec::new();
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let header = build_page_header(values.len() as i32, body.len() as i32);
        let mut page_bytes = header;
        page_bytes.extend_from_slice(&body);

        let mut meta = StructWriter::new();
        meta.i32_field(1, 5); // DOUBLE
        meta.field_header(3, 0x09); // path_in_schema list<string>
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(6);
        meta.buf.extend_from_slice(b"amount");
        meta.i64_field(5, values.len() as i64);
        meta.i64_field(6, page_bytes.len() as i64);
        meta.i64_field(7, page_bytes.len() as i64);
        meta.i64_field(9, base_offset);
        if let Some(stats) = stats_bytes {
            meta.struct_field(12, stats);
        }
        let meta_bytes = meta.finish();
        (page_bytes, meta_bytes)
    }

    fn build_statistics(min_value: &[u8], max_value: &[u8]) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.binary_field(5, max_value);
        w.binary_field(6, min_value);
        w.finish()
    }

    /// A full single-column (`amount` DOUBLE, REQUIRED) file with one row
    /// group per `(values, stats_bytes)` pair.
    fn build_file(row_groups: &[(&[f64], Option<Vec<u8>>)]) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");

        let mut rg_thrift = Vec::new();
        let mut total_rows = 0i64;
        for (values, stats) in row_groups {
            let base_offset = file.len() as i64;
            let (page_bytes, meta_bytes) =
                build_double_row_group(values, base_offset, stats.clone());
            file.extend_from_slice(&page_bytes);

            let mut chunk = StructWriter::new();
            chunk.i64_field(2, base_offset);
            chunk.struct_field(3, meta_bytes);
            let chunk_bytes = chunk.finish();

            let mut rg = StructWriter::new();
            rg.list_of_structs_field(1, vec![chunk_bytes]);
            rg.i64_field(2, page_bytes.len() as i64);
            rg.i64_field(3, values.len() as i64);
            rg_thrift.push(rg.finish());
            total_rows += values.len() as i64;
        }

        let mut root = StructWriter::new();
        root.string_field(4, "schema");
        root.i32_field(5, 1);
        let root = root.finish();

        let mut col = StructWriter::new();
        col.i32_field(1, 5); // DOUBLE
        col.i32_field(3, 0); // REQUIRED
        col.string_field(4, "amount");
        let col = col.finish();

        let mut fmd = StructWriter::new();
        fmd.i32_field(1, 1);
        fmd.list_of_structs_field(2, vec![root, col]);
        fmd.i64_field(3, total_rows);
        fmd.list_of_structs_field(4, rg_thrift);
        fmd.string_field(6, "test");
        let metadata = fmd.finish();

        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        file
    }

    fn where_clause(sql: &str) -> Select {
        let stmt = format!("SELECT amount FROM t WHERE {sql}");
        crate::parser::parse(&stmt).expect("valid SQL")
    }

    fn amount_leaf() -> Vec<Leaf> {
        vec![("amount".to_string(), 0, PhysicalType::Double)]
    }

    fn order_by_select(sql: &str) -> Select {
        crate::parser::parse(sql).expect("valid SQL")
    }

    #[test]
    fn order_leaf_resolves_only_a_single_bare_column_term() {
        let leaves = amount_leaf();
        let one = order_by_select("SELECT amount FROM t ORDER BY amount DESC LIMIT 1");
        assert_eq!(
            order_leaf(&one.order_by, &leaves),
            Some((0, PhysicalType::Double))
        );
        // The parser admits exactly one bare column reference here, so the
        // remaining "no leaf" cases are a column the file doesn't have and
        // no ORDER BY at all.
        let unknown = order_by_select("SELECT amount FROM t ORDER BY other LIMIT 1");
        assert_eq!(order_leaf(&unknown.order_by, &leaves), None);
        let none = order_by_select("SELECT amount FROM t LIMIT 1");
        assert_eq!(order_leaf(&none.order_by, &leaves), None);
    }

    #[test]
    fn order_key_bound_is_the_footer_max_descending_and_min_ascending() {
        let stats = build_statistics(&1.0f64.to_le_bytes(), &5.0f64.to_le_bytes());
        let bogus = build_statistics(&[0, 0, 0], &[0, 0, 0]);
        let file_bytes = build_file(&[
            (&[1.0, 5.0], Some(stats)),
            (&[7.0], None),
            (&[2.0], Some(bogus)),
        ]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let leaves = amount_leaf();
        let program = Program::new(Vec::new());
        let mut segments = row_group_segments(&file, &leaves, &leaves, None, &program, &[]);
        assert_eq!(segments.len(), 3);
        // Unbounded until the ORDER BY leaf is known.
        assert_eq!(segments[0].order_key_bound(true), None);
        for segment in &mut segments {
            segment.order_leaf = Some((0, PhysicalType::Double));
        }
        assert_eq!(segments[0].order_key_bound(true), Some(Value::Float(5.0)));
        assert_eq!(segments[0].order_key_bound(false), Some(Value::Float(1.0)));
        assert_eq!(
            segments[1].order_key_bound(true),
            None,
            "no statistics: unknown"
        );
        assert_eq!(
            segments[2].order_key_bound(true),
            None,
            "malformed statistics: unknown"
        );
    }

    #[test]
    fn a_projection_only_column_is_not_read_when_no_row_survives() {
        // #704: `ghost` names a column chunk the file does not have, so
        // reading it fails -- which only happens when a row survives.
        let file_bytes = build_file(&[(&[1.0, 2.0], None)]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let leaves: Vec<Leaf> = vec![
            ("amount".to_string(), 0, PhysicalType::Double),
            ("ghost".to_string(), 99, PhysicalType::Int64),
        ];
        let load = |sql: &str| {
            let select = crate::parser::parse(sql).expect("valid SQL");
            let program: Program = planner::compile(&select).unwrap();
            let segment = RowGroupSegment {
                file: &file,
                row_group_index: 0,
                columns: leaves.clone(),
                program: &program,
                order_leaf: None,
                bit_pattern: Vec::new(),
            };
            segment.load().map(|batch| batch.num_rows)
        };
        assert_eq!(load("SELECT ghost FROM t WHERE amount > 100").unwrap(), 0);
        let err = load("SELECT ghost FROM t WHERE amount > 1").unwrap_err();
        assert!(err.to_string().contains("column `ghost`"), "{err}");
    }

    #[test]
    fn from_bytes_matches_open() {
        let path = Path::new("tests/fixtures/parquet/production.parquet");
        let bytes = std::fs::read(path).unwrap();
        let mut mem = BatchEngine::from_bytes("production", bytes).unwrap();
        let mut disk = BatchEngine::open(path).unwrap();
        let sql = "SELECT count(*) FROM production";
        assert_eq!(mem.table_name(), "production");
        assert_eq!(mem.run_query(sql).unwrap(), disk.run_query(sql).unwrap());
    }

    #[test]
    fn from_bytes_rejects_non_parquet() {
        let err = BatchEngine::from_bytes("t", b"not parquet".to_vec()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Open);
    }

    #[test]
    fn load_columns_and_gather_at_decode_only_the_named_leaves() {
        let path = Path::new("tests/fixtures/parquet/production.parquet");
        let engine = BatchEngine::open(path).unwrap();
        let file = engine.file().unwrap();
        let leaves = engine.leaves.clone();
        let program = Program::new(Vec::new());
        let segments = row_group_segments(&file, &leaves, &leaves, None, &program, &[]);
        let seg = &segments[2];
        let amount = vec!["amount".to_string()];
        let key_only = seg.load_columns(&amount).unwrap();
        assert_eq!(key_only.num_rows, 904);
        assert!(key_only.typed_columns.contains_key("amount"));
        assert!(
            !key_only.typed_columns.contains_key("id"),
            "id was not asked for"
        );

        let gathered = seg
            .gather_at(&["id".to_string(), "region".to_string()], &[903, 0, 903])
            .expect("row groups can always gather")
            .unwrap();
        assert_eq!(gathered.num_rows, 3);
        let full = seg.load().unwrap();
        for (k, &p) in [903u32, 0, 903].iter().enumerate() {
            for name in ["id", "region"] {
                assert_eq!(
                    crate::vm::batch::Column::get(&gathered.typed_columns[name], k),
                    crate::vm::batch::Column::get(&full.typed_columns[name], p as usize),
                    "{name} at position {p}"
                );
            }
        }
        let err = seg.load_columns(&["nosuch".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("not loaded by this segment"),
            "{err}"
        );
    }

    /// #747: a WHERE clause that the footer statistics rule out for all but
    /// one row group leaves one segment, and the query answers as it does
    /// over every group (the clause is qualified, as column-rs's is).
    #[test]
    fn file_segments_prune_row_groups_by_the_where_clause() {
        let stats = |lo: f64, hi: f64| Some(build_statistics(&lo.to_le_bytes(), &hi.to_le_bytes()));
        let file_bytes = build_file(&[
            (&[1.0, 2.0, 3.0], stats(1.0, 3.0)),
            (&[100.0, 150.0], stats(100.0, 150.0)),
            (&[500.0, 900.0], stats(500.0, 900.0)),
        ]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let select =
            planner_parse("SELECT t.amount FROM t WHERE t.amount > 120 AND t.amount < 600")
                .unwrap();
        let program: Program = planner::compile(&select).unwrap();
        let columns = program.columns_to_load();
        let run = |clause| {
            let segments = file_segments(&file, &columns, clause, &program).unwrap();
            let output = engine::run(&segments, &program).unwrap();
            (segments.len(), query_result(&select, output).rows)
        };
        let (all_groups, expected) = run(None);
        let (pruned_groups, rows) = run(select.where_clause.as_ref());
        assert_eq!(all_groups, 3);
        assert_eq!(pruned_groups, 2, "group 0 (max 3.0) cannot match");
        assert_eq!(rows, expected);
        assert_eq!(rows.len(), 2);

        let narrow = planner_parse("SELECT t.amount FROM t WHERE t.amount > 800").unwrap();
        let program: Program = planner::compile(&narrow).unwrap();
        let segments =
            file_segments(&file, &columns, narrow.where_clause.as_ref(), &program).unwrap();
        assert_eq!(segments.len(), 1);
    }

    #[test]
    fn file_segments_run_a_filtered_query_like_the_engine() {
        // #699: column-rs's file segments, keyed by the names as given
        // (`p.id` is the column `id`), answer as `run_query` does.
        let path = Path::new("tests/fixtures/parquet/production.parquet");
        let mut engine = BatchEngine::open(path).unwrap();
        let sql = "SELECT id, region FROM production WHERE amount > 900 ORDER BY id";
        let expected = engine.run_query(sql).unwrap().rows;
        assert!(!expected.is_empty());

        let file = engine.file().unwrap();
        let select = planner_parse(
            "SELECT p.id, p.region FROM production p WHERE p.amount > 900 ORDER BY p.id",
        )
        .unwrap();
        let program: Program = planner::compile(&select).unwrap();
        let segments = file_segments(
            &file,
            &program.columns_to_load(),
            select.where_clause.as_ref(),
            &program,
        )
        .unwrap();
        assert_eq!(segments.len(), file.num_row_groups());
        let output = engine::run(&segments, &program).unwrap();
        assert_eq!(query_result(&select, output).rows, expected);

        let err = file_segments(&file, &["p.nosuch".to_string()], None, &program)
            .err()
            .expect("an unknown column is an error");
        assert!(
            err.to_string().contains("unknown column: p.nosuch"),
            "{err}"
        );
    }

    /// #621: a MAP column reads as maps, rendered as DuckDB prints them,
    /// and `m['k']` and the map functions answer as in DuckDB 1.5
    /// (`map_get_ci` has no DuckDB equivalent).
    #[test]
    fn map_columns_answer_subscripts_and_map_functions() {
        let path = Path::new("tests/fixtures/parquet/nested_map.parquet");
        let mut engine = BatchEngine::open(path).unwrap();
        let rows = |engine: &mut BatchEngine, sql: &str| -> Vec<String> {
            engine
                .run_query(sql)
                .unwrap()
                .rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect()
        };
        assert_eq!(
            rows(
                &mut engine,
                "SELECT h, h['k'], element_at(h, 'k'), map_keys(h), cardinality(h) FROM nested_map"
            ),
            [
                "{User-Agent=curl/8, Accept=*/*}||[]|[User-Agent, Accept]|2",
                "||||",
                "{}||[]|[]|0",
                "{k=NULL}||[NULL]|[k]|1",
                "{héader=välue, k=v}|v|[v]|[héader, k]|2",
            ]
        );
        assert_eq!(
            rows(
                &mut engine,
                "SELECT id FROM nested_map WHERE map_get_ci(h, 'user-AGENT') = 'curl/8'"
            ),
            ["0"]
        );
        assert_eq!(
            rows(&mut engine, "SELECT id FROM nested_map WHERE h['k'] = 'v'"),
            ["4"]
        );
        assert_eq!(
            rows(
                &mut engine,
                "SELECT count(*) FROM nested_map WHERE h['k'] IS NULL"
            ),
            ["4"]
        );
        // The key leaf's statistics are of keys, not maps: never pruned on.
        assert_eq!(
            rows(&mut engine, "SELECT id FROM nested_map WHERE h > 'zzz'"),
            Vec::<String>::new()
        );
        let tables = engine.tables().unwrap();
        let names: Vec<&str> = tables[0].columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "h", "x"]);
    }

    /// A column after a MAP reads its own chunk, filters and prunes; the
    /// map's `key`/`value` leaves are not columns (before, `x` read leaf
    /// index 5 of 4 and `key` read `x`'s chunk as strings).
    #[test]
    fn columns_after_a_nested_field_read_their_own_chunk() {
        let path = Path::new("tests/fixtures/parquet/nested_map.parquet");
        let mut engine = BatchEngine::open(path).unwrap();
        let rows = |engine: &mut BatchEngine, sql: &str| -> Vec<String> {
            engine
                .run_query(sql)
                .unwrap()
                .rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect()
        };
        assert_eq!(
            rows(&mut engine, "SELECT id, x FROM nested_map WHERE x > 15"),
            ["2,20", "3,30", "4,40"]
        );
        assert_eq!(rows(&mut engine, "SELECT sum(x) FROM nested_map"), ["100"]);
        let err = engine.run_query("SELECT key FROM nested_map").unwrap_err();
        assert!(err.to_string().contains("unknown column: key"), "{err}");
    }

    /// A nanosecond TIMESTAMP has no `ConvertedType`, only a `LogicalType`
    /// unit; a quoted time compares with it all the same (#679).
    #[cfg(feature = "storage-column-write")]
    #[test]
    fn a_nanosecond_timestamp_compares_with_a_quoted_time() {
        use crate::storage::parquet::writer::{
            ColumnSpec, ColumnType, ColumnValues, ParquetWriter, WriterOptions,
        };
        let schema = vec![ColumnSpec::new("ts", ColumnType::TimestampNanos, false)];
        let mut w = ParquetWriter::new(Vec::new(), schema, WriterOptions::default()).unwrap();
        // 2026-10-22 00:00, 01:00 and 02:00 UTC.
        let hour = 3_600_000_000_000_i64;
        let day = 1_792_627_200_000_000_000_i64;
        w.write_columns(vec![ColumnValues::TimestampNanos(
            (0..3).map(|i| Some(day + i * hour)).collect(),
        )])
        .unwrap();
        let mut engine = BatchEngine::from_bytes("t", w.finish().unwrap().0).unwrap();
        let rows = engine
            .run_query("SELECT count(*) FROM t WHERE ts >= '2026-10-22T01:00:00Z'")
            .unwrap()
            .rows;
        assert_eq!(rows, vec![vec![crate::engine::Cell::Int(2)]]);
    }

    /// End to end over the 3-row-group fixture whose `id`/`amount` are
    /// monotone across row groups -- the layout #464's pruning is for.
    /// Asserts results only: which groups were skipped is thread timing.
    #[test]
    fn top_n_over_a_clustered_parquet_file_returns_the_same_rows_as_a_full_sort() {
        let path = Path::new("tests/fixtures/parquet/production.parquet");
        let mut engine = BatchEngine::open(path).unwrap();
        let mut rows = |sql: &str| -> Vec<String> {
            engine
                .run_query(sql)
                .unwrap()
                .rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect()
        };
        assert_eq!(
            rows("SELECT id FROM production ORDER BY id DESC LIMIT 2"),
            vec!["5000", "4999"]
        );
        assert_eq!(
            rows("SELECT amount FROM production ORDER BY amount LIMIT 2"),
            vec!["2.5", "5.0"]
        );
        // A WHERE clause only removes rows, so the row group with the
        // best bound (ids up to 5000) is loaded and filtered, not trusted.
        assert_eq!(
            rows("SELECT id FROM production WHERE id < 3000 ORDER BY id DESC LIMIT 2"),
            vec!["2999", "2998"]
        );
        assert_eq!(
            rows("SELECT id, amount FROM production ORDER BY amount DESC LIMIT 1"),
            vec!["5000,12500.0"]
        );
    }

    /// #889: the top-N heap reserved `LIMIT + OFFSET + 1` candidates up
    /// front, so `LIMIT i64::MAX` panicked ("capacity overflow") and
    /// `LIMIT 1e12` aborted the process on the allocation. Covers the
    /// parallel (3 row groups) and the single-segment path.
    #[test]
    fn top_n_with_a_huge_limit_or_offset_returns_rows_instead_of_reserving_them() {
        for (path, table, key, total) in [
            (
                "tests/fixtures/parquet/production.parquet",
                "production",
                "id",
                5000,
            ),
            ("tests/fixtures/parquet/events.parquet", "events", "ts", 1),
        ] {
            let mut engine = BatchEngine::open(Path::new(path)).unwrap();
            let mut count = |tail: &str| {
                let sql = format!("SELECT * FROM {table} ORDER BY {key} DESC {tail}");
                engine.run_query(&sql).unwrap().rows.len()
            };
            let all = count("");
            assert!(all >= total);
            assert_eq!(count("LIMIT 9223372036854775807"), all);
            assert_eq!(count("LIMIT 1000000000000"), all);
            assert_eq!(count("LIMIT 1 OFFSET 9223372036854775806"), 0);
            assert_eq!(count("LIMIT 9223372036854775807 OFFSET 1"), all - 1);
        }
    }

    #[test]
    fn prunes_a_row_group_whose_max_statistic_cannot_satisfy_the_filter() {
        let file_bytes = build_file(&[
            (
                &[1.0, 2.0, 3.0],
                Some(build_statistics(
                    &1.0f64.to_le_bytes(),
                    &3.0f64.to_le_bytes(),
                )),
            ),
            (
                &[100.0, 150.0, 200.0],
                Some(build_statistics(
                    &100.0f64.to_le_bytes(),
                    &200.0f64.to_le_bytes(),
                )),
            ),
        ]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let select = where_clause("amount > 50");
        let leaves = amount_leaf();

        let program = Program::new(Vec::new());
        let segments = row_group_segments(
            &file,
            &leaves,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert_eq!(
            segments
                .iter()
                .map(|s| s.row_group_index)
                .collect::<Vec<_>>(),
            vec![1],
            "row group 0 (max 3.0) cannot satisfy `amount > 50` and should be pruned"
        );
    }

    #[test]
    fn missing_statistics_disables_pruning_for_that_row_group() {
        let file_bytes = build_file(&[(&[1.0, 2.0, 3.0], None), (&[100.0, 150.0], None)]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let select = where_clause("amount > 50");
        let leaves = amount_leaf();

        let program = Program::new(Vec::new());
        let segments = row_group_segments(
            &file,
            &leaves,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert_eq!(
            segments
                .iter()
                .map(|s| s.row_group_index)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "no statistics means no pruning, not a wrong skip"
        );
    }

    #[test]
    fn malformed_statistics_bytes_disable_pruning_instead_of_misreading_them() {
        // A DOUBLE column's max_value must be 8 bytes; 3 is truncated/corrupt.
        let bogus_stats = build_statistics(&[0, 0, 0], &[0, 0, 0]);
        let file_bytes = build_file(&[(&[1.0, 2.0, 3.0], Some(bogus_stats))]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let select = where_clause("amount > 50");
        let leaves = amount_leaf();

        let program = Program::new(Vec::new());
        let segments = row_group_segments(
            &file,
            &leaves,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert_eq!(
            segments
                .iter()
                .map(|s| s.row_group_index)
                .collect::<Vec<_>>(),
            vec![0],
            "malformed statistics bytes must fall back to no pruning, never a wrong skip"
        );
    }

    #[test]
    fn no_where_clause_keeps_every_row_group() {
        let file_bytes = build_file(&[(&[1.0], None), (&[2.0], None)]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let leaves = amount_leaf();

        let program = Program::new(Vec::new());
        let segments = row_group_segments(&file, &leaves, &leaves, None, &program, &[]);
        assert_eq!(segments.len(), 2);
    }

    /// End-to-end: pruning a row group must never change the query's
    /// answer, only which segments get read. Row group 0 is provably
    /// prunable for `amount > 50` (max 3.0); the full query must still
    /// return exactly the matching rows from row group 1.
    #[test]
    fn pruned_query_returns_the_same_rows_as_an_unpruned_scan_would() {
        let file_bytes = build_file(&[
            (
                &[1.0, 2.0, 3.0],
                Some(build_statistics(
                    &1.0f64.to_le_bytes(),
                    &3.0f64.to_le_bytes(),
                )),
            ),
            (
                &[100.0, 150.0, 200.0],
                Some(build_statistics(
                    &100.0f64.to_le_bytes(),
                    &200.0f64.to_le_bytes(),
                )),
            ),
        ]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let select =
            planner::expand_star(&where_clause("amount > 50"), &["amount".to_string()]).unwrap();
        let program: Program = planner::compile(&select).unwrap();
        let leaves = amount_leaf();
        let columns = resolve_columns(&leaves, &program.columns_to_load()).unwrap();

        let segments = row_group_segments(
            &file,
            &columns,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert_eq!(
            segments
                .iter()
                .map(|s| s.row_group_index)
                .collect::<Vec<_>>(),
            vec![1]
        );

        let output = engine::run(&segments, &program).unwrap();
        let amounts: Vec<f64> = output
            .into_rows()
            .into_iter()
            .map(|row| row[0].as_f64().expect("amount column"))
            .collect();
        assert_eq!(amounts, vec![100.0, 150.0, 200.0]);
    }

    #[test]
    fn type_name_covers_every_physical_type() {
        assert_eq!(type_name(PhysicalType::Boolean), "BOOLEAN");
        assert_eq!(type_name(PhysicalType::Int32), "INT32");
        assert_eq!(type_name(PhysicalType::Int64), "INT64");
        assert_eq!(type_name(PhysicalType::Int96), "INT96");
        assert_eq!(type_name(PhysicalType::Float), "FLOAT");
        assert_eq!(type_name(PhysicalType::Double), "DOUBLE");
        assert_eq!(type_name(PhysicalType::ByteArray), "BYTE_ARRAY");
        assert_eq!(
            type_name(PhysicalType::FixedLenByteArray),
            "FIXED_LEN_BYTE_ARRAY"
        );
        assert_eq!(type_name(PhysicalType::Unknown(-1)), "");
    }

    #[test]
    fn segment_load_errors_on_an_out_of_range_row_group() {
        let file_bytes = build_file(&[(&[1.0], None)]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let program = Program::new(Vec::new());
        let seg = RowGroupSegment {
            file: &file,
            row_group_index: 99,
            columns: amount_leaf(),
            program: &program,
            order_leaf: None,
            bit_pattern: Vec::new(),
        };
        assert!(seg.load().is_err());
    }

    #[test]
    fn segment_load_errors_on_an_out_of_range_column() {
        let file_bytes = build_file(&[(&[1.0], None)]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let program = Program::new(Vec::new());
        let seg = RowGroupSegment {
            file: &file,
            row_group_index: 0,
            columns: vec![("amount".to_string(), 7, PhysicalType::Double)],
            program: &program,
            order_leaf: None,
            bit_pattern: Vec::new(),
        };
        assert!(seg.load().is_err());
    }

    #[test]
    fn prunes_using_a_float_literal_against_a_double_column() {
        let file_bytes = build_file(&[
            (
                &[1.0, 2.0],
                Some(build_statistics(
                    &1.0f64.to_le_bytes(),
                    &2.0f64.to_le_bytes(),
                )),
            ),
            (
                &[100.0, 150.0],
                Some(build_statistics(
                    &100.0f64.to_le_bytes(),
                    &150.0f64.to_le_bytes(),
                )),
            ),
        ]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let select = where_clause("amount > 50.5");
        let leaves = amount_leaf();

        let program = Program::new(Vec::new());
        let segments = row_group_segments(
            &file,
            &leaves,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert_eq!(
            segments
                .iter()
                .map(|s| s.row_group_index)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn prunes_when_the_literal_is_on_the_left_hand_side_for_every_comparison_operator() {
        let file_bytes = build_file(&[
            (
                &[1.0, 2.0],
                Some(build_statistics(
                    &1.0f64.to_le_bytes(),
                    &2.0f64.to_le_bytes(),
                )),
            ),
            (
                &[100.0, 150.0],
                Some(build_statistics(
                    &100.0f64.to_le_bytes(),
                    &150.0f64.to_le_bytes(),
                )),
            ),
        ]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let leaves = amount_leaf();

        // Flipping which side the literal is on also flips the operator's
        // meaning: "50 < amount" is `amount > 50` (rules out row group 0,
        // whose max is 2), while "50 > amount" is `amount < 50` (rules out
        // row group 1, whose min is 100). "50 = amount" matches neither
        // group's range, so both are pruned.
        for (sql, expected) in [
            ("50 < amount", vec![1]),
            ("50 <= amount", vec![1]),
            ("50 > amount", vec![0]),
            ("50 >= amount", vec![0]),
            ("50 = amount", vec![]),
        ] {
            let select = where_clause(sql);
            let program = Program::new(Vec::new());
            let segments = row_group_segments(
                &file,
                &leaves,
                &leaves,
                select.where_clause.as_ref(),
                &program,
                &[],
            );
            assert_eq!(
                segments
                    .iter()
                    .map(|s| s.row_group_index)
                    .collect::<Vec<_>>(),
                expected,
                "{sql}"
            );
        }
    }

    #[test]
    fn parenthesized_and_or_predicates_still_prune() {
        let file_bytes = build_file(&[
            (
                &[1.0, 2.0],
                Some(build_statistics(
                    &1.0f64.to_le_bytes(),
                    &2.0f64.to_le_bytes(),
                )),
            ),
            (
                &[100.0, 150.0],
                Some(build_statistics(
                    &100.0f64.to_le_bytes(),
                    &150.0f64.to_le_bytes(),
                )),
            ),
        ]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let leaves = amount_leaf();

        // AND: either side proving emptiness is enough.
        let select = where_clause("(amount > 500) AND (amount > 0)");
        let program = Program::new(Vec::new());
        let segments = row_group_segments(
            &file,
            &leaves,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert!(segments.is_empty(), "no row group can satisfy amount > 500");

        // OR: both sides must prove emptiness.
        let select = where_clause("(amount > 50) OR (amount < 0)");
        let program = Program::new(Vec::new());
        let segments = row_group_segments(
            &file,
            &leaves,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert_eq!(
            segments
                .iter()
                .map(|s| s.row_group_index)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn a_string_literal_prunes_a_byte_array_column() {
        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");

        let build_region_row_group = |values: &[&str], base_offset: i64, stats: Option<Vec<u8>>| {
            let mut body = Vec::new();
            for v in values {
                body.extend_from_slice(&(v.len() as u32).to_le_bytes());
                body.extend_from_slice(v.as_bytes());
            }
            let header = build_page_header(values.len() as i32, body.len() as i32);
            let mut page_bytes = header;
            page_bytes.extend_from_slice(&body);

            let mut meta = StructWriter::new();
            meta.i32_field(1, 6); // BYTE_ARRAY
            meta.field_header(3, 0x09);
            meta.buf.push((1u8 << 4) | 0x08);
            meta.write_varint(6);
            meta.buf.extend_from_slice(b"region");
            meta.i64_field(5, values.len() as i64);
            meta.i64_field(6, page_bytes.len() as i64);
            meta.i64_field(7, page_bytes.len() as i64);
            meta.i64_field(9, base_offset);
            if let Some(s) = stats {
                meta.struct_field(12, s);
            }
            (page_bytes, meta.finish())
        };

        let mut rg_thrift = Vec::new();
        let mut total_rows = 0i64;
        for (values, stats) in [
            (
                &["east", "east"][..],
                Some(build_statistics(b"east", b"east")),
            ),
            (
                &["west", "west"][..],
                Some(build_statistics(b"west", b"west")),
            ),
        ] {
            let base_offset = file.len() as i64;
            let (page_bytes, meta_bytes) = build_region_row_group(values, base_offset, stats);
            file.extend_from_slice(&page_bytes);

            let mut chunk = StructWriter::new();
            chunk.i64_field(2, base_offset);
            chunk.struct_field(3, meta_bytes);

            let mut rg = StructWriter::new();
            rg.list_of_structs_field(1, vec![chunk.finish()]);
            rg.i64_field(2, page_bytes.len() as i64);
            rg.i64_field(3, values.len() as i64);
            rg_thrift.push(rg.finish());
            total_rows += values.len() as i64;
        }

        let mut root = StructWriter::new();
        root.string_field(4, "schema");
        root.i32_field(5, 1);

        let mut col = StructWriter::new();
        col.i32_field(1, 6); // BYTE_ARRAY
        col.i32_field(3, 0);
        col.string_field(4, "region");
        col.i32_field(6, 0); // UTF8: an unannotated BYTE_ARRAY is a BLOB (#595)

        let mut fmd = StructWriter::new();
        fmd.i32_field(1, 1);
        fmd.list_of_structs_field(2, vec![root.finish(), col.finish()]);
        fmd.i64_field(3, total_rows);
        fmd.list_of_structs_field(4, rg_thrift);
        fmd.string_field(6, "test");
        let metadata = fmd.finish();

        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");

        let file = ParquetFile::open(&file).unwrap();
        let leaves = vec![("region".to_string(), 0, PhysicalType::ByteArray)];
        let stmt = "SELECT region FROM t WHERE region = 'west'";
        let select = crate::parser::parse(stmt).expect("valid SQL");

        let program = Program::new(Vec::new());
        let segments = row_group_segments(
            &file,
            &leaves,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert_eq!(
            segments
                .iter()
                .map(|s| s.row_group_index)
                .collect::<Vec<_>>(),
            vec![1],
            "row group 0 (\"east\") cannot match region = 'west'"
        );
    }

    /// Exercises the test-only Thrift writer's varint-continuation and
    /// 15-or-more-items list encodings (real writers hit both routinely;
    /// the smaller fixtures elsewhere in this module don't).
    #[test]
    fn many_row_groups_with_large_pages_are_all_considered_for_pruning() {
        let big_values: Vec<f64> = (0..40).map(f64::from).collect();
        let big_stats = build_statistics(&0.0f64.to_le_bytes(), &39.0f64.to_le_bytes());
        let mut row_groups: Vec<(&[f64], Option<Vec<u8>>)> = vec![(&big_values, Some(big_stats))];
        let small = [1000.0];
        for _ in 0..15 {
            row_groups.push((
                &small,
                Some(build_statistics(
                    &1000.0f64.to_le_bytes(),
                    &1000.0f64.to_le_bytes(),
                )),
            ));
        }

        let file_bytes = build_file(&row_groups);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let select = where_clause("amount > 500");
        let leaves = amount_leaf();

        let program = Program::new(Vec::new());
        let segments = row_group_segments(
            &file,
            &leaves,
            &leaves,
            select.where_clause.as_ref(),
            &program,
            &[],
        );
        assert_eq!(
            segments.len(),
            15,
            "only the big-values row group should be pruned"
        );
        assert!(!segments.iter().any(|s| s.row_group_index == 0));
    }

    /// A file with a `UINT_64` column `sid` (5, `u64::MAX`) and an `INT64`
    /// column `n`, for the #600 bit-pattern reads below.
    fn u64_file() -> Vec<u8> {
        use crate::storage::parquet::writer::{
            ColumnSpec, ColumnType, ColumnValues, ParquetWriter, WriterOptions,
        };
        let schema = vec![
            ColumnSpec::new("sid", ColumnType::UInt64, false),
            ColumnSpec::new("n", ColumnType::Int64, false),
        ];
        let mut w = ParquetWriter::new(Vec::new(), schema, WriterOptions::default()).unwrap();
        w.write_columns(vec![
            ColumnValues::UInt64(vec![Some(5), Some(u64::MAX)]),
            ColumnValues::Int64(vec![Some(1), Some(2)]),
        ])
        .unwrap();
        w.finish().unwrap().0
    }

    /// `load_leaf`/`load_leaf_at` at leaf `index`, with `bit_pattern`: the
    /// values when they come back as [`Value`]s, `None` when typed.
    fn load_both(
        index: usize,
        pt: PhysicalType,
        bit_pattern: bool,
    ) -> (Option<Vec<Value>>, Option<Vec<Value>>) {
        use super::{load_leaf, load_leaf_at, Loaded};
        let bytes = u64_file();
        let file = ParquetFile::open(&bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let values = |l: Result<Loaded, _>| match l
            .map_err(|e: crate::storage::parquet::FileError| e.to_string())
        {
            Ok(Loaded::Values(v)) => Some(v),
            Ok(Loaded::Typed(_)) => None,
            Err(e) => {
                assert!(e.contains("18446744073709551615"), "{e}");
                Some(Vec::new())
            }
        };
        (
            values(load_leaf(
                &rg,
                index,
                pt,
                bit_pattern,
                super::ListRead::Full,
            )),
            values(load_leaf_at(
                &rg,
                index,
                pt,
                &[0, 1],
                bit_pattern,
                super::ListRead::Full,
            )),
        )
    }

    /// Field `name`'s entry in `leaf_columns` of DuckDB's
    /// `nested_shapes.parquet` (tools/gen_parquet_fixtures.sh), if any.
    fn nested_shapes_leaf(name: &str) -> Option<Leaf> {
        let bytes = std::fs::read("tests/fixtures/parquet/nested_shapes.parquet").unwrap();
        let file = ParquetFile::open(&bytes).unwrap();
        super::leaf_columns(&file)
            .into_iter()
            .find(|(n, _, _)| n == name)
    }

    /// `nested_values` of `nested_shapes.parquet`'s field `name`.
    fn nested_shapes_values(name: &str, levels_only: bool) -> Vec<Value> {
        let bytes = std::fs::read("tests/fixtures/parquet/nested_shapes.parquet").unwrap();
        let file = ParquetFile::open(&bytes).unwrap();
        let field = file.fields().into_iter().find(|f| f.name == name).unwrap();
        super::nested_values(&file.row_group(0).unwrap(), &field, levels_only).unwrap()
    }

    /// #846: the presence column a list read for NULL-ness loads as holds,
    /// row for row, what `nested_values` gives it (a NULL list, or a list
    /// with no elements shown).
    #[test]
    fn list_presence_column_agrees_with_nested_values() {
        let bytes = std::fs::read("tests/fixtures/parquet/list_dictionary.parquet").unwrap();
        let file = ParquetFile::open(&bytes).unwrap();
        let field = file.fields().into_iter().next().unwrap();
        let rg = file.row_group(0).unwrap();
        let there = rg.read_list_utf8_presence(&field).unwrap();
        let column = super::presence_column(&there);
        let expected = super::nested_values(&rg, &field, true).unwrap();
        assert_eq!(column.len(), expected.len());
        assert!(there.contains(&true) && there.contains(&false));
        for (i, value) in expected.iter().enumerate() {
            assert_eq!(&column.get(i), value, "row {i}");
        }
    }

    // engine_column_nested_values_1a2c930a: `is_list(field) && levels_only`
    // (#810). `lst` is one row holding `["x"]`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_nested_values_1a2c930a__v1_a_list_read_for_null_ness_has_no_elements() {
        assert_eq!(
            nested_shapes_values("lst", true),
            vec![Value::List(std::sync::Arc::new(Vec::new()))]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_nested_values_1a2c930a__v2_a_list_read_in_full_has_its_elements() {
        assert_eq!(
            nested_shapes_values("lst", false),
            vec![Value::List(std::sync::Arc::new(vec![Some("x".into())]))]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_nested_values_1a2c930a__v3_a_map_is_read_in_full_whatever_the_flag() {
        let map = nested_shapes_values("m", true);
        assert_eq!(map, nested_shapes_values("m", false));
        assert!(matches!(map.as_slice(), [Value::Map(_)]), "{map:?}");
    }

    /// `load_leaf` of `nested_shapes.parquet`'s field `name`, read as `list`.
    fn nested_shapes_load(name: &str, list: super::ListRead) -> super::Loaded {
        let bytes = std::fs::read("tests/fixtures/parquet/nested_shapes.parquet").unwrap();
        let file = ParquetFile::open(&bytes).unwrap();
        let (_, index, pt) = nested_shapes_leaf(name).unwrap();
        super::load_leaf(&file.row_group(0).unwrap(), index, pt, false, list).unwrap()
    }

    // engine_column_load_leaf_1a209f83: `is_list(&field) && list ==
    // ListRead::Flat` (#809). `lst` is one row holding `["x"]`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_1a209f83__v1_a_flat_list_read_is_a_typed_list_column() {
        let super::Loaded::Typed(super::Decoded::Column(column)) =
            nested_shapes_load("lst", super::ListRead::Flat)
        else {
            panic!("a flat list read is a typed column");
        };
        assert!(matches!(column, crate::vm::column::Column::List { .. }));
        assert_eq!(
            column.get(0),
            Value::List(std::sync::Arc::new(vec![Some("x".into())]))
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_1a209f83__v2_a_list_read_in_full_is_values() {
        let super::Loaded::Values(values) = nested_shapes_load("lst", super::ListRead::Full) else {
            panic!("a full list read is values");
        };
        assert_eq!(
            values,
            vec![Value::List(std::sync::Arc::new(vec![Some("x".into())]))]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_1a209f83__v3_a_map_is_values_whatever_the_flag() {
        let super::Loaded::Values(values) = nested_shapes_load("m", super::ListRead::Flat) else {
            panic!("a map is read as values");
        };
        assert!(matches!(values.as_slice(), [Value::Map(_)]), "{values:?}");
    }

    // engine_column_load_leaf_601ae351: `is_list(&field) && list ==
    // ListRead::LevelsOnly` (#846). `lst` is one row holding `["x"]`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_601ae351__v1_a_levels_only_list_read_is_a_typed_presence_column(
    ) {
        let super::Loaded::Typed(super::Decoded::Column(column)) =
            nested_shapes_load("lst", super::ListRead::LevelsOnly)
        else {
            panic!("a levels-only list read is a typed column");
        };
        assert!(matches!(column, crate::vm::column::Column::List { .. }));
        assert_eq!(column.get(0), Value::List(std::sync::Arc::new(Vec::new())));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_601ae351__v2_a_list_read_in_full_is_not_a_presence_column() {
        let super::Loaded::Values(values) = nested_shapes_load("lst", super::ListRead::Full) else {
            panic!("a full list read is values");
        };
        assert_eq!(values.len(), 1);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_601ae351__v3_a_map_is_values_under_levels_only() {
        let super::Loaded::Values(values) = nested_shapes_load("m", super::ListRead::LevelsOnly)
        else {
            panic!("a map is read as values");
        };
        assert!(matches!(values.as_slice(), [Value::Map(_)]), "{values:?}");
    }

    /// #801: `is_map(&f) || is_list(&f)`, a map (leaf A true).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_leaf_columns_2a9a10da__v1_a_map_is_a_column() {
        assert_eq!(
            nested_shapes_leaf("m").map(|(_, _, pt)| pt),
            Some(PhysicalType::ByteArray)
        );
    }

    /// A list (leaf B true).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_leaf_columns_2a9a10da__v2_a_list_is_a_column() {
        assert_eq!(
            nested_shapes_leaf("lst").map(|(_, _, pt)| pt),
            Some(PhysicalType::ByteArray)
        );
    }

    /// A struct (both false) is not a column.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_leaf_columns_2a9a10da__v3_a_struct_is_not_a_column() {
        assert_eq!(nested_shapes_leaf("st"), None);
    }

    /// #600: `bit_pattern && annotation == UInt64`, leaf A false -- without
    /// the flag a `UINT_64` above `i64::MAX` is still an error (#595).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_57155792__v1_flag_off_keeps_the_error() {
        // An error shows as the empty stand-in `values` returns.
        assert_eq!(
            load_both(0, PhysicalType::Int64, false),
            (Some(Vec::new()), Some(Vec::new()))
        );
    }

    /// Leaf A true, leaf B false: the flag on a column that is not `UINT_64`
    /// reads it as usual (typed).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_57155792__v2_flag_on_plain_column_reads_typed() {
        assert_eq!(load_both(1, PhysicalType::Int64, true), (None, None));
    }

    /// Both true: the `UINT_64` reads as its signed bit pattern.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_57155792__v3_flag_on_uint64_reads_the_bit_pattern() {
        let want = Some(vec![Value::Int(5), Value::Int(-1)]);
        assert_eq!(
            load_both(0, PhysicalType::Int64, true),
            (want.clone(), want)
        );
    }

    /// The same three vectors for `load_leaf_at`'s own decision (#600).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_at_57155792__v1_flag_off_keeps_the_error() {
        assert_eq!(
            load_both(0, PhysicalType::Int64, false),
            (Some(Vec::new()), Some(Vec::new()))
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_at_57155792__v2_flag_on_plain_column_reads_typed() {
        assert_eq!(load_both(1, PhysicalType::Int64, true), (None, None));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_column_load_leaf_at_57155792__v3_flag_on_uint64_reads_the_bit_pattern() {
        let want = Some(vec![Value::Int(5), Value::Int(-1)]);
        assert_eq!(
            load_both(0, PhysicalType::Int64, true),
            (want.clone(), want)
        );
    }
}
