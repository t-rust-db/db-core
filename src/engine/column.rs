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
use crate::parser::ast::{BinaryOp, Expr, ExprKind, Literal, ResultColumn, Select};
use crate::parser::ParseError;
use crate::storage::column::parquet::footer::PhysicalType;
use crate::storage::column::{decode_column_at, decode_column_full, Decoded};
use crate::storage::{MmapRegion, ParquetFile, PosixVfs, RowGroupReader, Vfs, VfsFile};
use crate::vm::batch::{Batch, Program, Segment, Value, Vm, VmError};
use crate::vm::engine;

use super::{
    single_statement, Cell, ColumnInfo, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, TableInfo,
};

/// A leaf column of the file: display name, index in the schema, physical type.
type Leaf = (String, usize, PhysicalType);

/// One open Parquet file, presented as a single table.
pub struct BatchEngine {
    path: PathBuf,
    /// The whole file, memory-mapped; a fresh [`ParquetFile`] view is
    /// opened over it per query (footer parsing is cheap; the data pages
    /// are only touched by the columns a program loads).
    data: MmapRegion,
    table: String,
    leaves: Vec<Leaf>,
    num_rows: i64,
    num_row_groups: usize,
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

    fn file(&self) -> Result<ParquetFile<'_>, EngineError> {
        ParquetFile::open(&self.data).map_err(|e| EngineError::new(ErrorKind::Open, e))
    }

    fn column_names(&self) -> Vec<String> {
        self.leaves.iter().map(|(n, _, _)| n.clone()).collect()
    }

    /// Parses one `SELECT` for this table: the batch grammar (analytics
    /// subset, aliases resolved), `*` expanded against the file's columns,
    /// and the `FROM` table checked against the one table we have.
    fn parse_for_table(&self, sql: &str) -> Result<Select, EngineError> {
        let select = planner_parse(sql)?;
        let from = from_table(&select)?;
        if !from.eq_ignore_ascii_case(&self.table) {
            return Err(EngineError::new(
                ErrorKind::Compile,
                format!(
                    "unknown table: {from} (this file is table `{}`)",
                    self.table
                ),
            ));
        }
        if let Some(join) = select.from.as_ref().and_then(|f| f.joins.first()) {
            let right = join.table.name().unwrap_or("<subquery>");
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                format!("JOIN with `{right}`: a single-file engine has one table; use column-rs's multi-table session"),
            ));
        }
        if in_subquery(&select).is_some() {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                "IN (SELECT ...) needs a second table; a single-file engine has one",
            ));
        }
        if select.columns.iter().any(is_window_column) {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                "window functions are not available through the single-file engine yet",
            ));
        }
        planner::expand_star(&select, &self.column_names()).map_err(plan_err)
    }
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

/// Leaf schema columns as `(name, column_index, physical_type)`, in file
/// order. Element 0 of the footer schema is the root group, skipped.
fn leaf_columns(file: &ParquetFile<'_>) -> Vec<Leaf> {
    file.metadata()
        .schema
        .iter()
        .skip(1)
        .enumerate()
        .filter_map(|(i, s)| s.physical_type.map(|pt| (s.name.clone(), i, pt)))
        .collect()
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
}

impl RowGroupSegment<'_, '_> {
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
        let mut batch = Batch::new(num_rows);
        for (name, index, physical_type) in &self.columns {
            let decoded = decode_column_full(rg, *index, *physical_type)
                .map_err(|e| segment_error(self.row_group_index, name, &e))?;
            batch = apply_decoded(batch, name, decoded);
        }
        Ok(batch)
    }
}

impl Segment for RowGroupSegment<'_, '_> {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        let rg = self.row_group()?;
        let num_rows = usize::try_from(rg.num_rows()).map_err(|_| VmError::SegmentLoad {
            reason: format!(
                "row group {}: negative row count {}",
                self.row_group_index,
                rg.num_rows()
            ),
        })?;

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
        let mut predicate_batch = Batch::new(num_rows);
        for (name, index, physical_type) in &predicate_leaves {
            let decoded = decode_column_full(&rg, *index, *physical_type)
                .map_err(|e| segment_error(self.row_group_index, name, &e))?;
            predicate_batch = apply_decoded(predicate_batch, name, decoded);
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
            let decoded = decode_column_at(&rg, *index, *physical_type, &indices)
                .map_err(|e| segment_error(self.row_group_index, name, &e))?;
            batch = apply_decoded(batch, name, decoded);
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

/// Applies one column's decoded shape onto `batch` under `name` (see
/// [`Decoded`]).
fn apply_decoded(batch: Batch, name: &str, decoded: Decoded) -> Batch {
    let Decoded::Column(column) = decoded;
    batch.with_typed_column(name.to_string(), column)
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
        })
        .collect()
}

/// Row-group pruning from footer [`Statistics`](footer::Statistics):
/// deciding whether a `WHERE` clause can be proven to match no row in a
/// row group without reading its data pages.
mod prune {
    use super::{
        BinaryOp, Expr, ExprKind, HashMap, Literal, Ordering, PhysicalType, RowGroupReader,
    };

    /// A value decoded from footer statistics or a `WHERE`-clause literal,
    /// typed so cross-type comparisons (e.g. an integer literal against a
    /// DOUBLE column) still work.
    #[derive(Debug, Clone, PartialEq)]
    enum StatValue {
        Int(i64),
        Float(f64),
        Str(String),
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
    /// the column's physical type. `None` for a physical type this pruner
    /// doesn't interpret (e.g. INT96, FIXED_LEN_BYTE_ARRAY) or malformed
    /// bytes -- always falls back to "cannot prune", never misreads them.
    fn decode_stat_bytes(bytes: &[u8], physical_type: PhysicalType) -> Option<StatValue> {
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

    fn literal_stat_value(lit: &Literal, physical_type: PhysicalType) -> Option<StatValue> {
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

    fn comparison_definitely_empty(
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        rg: &RowGroupReader<'_, '_>,
        leaves_by_name: &HashMap<&str, (usize, PhysicalType)>,
    ) -> Option<bool> {
        let (col_name, lit, flipped) = as_column_and_literal(lhs, rhs)?;
        let op = if flipped { flip(op) } else { op };
        let (index, physical_type) = *leaves_by_name.get(col_name)?;
        let statistics = rg.column_statistics(index).ok()??;
        let literal = literal_stat_value(lit, physical_type)?;
        let min = statistics
            .min
            .as_deref()
            .and_then(|b| decode_stat_bytes(b, physical_type));
        let max = statistics
            .max
            .as_deref()
            .and_then(|b| decode_stat_bytes(b, physical_type));

        let empty = match op {
            BinaryOp::Eq => {
                let below_min = min
                    .as_ref()
                    .and_then(|m| compare(&literal, m))
                    .is_some_and(|o| o == Ordering::Less);
                let above_max = max
                    .as_ref()
                    .and_then(|m| compare(&literal, m))
                    .is_some_and(|o| o == Ordering::Greater);
                below_min || above_max
            }
            BinaryOp::Lt => min
                .as_ref()
                .and_then(|m| compare(m, &literal))
                .is_some_and(|o| o != Ordering::Less),
            BinaryOp::Le => min
                .as_ref()
                .and_then(|m| compare(m, &literal))
                .is_some_and(|o| o == Ordering::Greater),
            BinaryOp::Gt => max
                .as_ref()
                .and_then(|m| compare(m, &literal))
                .is_some_and(|o| o != Ordering::Greater),
            BinaryOp::Ge => max
                .as_ref()
                .and_then(|m| compare(m, &literal))
                .is_some_and(|o| o == Ordering::Less),
            _ => false,
        };
        Some(empty)
    }

    /// Whether `expr` can be proven to match no row in `rg`, using only its
    /// footer statistics. Conservative by construction: any expression
    /// shape or missing statistic this function doesn't specifically
    /// recognize falls through to `false` ("may match"), never `true`.
    pub(super) fn definitely_empty(
        expr: &Expr,
        rg: &RowGroupReader<'_, '_>,
        leaves_by_name: &HashMap<&str, (usize, PhysicalType)>,
    ) -> bool {
        match &expr.kind {
            ExprKind::Paren(inner) => definitely_empty(inner, rg, leaves_by_name),
            ExprKind::Binary {
                op: BinaryOp::And,
                lhs,
                rhs,
            } => {
                definitely_empty(lhs, rg, leaves_by_name)
                    || definitely_empty(rhs, rg, leaves_by_name)
            }
            ExprKind::Binary {
                op: BinaryOp::Or,
                lhs,
                rhs,
            } => {
                definitely_empty(lhs, rg, leaves_by_name)
                    && definitely_empty(rhs, rg, leaves_by_name)
            }
            ExprKind::Binary { op, lhs, rhs } => {
                comparison_definitely_empty(*op, lhs, rhs, rg, leaves_by_name).unwrap_or(false)
            }
            _ => false,
        }
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

fn in_subquery(select: &Select) -> Option<&Select> {
    match &select.where_clause {
        Some(Expr {
            kind: ExprKind::InSubquery { subquery, .. },
            ..
        }) => Some(subquery),
        _ => None,
    }
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

impl Engine for BatchEngine {
    fn open(path: &Path) -> Result<Self, EngineError> {
        let data = PosixVfs
            .open(path)
            .and_then(|f| f.mmap())
            .map_err(|e| EngineError::new(ErrorKind::Open, format!("{}: {e}", path.display())))?;
        let (leaves, num_rows, num_row_groups) = {
            let file =
                ParquetFile::open(&data).map_err(|e| EngineError::new(ErrorKind::Open, e))?;
            (leaf_columns(&file), file.num_rows(), file.num_row_groups())
        };
        let table = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("data")
            .to_string();
        Ok(BatchEngine {
            path: path.to_path_buf(),
            data,
            table,
            leaves,
            num_rows,
            num_row_groups,
        })
    }

    fn mode(&self) -> Mode {
        Mode::Batch
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        let program: Program = planner::compile(&select).map_err(plan_err)?;
        let file = self.file()?;
        let columns = resolve_columns(&self.leaves, &program.columns_to_load())?;
        let segments = row_group_segments(
            &file,
            &columns,
            &self.leaves,
            select.where_clause.as_ref(),
            &program,
        );
        let output = engine::run(&segments, &program)
            .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
        Ok(QueryResult {
            columns: planner::output_column_names(&select),
            rows: output
                .into_rows()
                .into_iter()
                .map(|r| r.into_iter().map(Cell::from).collect())
                .collect(),
            ..Default::default()
        })
    }

    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        let stats = TableStats {
            row_groups: self.num_row_groups,
            rows: self.num_rows,
            source: None,
        };
        let nodes = planner::explain(&select, |_| stats.clone()).map_err(plan_err)?;
        Ok(nodes
            .into_iter()
            .map(|n| PlanRow {
                id: i64::from(n.id),
                parent: i64::from(n.parent),
                detail: n.detail,
            })
            .collect())
    }

    fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        let sections = planner::explain_opcodes(&select).map_err(plan_err)?;
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

#[cfg(test)]
mod tests {
    use super::{
        engine, plan_err, planner, resolve_columns, row_group_segments, type_name, ErrorKind, Leaf,
        ParquetFile, PhysicalType, PlanError, Program, RowGroupSegment,
    };
    use crate::parser::ast::Select;
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
    /// `storage::column::parquet::footer`'s and `parquet_file`'s own
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
        let segments = row_group_segments(&file, &leaves, &leaves, None, &program);
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
        );
        assert_eq!(
            segments.len(),
            15,
            "only the big-values row group should be pruned"
        );
        assert!(!segments.iter().any(|s| s.row_group_index == 0));
    }
}
