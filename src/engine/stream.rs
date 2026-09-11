// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The stream [`Engine`]: one live log file driven through
//! `storage::stream` (`LogFile` → `Segment`s in a `Ring`) and `vm::batch`
//! (ADR 0017, ADR 0018).
//!
//! The file is opened tail-first: the last `budget` bytes are read
//! backwards, parsed and sealed into segments held by a [`Ring`]. Every
//! query is parse → `expand_star` → severity-literal rewrite →
//! `codegen::batch::compile` → `vm::engine::run` over one [`StreamSegment`]
//! per ring segment, each materializing exactly the columns the program
//! loads. [`StreamEngine::refresh`] pulls appended lines into the ring;
//! [`StreamEngine::tail_source`] hands the live head to a `vm::batch`
//! program batch-at-a-time.
//!
//! The table is always named `log`. Single table only: `JOIN`,
//! `IN (SELECT ...)` and window functions report
//! [`ErrorKind::Unsupported`]. Queries run over what the ring holds (the
//! default scope); scope semantics, pruning and read-through belong to
//! `codegen::stream`.

mod rewrite;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::codegen::batch::{self as planner, PlanError, TableStats};
use crate::parser::ast::{Expr, ExprKind, ResultColumn, Select};
use crate::parser::ParseError;
use crate::storage::stream::adapter::{
    is_predefined, now_ns, ColumnRequest, StreamSegment, TailSource, PREDEFINED_COLUMNS,
};
use crate::storage::stream::{
    LogFile, OwnedColumn, Refresh, Ring, Segment, Source, SourceKind, SyslogParser,
};
use crate::vm::batch::Program;
use crate::vm::engine;

use super::{
    single_statement, Cell, ColumnInfo, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, TableInfo,
};

/// The one table name a stream engine serves.
pub const TABLE: &str = "log";

/// Default ring budget: bytes of log held hot.
pub const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

/// One open log file, presented as the single table `log`.
pub struct StreamEngine {
    path: PathBuf,
    file: LogFile,
    ring: Ring,
    source: Source,
    parser: SyslogParser,
    /// Tier-3 names seen in any held segment, in first-seen order.
    fields: Vec<String>,
}

impl std::fmt::Debug for StreamEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamEngine")
            .field("path", &self.path)
            .field("segments", &self.ring.len())
            .field("rows", &self.ring.rows())
            .field("bytes", &self.ring.bytes())
            .finish_non_exhaustive()
    }
}

impl StreamEngine {
    /// Open `path` holding at most `budget` bytes of the tail hot.
    pub fn open_with_budget(path: &Path, budget: usize) -> Result<Self, EngineError> {
        let mut file = LogFile::open(path)
            .map_err(|e| EngineError::new(ErrorKind::Open, format!("{}: {e}", path.display())))?;
        let blocks = file
            .read_tail(u64::try_from(budget).unwrap_or(u64::MAX))
            .map_err(|e| EngineError::new(ErrorKind::Open, format!("{}: {e}", path.display())))?;
        let source = Source::new(SourceKind::File, &path.to_string_lossy());
        let parser = SyslogParser::new();
        let mut engine = StreamEngine {
            path: path.to_path_buf(),
            file,
            ring: Ring::new(budget),
            source,
            parser,
            fields: Vec::new(),
        };
        let observed = now_ns();
        for b in &blocks {
            for s in Segment::seal_block(b, &engine.source, &engine.parser, observed) {
                engine.admit(Arc::new(s));
            }
        }
        Ok(engine)
    }

    /// The file this engine was opened on.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Segments currently held.
    #[must_use]
    pub fn ring(&self) -> &Ring {
        &self.ring
    }

    /// Pull appended complete lines into the ring. Returns the number of
    /// new rows. On truncation/rotation the ring is cleared and the tail
    /// of the new file is loaded.
    pub fn refresh(&mut self) -> Result<usize, EngineError> {
        let io = |e: std::io::Error| EngineError::new(ErrorKind::Execute, e);
        match self.file.refresh().map_err(io)? {
            Refresh::NoNew => Ok(0),
            Refresh::New(blocks) => Ok(self.seal_all(&blocks)),
            Refresh::Truncated => {
                self.ring.clear();
                self.fields.clear();
                let budget = u64::try_from(self.ring.budget()).unwrap_or(u64::MAX);
                let blocks = self.file.read_tail(budget).map_err(io)?;
                Ok(self.seal_all(&blocks))
            }
        }
    }

    fn seal_all(&mut self, blocks: &[crate::storage::stream::Block]) -> usize {
        let observed = now_ns();
        let mut rows = 0usize;
        for b in blocks {
            for s in Segment::seal_block(b, &self.source, &self.parser, observed) {
                rows = rows.saturating_add(s.len());
                self.admit(Arc::new(s));
            }
        }
        rows
    }

    fn admit(&mut self, seg: Arc<Segment>) {
        for name in seg.field_names() {
            if !self.fields.iter().any(|f| f == name) {
                self.fields.push(name.to_string());
            }
        }
        let _evicted = self.ring.push_head(seg);
    }

    /// All column names of `log`: the predefined ones, then Tier-3 fields.
    #[must_use]
    pub fn columns(&self) -> Vec<String> {
        PREDEFINED_COLUMNS
            .iter()
            .map(|s| (*s).to_string())
            .chain(self.fields.iter().cloned())
            .collect()
    }

    /// The ring's segments as `vm::batch` segments materializing `columns`
    /// — the batch-at-a-time surface for a program that drives its own
    /// execution (e.g. the driving side of a cross-mode join).
    #[must_use]
    pub fn segments(&self, columns: &[ColumnRequest]) -> Vec<StreamSegment> {
        self.ring
            .segments()
            .map(|s| StreamSegment::new(Arc::clone(s), columns.to_vec()))
            .collect()
    }

    /// A live source over the file's head, independent of this engine's
    /// ring: each `next_batch()` is the newly appended lines with `columns`.
    pub fn tail_source(
        &self,
        columns: &[ColumnRequest],
        poll: Duration,
        max_idle_polls: Option<u32>,
    ) -> Result<TailSource, EngineError> {
        let file = LogFile::open(&self.path).map_err(|e| {
            EngineError::new(ErrorKind::Open, format!("{}: {e}", self.path.display()))
        })?;
        Ok(TailSource::new(
            file,
            self.source.clone(),
            SyslogParser::new(),
            columns.to_vec(),
            poll,
            max_idle_polls,
        ))
    }

    /// Parse one `SELECT` for `log`: `*` expanded against the current
    /// columns, single-table checks, severity literals rewritten.
    fn parse_for_table(&self, sql: &str) -> Result<Select, EngineError> {
        let select = planner_parse(sql)?;
        let from = from_table(&select)?;
        if !from.eq_ignore_ascii_case(TABLE) {
            return Err(EngineError::new(
                ErrorKind::Compile,
                format!("unknown table: {from} (a log file is table `{TABLE}`)"),
            ));
        }
        if let Some(join) = select.from.as_ref().and_then(|f| f.joins.first()) {
            let right = join.table.name().unwrap_or("<subquery>");
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                format!("JOIN with `{right}`: the stream engine has one table"),
            ));
        }
        if in_subquery(&select).is_some() {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                "IN (SELECT ...) needs a second table; the stream engine has one",
            ));
        }
        if select.columns.iter().any(is_window_column) {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                "window functions are not available through the stream engine yet",
            ));
        }
        let mut select = planner::expand_star(&select, &self.columns()).map_err(plan_err)?;
        rewrite::severity_literals(&mut select)?;
        Ok(select)
    }

    /// Resolve the program's `columns_to_load()`: every name must be a
    /// predefined column or a Tier-3 field seen in the ring.
    fn requests(&self, names: &[String]) -> Result<Vec<ColumnRequest>, EngineError> {
        names
            .iter()
            .map(|name| {
                let (_, col) = planner::split_qualified(name);
                if is_predefined(col) || self.fields.iter().any(|f| f == col) {
                    Ok(ColumnRequest {
                        key: name.clone(),
                        name: col.to_string(),
                    })
                } else {
                    Err(EngineError::new(
                        ErrorKind::Compile,
                        format!("unknown column: {name}"),
                    ))
                }
            })
            .collect()
    }

    fn table_stats(&self) -> TableStats {
        TableStats {
            row_groups: self.ring.len(),
            rows: i64::try_from(self.ring.rows()).unwrap_or(i64::MAX),
        }
    }

    /// Declared type of a column for [`TableInfo`].
    fn type_name(&self, name: &str) -> &'static str {
        match name {
            "timestamp" | "observed_ts" | "severity" => "INTEGER",
            "severity_text" | "facility" | "message" | "raw" => "TEXT",
            other => {
                self.ring
                    .segments()
                    .find_map(|s| s.field(other))
                    .map_or("TEXT", |c| match c {
                        OwnedColumn::Int(_) => "INTEGER",
                        OwnedColumn::Float(_) => "REAL",
                        OwnedColumn::Bool(_) => "BOOLEAN",
                        OwnedColumn::Dict { .. } | OwnedColumn::Str(_) => "TEXT",
                    })
            }
        }
    }
}

fn planner_parse(sql: &str) -> Result<Select, EngineError> {
    crate::parser::parse(sql).map_err(|e: ParseError| EngineError::new(ErrorKind::Parse, e))
}

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

fn plan_err(e: PlanError) -> EngineError {
    match e {
        PlanError::Internal(msg) => EngineError::new(
            ErrorKind::Execute,
            format!("planner invariant violated: {msg}"),
        ),
        other => EngineError::new(ErrorKind::Compile, other),
    }
}

impl Engine for StreamEngine {
    fn open(path: &Path) -> Result<Self, EngineError> {
        Self::open_with_budget(path, DEFAULT_BUDGET)
    }

    fn mode(&self) -> Mode {
        Mode::Stream
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        let program: Program = planner::compile(&select).map_err(plan_err)?;
        let columns = self.requests(&program.columns_to_load())?;
        let segments = self.segments(&columns);
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
        let stats = self.table_stats();
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
        FileStats::Stream {
            bytes_parsed: self.file.bytes_read(),
            lines: u64::try_from(self.ring.rows()).unwrap_or(u64::MAX),
        }
    }

    fn tables(&self) -> Result<Vec<TableInfo>, EngineError> {
        Ok(vec![TableInfo {
            name: TABLE.to_string(),
            columns: self
                .columns()
                .iter()
                .map(|name| ColumnInfo {
                    name: name.clone(),
                    type_name: self.type_name(name).to_string(),
                })
                .collect(),
        }])
    }
}
