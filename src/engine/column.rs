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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::codegen::batch::{self as planner, PlanError, TableStats};
use crate::parser::ast::{Expr, ExprKind, ResultColumn, Select};
use crate::parser::ParseError;
use crate::storage::column::parquet::footer::PhysicalType;
use crate::storage::{MmapRegion, ParquetFile, PosixVfs, Vfs, VfsFile};
use crate::vm::batch::{Batch, Program, Segment, Value, VmError};
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
}

impl Segment for RowGroupSegment<'_, '_> {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        let rg = self
            .file
            .row_group(self.row_group_index)
            .ok_or_else(|| VmError::SegmentLoad {
                reason: format!(
                    "row group {} does not exist (file has {})",
                    self.row_group_index,
                    self.file.num_row_groups()
                ),
            })?;
        let num_rows = usize::try_from(rg.num_rows()).map_err(|_| VmError::SegmentLoad {
            reason: format!(
                "row group {}: negative row count {}",
                self.row_group_index,
                rg.num_rows()
            ),
        })?;
        let mut batch = Batch::new(num_rows);
        for (name, index, physical_type) in &self.columns {
            let values = match physical_type {
                PhysicalType::Int64 => rg.read_int64_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, Value::Int))
                        .collect()
                }),
                PhysicalType::Int32 => rg.read_int32_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, |i| Value::Int(i64::from(i))))
                        .collect()
                }),
                PhysicalType::Double => rg.read_double_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, Value::Float))
                        .collect()
                }),
                PhysicalType::Float => rg.read_float_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, |f| Value::Float(f64::from(f))))
                        .collect()
                }),
                PhysicalType::Boolean => rg.read_boolean_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, Value::Bool))
                        .collect()
                }),
                _ => rg.read_string_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, |s| Value::Str(s.into())))
                        .collect()
                }),
            }
            .map_err(|e| VmError::SegmentLoad {
                reason: format!("row group {}: column `{name}`: {e}", self.row_group_index),
            })?;
            batch = batch.with_column(name.clone(), values);
        }
        Ok(Arc::new(batch))
    }
}

fn row_group_segments<'f>(
    file: &'f ParquetFile<'f>,
    columns: &[Leaf],
) -> Vec<RowGroupSegment<'f, 'f>> {
    (0..file.num_row_groups())
        .map(|i| RowGroupSegment {
            file,
            row_group_index: i,
            columns: columns.to_vec(),
        })
        .collect()
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
        let segments = row_group_segments(&file, &columns);
        let rows = engine::run(&segments, &program)
            .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
        Ok(QueryResult {
            columns: planner::output_column_names(&select),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(Cell::from).collect())
                .collect(),
        })
    }

    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        let stats = TableStats {
            row_groups: self.num_row_groups,
            rows: self.num_rows,
        };
        let nodes = planner::explain(&select, |_| stats).map_err(plan_err)?;
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
                rows: s
                    .rows
                    .into_iter()
                    .map(|r| OpcodeRow {
                        addr: r.addr,
                        opcode: r.opcode.to_string(),
                        operands: if r.comment.is_empty() {
                            r.operands
                        } else {
                            format!("{}  ; {}", r.operands, r.comment)
                        },
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
