// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The stream [`Engine`]: one live log file driven through
//! `storage::stream` (`LogFile` → `Segment`s in a `Ring`) and `vm::batch`
//! (ADR 0017, ADR 0018).
//!
//! The file is opened tail-first: the last `budget` bytes are read
//! backwards, parsed and sealed into segments held by a [`Ring`]. Every
//! query is parse → `expand_star` → `codegen::stream::compile` (`Prune`
//! prologue + a `vm::batch::Program` body) → segment selection over the
//! `Prune` → `vm::engine::run` over one [`StreamSegment`] per surviving
//! ring segment, each materializing exactly the columns the program
//! loads. [`StreamEngine::refresh`] pulls appended lines into the ring;
//! [`StreamEngine::tail_source`] hands the live head to a `vm::batch`
//! program batch-at-a-time.
//!
//! The table is always named `log`. Single table only: `JOIN`,
//! `IN (SELECT ...)` and window functions report
//! [`ErrorKind::Unsupported`]. Scope wider than the ring should read
//! through the file via a sidecar index (ADR 0018); no sidecar exists
//! yet (#323), so a query outside the ring's `Lines`/`Bytes`/`All` scope
//! runs over what the ring holds and reports `capped` rather than
//! reading further back.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::codegen::batch::{self as planner, PlanError, TableStats};
use crate::codegen::stream::{self as stream_planner, StreamPlanError};
use crate::parser::ast::{Expr, ExprKind, ResultColumn, ScopeUnit, Select};
use crate::parser::ParseError;
use crate::storage::stream::adapter::{
    is_predefined, now_ns, ColumnRequest, StreamSegment, TailSource, PREDEFINED_COLUMNS,
};
use crate::storage::stream::{
    LogFile, OwnedColumn, Refresh, Ring, Segment, Source, SourceKind, SyslogParser,
};
use crate::vm::stream::{IndexPred, Program, Scope};

use super::{
    single_statement, Cell, ColumnInfo, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, ScopeReport, TableInfo,
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
        let select = planner::expand_star(&select, &self.columns()).map_err(plan_err)?;
        Ok(select)
    }

    /// Rewrites every `severity <cmp> '<name>'` in `select` into its
    /// numeric code (`WARN` etc. are the stream engine's own literal
    /// convention, not something `codegen::batch` understands) -- exposed
    /// so a cross-mode caller (#317) can apply the same rewrite this
    /// engine's own [`Self::parse_for_table`] does before planning a join.
    pub fn rewrite_severity_literals(select: &mut Select) -> Result<(), EngineError> {
        crate::codegen::stream::rewrite_severity_literals(select)
            .map_err(|e| EngineError::new(ErrorKind::Compile, e.to_string()))
    }

    /// Resolve a program's `columns_to_load()` (possibly table-qualified,
    /// e.g. `log.severity` -- the driving side of a cross-mode join,
    /// #317) into requests against this ring: every name must be a
    /// predefined column or a Tier-3 field seen in the ring.
    #[must_use = "validate every requested column before scanning"]
    pub fn column_requests(&self, names: &[String]) -> Result<Vec<ColumnRequest>, EngineError> {
        self.requests(names)
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

    /// Segments surviving `prune`, plus the effective range those
    /// segments cover. `scope`/`SINCE`/`UNTIL` bound *observed* time --
    /// "since I opened/ingested this" (ADR 0018's own distinction between
    /// `event_ts_ns` and `observed_ts_ns`) -- so a fixture seeded with old
    /// event timestamps but ingested just now still matches the default
    /// scope; an ordinary `WHERE timestamp ...` predicate still filters
    /// event time as part of the residual query. `DictEq` predicates then
    /// drop any segment whose dictionary lacks the value
    /// (`OwnedColumn::dict_contains`). `Scope::Lines`/`Bytes`/`All` have
    /// no fixed nanosecond edge to prune by and no sidecar index to read
    /// through yet (#323), so they fall back to every segment the ring
    /// currently holds -- correct, just not narrowed.
    fn select_segments(
        &self,
        prune: &crate::vm::stream::Prune,
        columns: &[ColumnRequest],
    ) -> (Vec<StreamSegment>, ScopeReport) {
        let now = now_ns();
        let requested_range = effective_time_range(&prune.scope, now, &prune.preds);
        // Prune segments by time-range overlap vs. all segments in the ring.
        let mut candidates: Vec<Arc<Segment>> = match &requested_range {
            Some(range) => self
                .ring
                .segments()
                .filter(|s| match s.minmax_observed() {
                    Some((lo, hi)) => lo < range.end && hi >= range.start,
                    None => true,
                })
                .cloned()
                .collect(),
            None => self.ring.segments().cloned().collect(),
        };
        for pred in &prune.preds {
            if let IndexPred::DictEq { column, value } = pred {
                candidates.retain(|s| {
                    s.field(column)
                        .and_then(|c| c.dict_contains(value))
                        .unwrap_or(true)
                });
            }
        }

        let lines = candidates
            .iter()
            .map(|s| u64::try_from(s.len()).unwrap_or(u64::MAX))
            .fold(0u64, u64::saturating_add);
        let (first_ts, last_ts) = candidates.iter().filter_map(|s| s.minmax_event()).fold(
            (None, None),
            |(lo, hi), (mn, mx)| {
                (
                    Some(lo.map_or(mn, |l: i64| l.min(mn))),
                    Some(hi.map_or(mx, |h: i64| h.max(mx))),
                )
            },
        );
        // Capped when the request reaches further back than the ring's
        // oldest held line -- read-through would serve the rest once
        // #323 lands; today the report is honest that it did not.
        let capped = match (&requested_range, self.ring.segments().next()) {
            (Some(range), Some(oldest)) => oldest
                .minmax_observed()
                .is_some_and(|(oldest_lo, _)| range.start < oldest_lo),
            _ => false,
        };

        let segments = candidates
            .into_iter()
            .map(|s| StreamSegment::new(s, columns.to_vec()))
            .collect();
        let report = ScopeReport {
            lines,
            first_ts,
            last_ts,
            scope_requested: prune.scope,
            scope_available: prune.scope,
            capped,
        };
        (segments, report)
    }

    /// This engine's `log` table as a [`TableStats`], `source: None` --
    /// callers labelling a cross-mode plan (#317) fill that in themselves.
    #[must_use]
    pub fn table_stats(&self) -> TableStats {
        TableStats {
            row_groups: self.ring.len(),
            rows: i64::try_from(self.ring.rows()).unwrap_or(i64::MAX),
            source: None,
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

fn stream_err(e: StreamPlanError) -> EngineError {
    match e {
        StreamPlanError::Rejected { .. } => EngineError::new(ErrorKind::Unsupported, e),
        StreamPlanError::InvalidLiteral { .. } => EngineError::new(ErrorKind::Compile, e),
        StreamPlanError::Batch(inner) => plan_err(inner),
    }
}

/// The query's effective `Scope`: the tightest `SINCE` bound if the
/// query has one, else the built-in default (ADR 0018 §Scope and
/// retention: the default itself is a client concern, not db-core's --
/// this fallback only covers a bare `SELECT ... FROM log` with no client
/// wired up yet).
const DEFAULT_SCOPE: Duration = Duration::from_secs(3600);

fn resolve_scope(select: &Select) -> Scope {
    let Some(since) = select.scope.as_ref().and_then(|c| c.since.as_ref()) else {
        return Scope::Time(DEFAULT_SCOPE);
    };
    match since.unit {
        ScopeUnit::Seconds => Scope::Time(Duration::from_secs(since.amount)),
        ScopeUnit::Minutes => Scope::Time(Duration::from_secs(since.amount.saturating_mul(60))),
        ScopeUnit::Hours => Scope::Time(Duration::from_secs(since.amount.saturating_mul(3_600))),
        ScopeUnit::Days => Scope::Time(Duration::from_secs(since.amount.saturating_mul(86_400))),
        ScopeUnit::Lines => Scope::Lines(since.amount),
        ScopeUnit::Bytes => Scope::Bytes(since.amount),
    }
}

/// The `[lo, hi)` event-time range `scope`/`preds` together imply, or
/// `None` for `Lines`/`Bytes`/`All` (no fixed nanosecond edge --
/// `select_segments` falls back to every held segment for those).
fn effective_time_range(scope: &Scope, now_ns: i64, preds: &[IndexPred]) -> Option<Range<i64>> {
    let mut lo = i64::MIN;
    let mut hi = i64::MAX;
    let mut has_bound = false;
    if let Scope::Time(d) = scope {
        let delta = i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
        lo = lo.max(now_ns.saturating_sub(delta));
        has_bound = true;
    }
    for pred in preds {
        if let IndexPred::TimeRange { lo: plo, hi: phi } = pred {
            lo = lo.max(*plo);
            hi = hi.min(*phi);
            has_bound = true;
        }
    }
    has_bound.then_some(lo..hi)
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
        let scope = resolve_scope(&select);
        let program: Program = stream_planner::compile(&select, scope).map_err(stream_err)?;
        let columns = self.requests(&program.body.columns_to_load())?;
        let (segments, scope_report) = self.select_segments(&program.prune, &columns);
        let rows = crate::vm::engine::run(&segments, &program.body)
            .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
        Ok(QueryResult {
            columns: planner::output_column_names(&select),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(Cell::from).collect())
                .collect(),
            scope_report: Some(scope_report),
        })
    }

    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        let stmt = single_statement(sql)?;
        let select = self.parse_for_table(&stmt)?;
        let stats = self.table_stats();
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
