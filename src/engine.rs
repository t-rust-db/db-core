// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The client-facing seam over db-core's execution modes (#295, ADR 0017).
//!
//! A client such as db-studio opens a file, decides which mode it belongs
//! to (row for `.sqlite`, batch for `.parquet`, stream for `.log`), and
//! then drives one [`Engine`] without branching on the mode again: run a
//! query, get a [`QueryResult`]; ask for a plan, get [`PlanRow`]s; ask for
//! opcodes, get [`OpcodeSection`]s; ask what the file is, get
//! [`FileStats`].
//!
//! The types here are *client* types: owned, display-friendly, and
//! convertible from every mode's own value/plan/opcode types. They are not
//! a third engine value model -- `value::Value` (row) and
//! `vm::batch::Value` (batch) stay distinct (ADR 0010, ADR 0014); a
//! [`Cell`] is what a grid renders after execution is over.
//!
//! Object-safety is deliberate: db-studio switches which engine is live
//! per open file, so it needs `Box<dyn Engine>`. That is why errors are one
//! concrete [`EngineError`] rather than an associated type, and why
//! [`Engine::open`] carries `where Self: Sized`. The `dyn` lives in the
//! client; db-core itself never names `dyn Engine`.
//!
//! Implementations: [`row::RowEngine`] (`engine-row`). Batch and stream
//! are follow-ups; their shapes are already accommodated -- `PlanRow`
//! matches `codegen::batch::PlanNode` field-for-field, `OpcodeSection`
//! matches `codegen::batch::OpcodeSection`, and `FileStats::Stream` is
//! "how much of the file has been parsed", not a page count.

use std::fmt;
use std::path::Path;

#[cfg(feature = "engine-row")]
pub mod row;

#[cfg(feature = "engine-column")]
pub mod column;

#[cfg(feature = "engine-stream")]
pub mod stream;

/// Cross-mode joins (#312 ADR-0019, #314): a SQLite table as a
/// `vm::batch::Batch`/`Source`, the lookup side of a batch/stream-driving
/// join. Deliberately not part of `row`'s module tree (ADR 0000 §(c): the
/// SQLite side never names `vm::batch`) -- like this file itself, it is a
/// seam allowed to know both `row::RowEngine` and `vm::batch`.
#[cfg(all(feature = "engine-row", feature = "vm-batch"))]
pub mod cross_mode;

/// Cross-mode query resolver (#342, epic #317): routes a `SELECT` whose
/// `FROM`/`JOIN` spans the stream table `log` and a SQLite lookup table to
/// [`cross_mode`], `codegen::batch::compile_join` and
/// `vm::engine::run_join_segments`. A sibling seam like `cross_mode`, for
/// the same layer-isolation reason (ADR 0000 §(c)).
#[cfg(all(
    feature = "engine-row",
    feature = "engine-stream",
    feature = "vm-batch"
))]
pub mod resolve;

/// Which execution mode an engine drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `storage::row` + `vm::row`: SQLite-format files.
    Row,
    /// `storage::column` + `vm::batch`: Parquet files.
    Batch,
    /// `storage::stream` + a streaming VM: log files.
    Stream,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Row => "row",
            Mode::Batch => "batch",
            Mode::Stream => "stream",
        })
    }
}

/// One rendered value. Owned and mode-independent: lossless from
/// `value::Value` (row) and from `vm::batch::Value` (batch -- which is where
/// [`Cell::Bool`] comes from; row has no boolean type).
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// SQL `NULL`.
    Null,
    /// A signed 64-bit integer.
    Int(i64),
    /// An IEEE 754 double.
    Real(f64),
    /// A boolean (batch predicates and comparisons).
    Bool(bool),
    /// Text.
    Text(String),
    /// Uninterpreted bytes.
    Blob(Vec<u8>),
}

impl fmt::Display for Cell {
    /// Shell-style rendering: `NULL` is empty, reals use `value::format_real`
    /// (SQLite's `%!.15g`), blobs render as `X'..'` hex.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render Cell types in shell-style format (display, not debug).
        match self {
            Cell::Null => Ok(()),
            Cell::Int(n) => write!(f, "{n}"),
            Cell::Real(x) => f.write_str(&crate::value::format_real(*x)),
            Cell::Bool(b) => f.write_str(if *b { "1" } else { "0" }),
            Cell::Text(s) => f.write_str(s),
            Cell::Blob(bytes) => {
                f.write_str("X'")?;
                for b in bytes {
                    write!(f, "{b:02X}")?;
                }
                f.write_str("'")
            }
        }
    }
}

impl From<crate::value::Value> for Cell {
    fn from(v: crate::value::Value) -> Self {
        match v {
            crate::value::Value::Null => Cell::Null,
            crate::value::Value::Integer(n) => Cell::Int(n),
            crate::value::Value::Real(x) => Cell::Real(x),
            crate::value::Value::Text(s) => Cell::Text(s.to_string()),
            crate::value::Value::Blob(b) => Cell::Blob(b.to_vec()),
        }
    }
}

impl From<&crate::value::Value> for Cell {
    fn from(v: &crate::value::Value) -> Self {
        match v {
            crate::value::Value::Null => Cell::Null,
            crate::value::Value::Integer(n) => Cell::Int(*n),
            crate::value::Value::Real(x) => Cell::Real(*x),
            crate::value::Value::Text(s) => Cell::Text(s.to_string()),
            crate::value::Value::Blob(b) => Cell::Blob(b.to_vec()),
        }
    }
}

#[cfg(feature = "vm-batch")]
impl From<crate::vm::batch::Value> for Cell {
    fn from(v: crate::vm::batch::Value) -> Self {
        match v {
            crate::vm::batch::Value::Null => Cell::Null,
            crate::vm::batch::Value::Int(n) => Cell::Int(n),
            crate::vm::batch::Value::Float(x) => Cell::Real(x),
            crate::vm::batch::Value::Bool(b) => Cell::Bool(b),
            crate::vm::batch::Value::Str(s) => Cell::Text(s.into_owned()),
        }
    }
}

/// A result set: column labels plus rows of [`Cell`]s. A statement that
/// produces no rows (DDL, DML, `BEGIN`) yields an empty `rows` and empty
/// `columns`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryResult {
    /// Column labels, one per cell of every row. May be empty when the
    /// engine has no names to derive (e.g. a multi-statement script whose
    /// last statement returned rows without a `SELECT` AST).
    pub columns: Vec<String>,
    /// The rows, in result order.
    pub rows: Vec<Vec<Cell>>,
    /// The stream engine's effective range for this query (ADR 0018
    /// §Scope and retention: "every result reports its effective range").
    /// `None` for row/batch results, which have no scope concept.
    #[cfg(feature = "vm-stream")]
    pub scope_report: Option<ScopeReport>,
}

impl QueryResult {
    /// True when no statement produced rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// A stream query's effective range (ADR 0018 §Scope and retention): "a
/// default that is silent is a wrong answer waiting to be trusted."
#[cfg(feature = "vm-stream")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeReport {
    /// Rows the query actually saw.
    pub lines: u64,
    /// Earliest event timestamp seen, nanoseconds since epoch.
    pub first_ts: Option<i64>,
    /// Latest event timestamp seen, nanoseconds since epoch.
    pub last_ts: Option<i64>,
    /// The scope the query asked for.
    pub scope_requested: crate::vm::stream::Scope,
    /// The scope actually available (narrower than requested when the
    /// ring/file cannot reach far enough back).
    pub scope_available: crate::vm::stream::Scope,
    /// `true` when `scope_available` was clamped at the start of the file.
    pub capped: bool,
}

/// One `EXPLAIN QUERY PLAN` step. The same shape as row's `EqpRow` (minus
/// SQLite's unused third column) and batch's `codegen::batch::PlanNode`:
/// a tree encoded as `(id, parent)` pairs plus a human-readable `detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    /// This step's identifier, unique within the plan.
    pub id: i64,
    /// The parent step's `id`; `0` (row) or `id` itself (batch) for a root.
    pub parent: i64,
    /// `SCAN t`, `SEARCH t USING INDEX ...`, `HASH JOIN ...`, ...
    pub detail: String,
}

/// One instruction of an `EXPLAIN` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpcodeRow {
    /// Program address (instruction index).
    pub addr: usize,
    /// The opcode's mnemonic.
    pub opcode: String,
    /// Operands, rendered. Row: `p1 p2 p3 p4`; batch: whatever the batch
    /// opcode carries.
    pub operands: String,
}

/// A labelled run of opcodes. Row programs are one section (`main`);
/// batch programs have one per phase (`build`, `probe`, `body`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpcodeSection {
    /// Section name.
    pub label: String,
    /// The section's instructions, in address order.
    pub rows: Vec<OpcodeRow>,
}

/// A table's name and columns, as known from the file's schema without
/// running a query -- a schema-tree UI's shape (#310, db-studio#10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    /// The table's name.
    pub name: String,
    /// The table's columns, in declared order.
    pub columns: Vec<ColumnInfo>,
}

/// One column of a [`TableInfo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    /// The column's name.
    pub name: String,
    /// Declared type text, e.g. `INTEGER`/`TEXT` (empty when the column
    /// has none -- SQLite allows untyped columns).
    pub type_name: String,
}

/// What an engine knows about its file without scanning it. One variant
/// per mode because the honest numbers differ: a SQLite file has a page
/// count in its header; a Parquet file has row groups and a row count in
/// its footer; a log file has only "how much have I parsed so far".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStats {
    /// A SQLite-format file (from the 100-byte header, no scan).
    Row {
        /// Page size in bytes.
        page_size: u32,
        /// Number of pages in the file, per the header.
        page_count: u32,
        /// Pages on the freelist.
        freelist_pages: u32,
    },
    /// A Parquet file (from the footer).
    Batch {
        /// Row groups (segments).
        row_groups: usize,
        /// Total rows across row groups.
        rows: i64,
    },
    /// A log file (parse progress).
    Stream {
        /// Bytes consumed by the parser so far.
        bytes_parsed: u64,
        /// Lines yielded so far.
        lines: u64,
    },
}

/// Where an [`EngineError`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The file could not be opened or is not this engine's format.
    Open,
    /// The SQL did not parse.
    Parse,
    /// The SQL parsed but could not be compiled (unknown table, unsupported
    /// construct, ...).
    Compile,
    /// The program ran and failed (constraint violation, I/O, lock).
    Execute,
    /// A construct this engine does not support (e.g. `EXPLAIN` of a
    /// statement kind that has no plan).
    Unsupported,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ErrorKind::Open => "open",
            ErrorKind::Parse => "parse",
            ErrorKind::Compile => "compile",
            ErrorKind::Execute => "execute",
            ErrorKind::Unsupported => "unsupported",
        })
    }
}

/// The one error type every engine returns. A kind for the client to
/// branch on (pane to show, whether the file is still usable) plus the
/// engine's own message, which is what the client displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    /// Which phase failed.
    pub kind: ErrorKind,
    /// The underlying error's `Display`.
    pub message: String,
}

impl EngineError {
    /// Wraps any displayable error under `kind`.
    pub fn new(kind: ErrorKind, err: impl fmt::Display) -> Self {
        EngineError {
            kind,
            message: err.to_string(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for EngineError {}

/// One open file in one execution mode. Object-safe: a client may hold a
/// `Box<dyn Engine>` and switch which one is live per file.
pub trait Engine {
    /// Opens `path` in this engine's mode. Fails with
    /// [`ErrorKind::Open`] if the file is missing or not this format.
    fn open(path: &Path) -> Result<Self, EngineError>
    where
        Self: Sized;

    /// Which mode this engine drives.
    fn mode(&self) -> Mode;

    /// Runs `sql` -- one or more `;`-separated statements -- and returns
    /// the last result set produced. Statements that return no rows
    /// contribute nothing; an all-DDL/DML script yields an empty result.
    /// Transaction state (`BEGIN`/`COMMIT`) persists across calls on the
    /// same engine.
    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError>;

    /// `EXPLAIN QUERY PLAN` for a single `SELECT`. Does not execute it.
    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError>;

    /// `EXPLAIN` -- the compiled program -- for a single statement. Does
    /// not execute it.
    fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError>;

    /// What the engine knows about the file without scanning it.
    fn stats(&self) -> FileStats;

    /// Tables (and their columns) known from the file's schema, without
    /// running a query. Empty for a file with no tables.
    fn tables(&self) -> Result<Vec<TableInfo>, EngineError>;
}

/// `explain_*` (and the batch engine's `run_query`) take exactly one
/// statement: split on top-level `;`, accept one, reject none or several.
#[cfg(any(
    feature = "engine-row",
    feature = "engine-column",
    feature = "engine-stream"
))]
pub(crate) fn single_statement(sql: &str) -> Result<String, EngineError> {
    let mut stmts = crate::parser::row::tokenizer::split_statements(sql).into_iter();
    match (stmts.next(), stmts.next()) {
        (Some(one), None) => Ok(one),
        (None, _) => Err(EngineError::new(ErrorKind::Parse, "empty statement")),
        (Some(_), Some(_)) => Err(EngineError::new(
            ErrorKind::Unsupported,
            "EXPLAIN takes a single statement",
        )),
    }
}
