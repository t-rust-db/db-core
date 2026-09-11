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

use crate::clock::{Clock, SystemClock};
use crate::codegen::batch::{self as planner, PlanError, TableStats};
use crate::codegen::stream::{self as stream_planner, StreamPlanError};
use crate::parser::ast::{Expr, ExprKind, FunctionArgs, ResultColumn, ScopeUnit, Select};
use crate::parser::ParseError;
use crate::storage::stream::adapter::{
    is_predefined, ColumnRequest, StreamSegment, TailSource, PREDEFINED_COLUMNS,
};
use crate::storage::stream::{
    detect, DetectedParser, EvictedSummary, LogFile, OwnedColumn, Refresh, Ring, Segment, Source,
    SourceKind,
};
use crate::vm::batch::{AggFunc, Value};
use crate::vm::stream::{EmitMode, IndexPred, Program, Scope};

use super::{
    single_statement, Cell, ColumnInfo, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, ScopeReport, TableInfo,
};

/// The one table name a stream engine serves.
pub const TABLE: &str = "log";

/// Default ring budget: bytes of log held hot.
pub const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

/// Ring autoscaling floor (#308 ADR 0018 §Storage: `target =
/// clamp(rate_ewma * default_scope, min, hard_cap)`) -- never shrink the
/// ring below one segment's worth of headroom even on a near-idle file.
pub const MIN_RING_BUDGET: usize = 4 * 1024 * 1024;

/// Ring autoscaling ceiling: never grow past this regardless of burst
/// rate, so a runaway feed cannot exhaust memory.
pub const HARD_CAP_RING_BUDGET: usize = 512 * 1024 * 1024;

/// EWMA smoothing constant for the ingestion-rate sample (bytes/sec)
/// driving autoscaling -- closer to `1.0` reacts faster to bursts,
/// closer to `0.0` smooths harder. The ADR pins the *formula*, not this
/// constant; this value has no measured basis and may need tuning.
const RATE_EWMA_ALPHA: f64 = 0.3;

/// One open log file, presented as the single table `log`.
pub struct StreamEngine {
    path: PathBuf,
    file: LogFile,
    ring: Ring,
    source: Source,
    parser: DetectedParser,
    /// Tier-3 names seen in any held segment, in first-seen order.
    fields: Vec<String>,
    clock: Box<dyn Clock>,
    /// Smoothed ingestion rate, bytes/sec, feeding ring autoscaling.
    rate_ewma_bytes_per_sec: f64,
    /// Wall-clock time of the last rate sample, for computing elapsed time
    /// on the next one.
    last_sample_ns: Option<i64>,
    /// The default scope this engine autoscales the ring toward holding
    /// hot (ADR 0018: "the ring's job is to hold at least the default
    /// query scope") -- set once at open, not re-read per query, since
    /// autoscaling reacts to *ingestion rate*, not to any one query's
    /// own `SINCE`.
    autoscale_target_scope: Duration,
    /// Off by default (#308): `open_with_budget`'s `budget` is an
    /// explicit client contract every existing caller (and #306/#307's
    /// own tests) relies on staying exactly what they set. Autoscaling
    /// only engages after [`Self::enable_autoscaling`].
    autoscale_enabled: bool,
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
        let sample: &[u8] = blocks.first().map_or(&[], |b| &b.bytes[..]);
        let detection = detect::detect(sample, detect::DEFAULT_SAMPLE_LINES);
        let parser = DetectedParser::for_format(detection.format);
        let mut engine = StreamEngine {
            path: path.to_path_buf(),
            file,
            ring: Ring::new(budget),
            source,
            parser,
            fields: Vec::new(),
            clock: Box::new(SystemClock),
            rate_ewma_bytes_per_sec: 0.0,
            last_sample_ns: None,
            autoscale_target_scope: DEFAULT_SCOPE,
            autoscale_enabled: false,
        };
        let observed = engine.clock.now_ns();
        for b in &blocks {
            for s in Segment::seal_block(b, &engine.source, &engine.parser, observed) {
                engine.admit(Arc::new(s));
            }
        }
        Ok(engine)
    }

    /// Replace this engine's time source (#308) -- production code has
    /// no reason to call this (the real clock is the default); tests use
    /// it to simulate ingestion over minutes/hours/days of ring
    /// autoscaling and summary-horizon behavior without real `sleep`s.
    pub fn set_clock(&mut self, clock: Box<dyn Clock>) {
        self.clock = clock;
    }

    /// Turn on ring autoscaling (#308, off by default): `target =
    /// clamp(rate_ewma * target_scope, MIN_RING_BUDGET,
    /// HARD_CAP_RING_BUDGET)`, resampled on every [`Self::refresh`].
    /// Also raises the retained-summary horizon to `target_scope` (a
    /// wider ring should keep at least as much summarized history).
    pub fn enable_autoscaling(&mut self, target_scope: Duration) {
        self.autoscale_enabled = true;
        self.autoscale_target_scope = target_scope;
        let horizon_ns = i64::try_from(target_scope.as_nanos()).unwrap_or(i64::MAX);
        self.ring.set_summary_horizon(horizon_ns);
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
                let sample: &[u8] = blocks.first().map_or(&[], |b| &b.bytes[..]);
                let detection = detect::detect(sample, detect::DEFAULT_SAMPLE_LINES);
                self.parser = DetectedParser::for_format(detection.format);
                Ok(self.seal_all(&blocks))
            }
        }
    }

    fn seal_all(&mut self, blocks: &[crate::storage::stream::Block]) -> usize {
        let observed = self.clock.now_ns();
        let mut rows = 0usize;
        let mut bytes = 0usize;
        for b in blocks {
            for s in Segment::seal_block(b, &self.source, &self.parser, observed) {
                rows = rows.saturating_add(s.len());
                bytes = bytes.saturating_add(s.byte_len());
                self.admit(Arc::new(s));
            }
        }
        self.sample_rate_and_autoscale(bytes, observed);
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

    /// Ring autoscaling (#308, ADR 0018 §Storage): `target =
    /// clamp(rate_ewma * default_scope, min, hard_cap)`. Updates the
    /// smoothed bytes/sec rate from `bytes_admitted` over the elapsed
    /// time since the last sample, then resizes the ring toward holding
    /// at least `autoscale_target_scope` of that rate -- grows on a
    /// sustained burst, shrinks on sustained idle. The first sample (no
    /// prior `last_sample_ns`) only seeds the rate; it does not resize,
    /// since one elapsed-time-of-zero sample would divide by zero.
    fn sample_rate_and_autoscale(&mut self, bytes_admitted: usize, now_ns: i64) {
        if !self.autoscale_enabled {
            return;
        }
        let Some(last) = self.last_sample_ns else {
            self.last_sample_ns = Some(now_ns);
            return;
        };
        self.last_sample_ns = Some(now_ns);
        let elapsed_ns = now_ns.saturating_sub(last);
        if elapsed_ns <= 0 {
            return;
        }
        let elapsed_secs = elapsed_ns as f64 / 1e9;
        let sample = bytes_admitted as f64 / elapsed_secs;
        self.rate_ewma_bytes_per_sec =
            RATE_EWMA_ALPHA * sample + (1.0 - RATE_EWMA_ALPHA) * self.rate_ewma_bytes_per_sec;

        let target_scope_secs = self.autoscale_target_scope.as_secs_f64();
        let target_bytes = (self.rate_ewma_bytes_per_sec * target_scope_secs)
            .clamp(0.0, HARD_CAP_RING_BUDGET as f64);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to [0.0, HARD_CAP_RING_BUDGET as f64] just above"
        )]
        let target = (target_bytes as usize).clamp(MIN_RING_BUDGET, HARD_CAP_RING_BUDGET);
        if target != self.ring.budget() {
            let _evicted = self.ring.set_budget(target);
        }
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
            self.parser.clone(),
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
        let now = self.clock.now_ns();
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

    /// Folds retained per-segment summaries (#308, ADR 0018: "a live
    /// `count(*) ... since 1d` over a 42-minute ring merges a day of
    /// summaries, re-aggregates the hot ring, and adds the head") into
    /// `rows` when `select` is exactly one of the decomposable shapes:
    /// `COUNT(*)`, `COUNT(col)`, `SUM(col)`, `MIN(col)`, `MAX(col)`, with
    /// no `GROUP BY` and no `WHERE` -- a query with either of those has
    /// no way to apply itself to a summary (a summary has no rows left
    /// to filter or bucket), so it is left exactly as the ring answered
    /// it, still reported `capped` if the request reached further back.
    /// `AVG` is excluded too: `vm::engine::run`'s result is already
    /// finalized (sum/count already divided), so there is nothing left
    /// to merge a summary's own sum/count into.
    fn merge_retained_summaries(
        &self,
        select: &Select,
        requested_range: Option<Range<i64>>,
        rows: Vec<Vec<Value>>,
        report: &mut ScopeReport,
    ) -> Vec<Vec<Value>> {
        let Some(range) = requested_range else {
            return rows;
        };
        if !select.group_by.is_empty() || select.where_clause.is_some() {
            return rows;
        }
        let Some((agg, column)) = single_ungrouped_aggregate(select) else {
            return rows;
        };
        let summaries = self.ring.summaries_overlapping(&range);
        if summaries.is_empty() {
            return rows;
        }
        let live = rows.into_iter().next().and_then(|r| r.into_iter().next());
        let merged = fold_summaries_into(agg, column.as_deref(), &summaries, live);
        report.capped = false;
        report.lines = report.lines.saturating_add(
            summaries
                .iter()
                .map(|e| e.summary.rows)
                .fold(0u64, u64::saturating_add),
        );
        vec![vec![merged]]
    }

    /// Runs an already-compiled `program` for `select` against this
    /// engine's current ring state (segment selection, `vm::batch`
    /// execution, epilogue/summary handling) -- the shared tail of
    /// [`Engine::run_query`] and [`StandingQuery::poll`] (#309), so a
    /// standing query re-evaluates through the exact same path a one-shot
    /// query does rather than a parallel, potentially-diverging copy.
    fn run_compiled(
        &mut self,
        select: &Select,
        program: &Program,
    ) -> Result<QueryResult, EngineError> {
        let columns = self.requests(&program.body.columns_to_load())?;
        let (segments, mut scope_report) = self.select_segments(&program.prune, &columns);
        let rows = crate::vm::engine::run(&segments, &program.body)
            .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;

        if let Some(epilogue) = &program.epilogue {
            let (out_columns, out_rows) = run_range_vector_epilogue(select, epilogue, rows)?;
            return Ok(QueryResult {
                columns: out_columns,
                rows: out_rows,
                scope_report: Some(scope_report),
            });
        }

        let now = self.clock.now_ns();
        let requested_range = effective_time_range(&program.prune.scope, now, &program.prune.preds);
        let rows = self.merge_retained_summaries(select, requested_range, rows, &mut scope_report);
        Ok(QueryResult {
            columns: planner::output_column_names(select),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(Cell::from).collect())
                .collect(),
            scope_report: Some(scope_report),
        })
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

/// A compiled query re-evaluated on demand (ADR 0018 §Consequences: "a
/// standing query is a compiled program plus an interval and a `for`
/// duration, evaluated on each segment seal in the client process").
/// Client-driven, not a background thread or server: db-core owns no
/// scheduler. The caller decides when to call [`Self::poll`] -- typically
/// after each [`StreamEngine::refresh`], or on its own `interval` cadence
/// -- and reads [`Self::interval`] as a hint for that cadence.
pub struct StandingQuery {
    select: Select,
    program: Program,
    sql: String,
    mode: EmitMode,
    interval: Duration,
    for_duration: Duration,
    /// The last result set an `OnChange` poll fired on -- `None` before
    /// the first fire.
    last_fired_rows: Option<Vec<Vec<Cell>>>,
    /// When a `Threshold` condition most recently started holding
    /// continuously, per the polling engine's own clock -- `None` while
    /// not holding. Reset the moment the condition stops holding, so a
    /// later re-crossing starts a fresh `for_duration` count rather than
    /// picking up where an earlier, already-fired hold left off.
    threshold_since_ns: Option<i64>,
    /// Whether the current continuous hold (tracked by
    /// `threshold_since_ns`) has already fired -- cleared alongside it,
    /// so exactly one event fires per hold reaching `for_duration`, not
    /// one per poll for as long as the condition keeps holding
    /// afterwards (#309's "done when" criterion).
    fired_for_current_hold: bool,
}

/// One [`StandingQuery::poll`] fire: the result set that triggered it.
#[derive(Debug, Clone, PartialEq)]
pub struct StandingQueryEvent {
    /// The query's result at the moment of firing.
    pub result: QueryResult,
}

impl StandingQuery {
    /// Parses and compiles `sql` against `engine`'s current schema once
    /// (the user-facing contract: `StandingQuery` owns the SQL text, not
    /// a caller-supplied `Program`, so `poll` never recompiles). `sql`
    /// must be a plain `log`-table `SELECT`, exactly as `run_query`
    /// accepts.
    ///
    /// # Errors
    /// `ErrorKind::Compile` if `mode` is `Threshold` with a non-comparison
    /// operator (`AND`/`OR`/arithmetic/`||`/bitwise all make no sense as
    /// a threshold check), or if `sql` fails to parse/compile for any of
    /// `run_query`'s own reasons.
    pub fn new(
        engine: &StreamEngine,
        sql: &str,
        mode: EmitMode,
        interval: Duration,
        for_duration: Duration,
    ) -> Result<Self, EngineError> {
        if let EmitMode::Threshold { op, .. } = &mode {
            if !is_comparison_op(*op) {
                return Err(EngineError::new(
                    ErrorKind::Compile,
                    format!("EmitMode::Threshold needs a comparison operator, got {op:?}"),
                ));
            }
        }
        let stmt = single_statement(sql)?;
        let select = engine.parse_for_table(&stmt)?;
        let scope = resolve_scope(&select);
        let program: Program =
            stream_planner::compile(&select, scope, engine.clock.now_ns()).map_err(stream_err)?;
        Ok(Self {
            select,
            program,
            sql: sql.to_string(),
            mode,
            interval,
            for_duration,
            last_fired_rows: None,
            threshold_since_ns: None,
            fired_for_current_hold: false,
        })
    }

    /// The SQL this standing query re-evaluates on every poll.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The polling cadence the caller asked for -- a hint db-core stores
    /// but never itself schedules against (no server, no timer thread:
    /// ADR 0018).
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Re-evaluates this standing query against `engine`'s current ring
    /// state (the same execution path [`Engine::run_query`] uses,
    /// via [`StreamEngine::run_compiled`]) and decides, per `self.mode`,
    /// whether this poll constitutes a fire. `engine.refresh()` is the
    /// caller's responsibility -- `poll` never pulls new data in itself,
    /// matching the "client-side callback" model: the caller decides when
    /// a segment has sealed or `interval` has elapsed, not db-core.
    ///
    /// # Errors
    /// Whatever `run_compiled`'s own execution can fail with
    /// (`ErrorKind::Execute`/`ErrorKind::Compile`).
    pub fn poll(
        &mut self,
        engine: &mut StreamEngine,
    ) -> Result<Option<StandingQueryEvent>, EngineError> {
        let result = engine.run_compiled(&self.select, &self.program)?;
        match self.mode.clone() {
            EmitMode::Rows => Ok(Some(StandingQueryEvent { result })),
            EmitMode::OnChange => Ok(self.poll_on_change(result)),
            EmitMode::Threshold { op, threshold } => {
                Ok(self.poll_threshold(engine, op, threshold, result))
            }
        }
    }

    /// Fires only on a transition (a result set different from the last
    /// one this same query fired on), never on a repeat of that same
    /// result. The very first poll fires if it has any rows at all --
    /// "nothing seen yet" -> "something" is itself a transition; an empty
    /// result never fires (there is no result to alert on) but does
    /// update `last_fired_rows` so a later change *back* to that same
    /// emptiness is not mistaken for a fresh transition.
    fn poll_on_change(&mut self, result: QueryResult) -> Option<StandingQueryEvent> {
        let changed = self.last_fired_rows.as_ref() != Some(&result.rows);
        if !changed {
            return None;
        }
        self.last_fired_rows = Some(result.rows.clone());
        if result.rows.is_empty() {
            None
        } else {
            Some(StandingQueryEvent { result })
        }
    }

    /// Fires once when the reduced value crosses `op`/`threshold` and has
    /// held continuously for at least `self.for_duration` -- not on every
    /// subsequent poll while it remains crossed. Uses `engine`'s own
    /// clock (so `FakeClock`-driven tests control this deterministically,
    /// same as ring autoscaling did in #308) rather than the system
    /// clock.
    fn poll_threshold(
        &mut self,
        engine: &StreamEngine,
        op: crate::parser::ast::BinaryOp,
        threshold: f64,
        result: QueryResult,
    ) -> Option<StandingQueryEvent> {
        let value = latest_scalar(&result);
        let holds = value.is_some_and(|v| crate::vm::stream::threshold_holds(op, threshold, &v));
        if !holds {
            self.threshold_since_ns = None;
            self.fired_for_current_hold = false;
            return None;
        }
        let now = engine.clock.now_ns();
        let since = *self.threshold_since_ns.get_or_insert(now);
        let held_ns = now.saturating_sub(since);
        let for_ns = i64::try_from(self.for_duration.as_nanos()).unwrap_or(i64::MAX);
        if held_ns >= for_ns && !self.fired_for_current_hold {
            self.fired_for_current_hold = true;
            Some(StandingQueryEvent { result })
        } else {
            None
        }
    }
}

/// `EmitMode::Threshold`'s six valid comparisons -- `AND`/`OR`/
/// arithmetic/`||`/bitwise ops make no sense as a threshold and are
/// rejected by `StandingQuery::new`.
fn is_comparison_op(op: crate::parser::ast::BinaryOp) -> bool {
    use crate::parser::ast::BinaryOp;
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    )
}

/// The last row's last cell as a `vm::batch::Value`, for `Threshold`
/// evaluation -- a range-vector query's output is `(window_start, value)`
/// per window (#308), so the most recent window is its last row, and the
/// reduced value is always its final column.
fn latest_scalar(result: &QueryResult) -> Option<Value> {
    let cell = result.rows.last()?.last()?;
    Some(match cell {
        Cell::Null => Value::Null,
        Cell::Int(n) => Value::Int(*n),
        Cell::Real(x) => Value::Float(*x),
        Cell::Bool(b) => Value::Bool(*b),
        Cell::Text(s) => Value::Str(s.clone().into()),
        Cell::Blob(_) => Value::Null,
    })
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

/// Drives `program.body`'s `(timestamp, value)` rows through
/// [`crate::vm::stream::run_epilogue`] (#308): buckets into tumbling
/// windows, reduces each, and returns `(window_start_ns, reduced_value)`
/// rows -- late rows (behind the watermark) are dropped from the result
/// here, not reported; `#309`'s standing-query surface is the intended
/// consumer of a side output, so this query-at-a-time path only needs
/// the on-time rows.
fn run_range_vector_epilogue(
    select: &Select,
    epilogue: &crate::vm::stream::Epilogue,
    rows: Vec<Vec<Value>>,
) -> Result<(Vec<String>, Vec<Vec<Cell>>), EngineError> {
    let epilogue_rows: Vec<crate::vm::stream::EpilogueRow> = rows
        .into_iter()
        .map(|mut r| {
            let value = r.pop().unwrap_or(Value::Null);
            let ts = match r.first() {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            (ts, value)
        })
        .collect();
    let (windows, _late) = crate::vm::stream::run_epilogue(&epilogue_rows, epilogue);
    let label = range_vector_label(select);
    let out_rows = windows
        .into_iter()
        .map(|(start, value)| vec![Cell::Int(start), Cell::from(value)])
        .collect();
    Ok((vec!["window_start".to_string(), label], out_rows))
}

/// The output label for a range-vector query's one value column, e.g.
/// `COUNT_OVER_TIME(message)` -- `select` is known to have exactly one
/// `ResultColumn::Expr` carrying a `FunctionCall` (that is what made
/// `codegen::stream::compile` build an [`Epilogue`] in the first place).
fn range_vector_label(select: &Select) -> String {
    match select.columns.first() {
        Some(ResultColumn::Expr {
            expr:
                Expr {
                    kind: ExprKind::FunctionCall { name, args, .. },
                    ..
                },
            ..
        }) => {
            let arg = match args {
                FunctionArgs::Star => "*".to_string(),
                FunctionArgs::List(list) => list
                    .iter()
                    .filter_map(|e| match &e.kind {
                        ExprKind::Column { name, .. } => Some(name.clone()),
                        _ => None,
                    })
                    .next()
                    .unwrap_or_default(),
            };
            format!("{}({arg})", name.to_ascii_uppercase())
        }
        _ => "value".to_string(),
    }
}

/// If `select` is exactly one ungrouped, unfiltered aggregate call over
/// `*` or one column -- `COUNT(*)`, `COUNT(x)`, `SUM(x)`, `MIN(x)`,
/// `MAX(x)` -- the function and, for anything but `COUNT(*)`, the column
/// name. `None` for every other shape (multiple result columns, a
/// non-aggregate expression, `AVG`, `DISTINCT`, or any argument that
/// isn't a bare column) -- [`StreamEngine::merge_retained_summaries`]'s
/// only caller leaves those to the ring exactly as it answered them.
fn single_ungrouped_aggregate(select: &Select) -> Option<(AggFunc, Option<String>)> {
    let [ResultColumn::Expr { expr, .. }] = select.columns.as_slice() else {
        return None;
    };
    let Expr {
        kind:
            ExprKind::FunctionCall {
                name,
                distinct: false,
                args,
                tail,
            },
        ..
    } = expr
    else {
        return None;
    };
    if tail
        .as_deref()
        .is_some_and(|t| t.filter.is_some() || t.over.is_some())
    {
        return None;
    }
    let agg = AggFunc::from_name(name)?;
    if matches!(agg, AggFunc::Avg) {
        return None;
    }
    match args {
        FunctionArgs::Star if matches!(agg, AggFunc::Count) => Some((agg, None)),
        FunctionArgs::Star => None,
        FunctionArgs::List(list) => match list.as_slice() {
            [Expr {
                kind: ExprKind::Column {
                    table: None, name, ..
                },
                ..
            }] => Some((agg, Some(name.clone()))),
            _ => None,
        },
    }
}

/// Combines `live` (the ring's already-finalized single-row aggregate,
/// `None` if the ring held no matching row) with every retained
/// [`EvictedSummary`] overlapping the query's range, using the same
/// additive/comparative semantics `vm::engine::merge_rows` applies
/// across live segments -- `NULL` is each operation's identity, matching
/// that function's own convention (db-core#232).
fn fold_summaries_into(
    agg: AggFunc,
    column: Option<&str>,
    summaries: &[&EvictedSummary],
    live: Option<Value>,
) -> Value {
    let column_summary = |e: &&EvictedSummary| -> Option<crate::storage::stream::ColumnSummary> {
        column.map_or(
            Some(crate::storage::stream::ColumnSummary {
                count: e.summary.rows,
                ..Default::default()
            }),
            |c| e.summary.column(c).copied(),
        )
    };
    match agg {
        AggFunc::Count => {
            let live_n = match live {
                Some(Value::Int(n)) => n,
                _ => 0,
            };
            let total = summaries
                .iter()
                .filter_map(column_summary)
                .map(|c| c.count)
                .fold(live_n, |acc, c| {
                    acc.saturating_add(i64::try_from(c).unwrap_or(i64::MAX))
                });
            Value::Int(total)
        }
        AggFunc::Sum => {
            let live_v = live.and_then(|v| v.as_f64()).unwrap_or(0.0);
            let total = summaries
                .iter()
                .filter_map(column_summary)
                .fold(live_v, |acc, c| acc + c.sum);
            Value::Float(total)
        }
        AggFunc::Min => summaries
            .iter()
            .filter_map(column_summary)
            .filter(|c| c.count > 0)
            .map(|c| c.min)
            .fold(live.and_then(|v| v.as_f64()), |acc, v| {
                Some(acc.map_or(v, |a| a.min(v)))
            })
            .map_or(Value::Null, Value::Float),
        AggFunc::Max => summaries
            .iter()
            .filter_map(column_summary)
            .filter(|c| c.count > 0)
            .map(|c| c.max)
            .fold(live.and_then(|v| v.as_f64()), |acc, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
            .map_or(Value::Null, Value::Float),
        // `single_ungrouped_aggregate` never returns `Avg` (its own
        // finalized sum/count can't be re-merged, see this fn's doc
        // comment) -- a non-panicking fallback here costs nothing and
        // keeps this helper safe to call with any `AggFunc` value.
        AggFunc::Avg => Value::Null,
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
        let scope = resolve_scope(&select);
        let program: Program =
            stream_planner::compile(&select, scope, self.clock.now_ns()).map_err(stream_err)?;
        self.run_compiled(&select, &program)
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code fails fast (db-core#230)"
)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<file-stem>_<line>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#299 MC/DC backfill).

    use super::{Range, ScopeReport, StreamEngine, Value};
    use crate::engine::Engine as _;

    fn temp_log_with(text: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "db-core-engine-stream-mcdc-{}-{n}.log",
            std::process::id()
        ));
        std::fs::write(&p, text).unwrap();
        p
    }

    // stream_357 (`requests`): `is_predefined(col) || self.fields.iter().any(|f| f == col)`
    #[test]
    fn mcdc__stream_214__v1_predefined_alone_is_enough() {
        let p = temp_log_with("<134>Sep 10 08:00:01 h app: msg\n");
        let e = StreamEngine::open(&p).unwrap();
        // "severity" is predefined (true) and not a seen Tier-3 field
        // (false) -- the first leaf alone makes this resolve.
        assert!(e.requests(&["severity".to_string()]).is_ok());
    }

    #[test]
    fn mcdc__stream_214__v2_seen_tier3_field_alone_is_enough() {
        let p = temp_log_with("<134>Sep 10 08:00:01 h nginx[7]: msg\n");
        let e = StreamEngine::open(&p).unwrap();
        // "pid" is not predefined (false) but was seen as a Tier-3 field
        // (true) -- the second leaf alone makes this resolve.
        assert!(e.requests(&["pid".to_string()]).is_ok());
    }

    #[test]
    fn mcdc__stream_214__v3_neither_is_unknown() {
        let p = temp_log_with("<134>Sep 10 08:00:01 h app: msg\n");
        let e = StreamEngine::open(&p).unwrap();
        // Not predefined and never seen as a field -- both leafs false.
        assert!(e.requests(&["not_a_real_column".to_string()]).is_err());
    }

    fn report() -> ScopeReport {
        ScopeReport {
            lines: 0,
            first_ts: None,
            last_ts: None,
            scope_requested: crate::vm::stream::Scope::All,
            scope_available: crate::vm::stream::Scope::All,
            capped: true,
        }
    }

    fn parse(sql: &str) -> crate::parser::ast::Select {
        crate::parser::parse(sql).unwrap()
    }

    // stream_474 (`merge_retained_summaries`):
    // `!select.group_by.is_empty() || select.where_clause.is_some()`
    #[test]
    fn mcdc__stream_480__v1_group_by_alone_short_circuits() {
        let p = temp_log_with("<134>Sep 10 08:00:01 h app: msg\n");
        let e = StreamEngine::open(&p).unwrap();
        let select = parse("SELECT severity, count(*) FROM log GROUP BY severity");
        let rows: Vec<Vec<Value>> = vec![vec![Value::Int(1)]];
        let mut rep = report();
        // GROUP BY present (true), no WHERE (false) -- the first leaf
        // alone short-circuits: `rows` comes back untouched.
        let out = e.merge_retained_summaries(&select, Some(0..1), rows.clone(), &mut rep);
        assert_eq!(out, rows);
    }

    #[test]
    fn mcdc__stream_480__v2_where_alone_short_circuits() {
        let p = temp_log_with("<134>Sep 10 08:00:01 h app: msg\n");
        let e = StreamEngine::open(&p).unwrap();
        let select = parse("SELECT count(*) FROM log WHERE severity >= 0");
        let rows: Vec<Vec<Value>> = vec![vec![Value::Int(1)]];
        let mut rep = report();
        // No GROUP BY (false), WHERE present (true) -- the second leaf
        // alone short-circuits: `rows` comes back untouched.
        let out = e.merge_retained_summaries(&select, Some(0..1), rows.clone(), &mut rep);
        assert_eq!(out, rows);
    }

    #[test]
    fn mcdc__stream_480__v3_neither_lets_the_merge_proceed() {
        let p = temp_log_with("<134>Sep 10 08:00:01 h app: msg\n<134>Sep 10 08:00:02 h app: msg\n");
        let mut e = StreamEngine::open_with_budget(&p, 1).unwrap();
        e.refresh().unwrap(); // no-op; establishes a deterministic ring state
        let select = parse("SELECT count(*) FROM log");
        // Neither GROUP BY nor WHERE (both false) -- the guard does not
        // short-circuit, so the merge path (retained-summary lookup) runs;
        // whether it changes `rows` depends on the ring, not this guard.
        let rows: Vec<Vec<Value>> = vec![vec![Value::Int(1)]];
        let mut rep = report();
        let range: Range<i64> = i64::MIN..i64::MAX;
        let _ = e.merge_retained_summaries(&select, Some(range), rows, &mut rep);
    }
}
