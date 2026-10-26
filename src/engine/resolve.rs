// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Runtime resolver for cross-mode queries (#342, epic #317): given a
//! `SELECT` whose `FROM`/`JOIN` names the stream engine's one table
//! (`log`) and a SQLite lookup table, decide which side is which and
//! drive the join that `codegen::batch::compile_join` already plans and
//! `vm::engine::run_join_segments` already executes (ADR-0019, #312/#314).
//!
//! No new opcodes, no new join operator: this module is pure dispatch --
//! per ADR-0019, the lookup (SQLite) side is always the `HashBuild`/build
//! side. The query may write it as either the `FROM` table or the `JOIN`
//! target (ADR-0021, #371): `resolve_sides` determines which side is
//! which by table identity, not grammar position, and
//! `codegen::batch::compile_join_build_side` assigns build/probe roles
//! accordingly. The one exception: a `LEFT JOIN` with the SQLite table
//! written as `FROM` (e.g. `hosts LEFT JOIN log`) stays rejected as
//! [`ErrorKind::Unsupported`] -- a `LEFT JOIN`'s kept (unmatched-preserved)
//! side is always the probe side, and probe execution is fixed to the
//! stream segments by `run_join_segments`'s calling convention, so that
//! query shape would need `RIGHT JOIN` semantics, which isn't implemented.
//!
//! A sibling seam to [`cross_mode`](super::cross_mode), for the same
//! reason: ADR 0000 §(c) forbids the SQLite side from naming `vm::batch`,
//! so this module -- which names `row::RowEngine`, `stream::StreamEngine`
//! and `vm::batch` together -- cannot live inside any of `engine::row`,
//! `engine::stream` or `engine::column` (`tests/unit/layer_isolation_test.rs`
//! enforces this by source scan).
//!
//! v1 scope, per epic #317 and ADR-0021 (#368): exactly one `JOIN`, the
//! stream table as one side (either grammar position, INNER only when the
//! SQLite table is written as `FROM`), one SQLite lookup table.
//! Key-restricted materialization is still out of scope (ADR-0023).
//!
//! #394 extends this to N SQLite lookup tables via a star join: `log JOIN
//! hosts ON log.host_id = hosts.id JOIN users ON log.user_id = users.id`.
//! This shape is stricter than the single-`JOIN` case -- `log` must be
//! written first (no SQLite-first grammar position, since there's no
//! single "other side" to swap into with N lookup tables) and every join
//! key must be a column of `log` (no lookup table is joined against
//! another lookup table's payload). See [`resolve_multi_sides`] and
//! [`crate::codegen::batch::compile_cross_mode_multi_join`].
//! The stream/SQLite `EXPLAIN`/`explain_opcodes` don't support this shape
//! yet; the Parquet/SQLite ones do (#782).
//!
//! #782 lets a Parquet/SQLite star join name a second *Parquet* table
//! next to the SQLite ones ([`run_column_tables_query`],
//! [`CrossModeEngine::open_column_tables_sqlite`]). Which Parquet table
//! probes is decided per query (`sides::choose_probe`): the largest by
//! footer row count that the query is a star around, whatever order it is
//! written in, so the other is the one read into memory; a `LEFT JOIN`
//! keeps the `FROM` table as the probe. A filter on a built Parquet table
//! prunes the probe's row groups through the same key set a SQLite lookup
//! filter does (`prune`).
//!
//! [`run_stream_stream_query`]/[`explain_stream_stream_plan`] (ADR-0022,
//! #372) join two stream sources instead: both are the same table `log`,
//! so the query is a self-join and must alias both sides
//! (`FROM log AS a JOIN log AS b ON ...`) -- `alias_normalize` rewrites
//! each side's [`TableRefKind`] to its alias before compiling, so
//! `codegen::batch`'s existing table-name-based column classification
//! tells the two sides apart unmodified. A time window (`SINCE`/`UNTIL`)
//! is required, not optional: with no bounded side to hash-build from,
//! the window is what makes the join finite (a query with neither is
//! rejected as `ErrorKind::Unsupported`, distinct from the "not a
//! stream/SQLite pair" rejection above). The `JOIN` target still builds
//! (unchanged from the stream/SQLite convention); the difference is that
//! *both* sides are now windowed to a finite segment set first
//! (`StreamEngine::segments_in_range`) rather than one side being
//! naturally bounded (a SQLite table) and the other left unbounded.

#[cfg(feature = "engine-column")]
mod prune;
#[cfg(feature = "engine-column")]
mod sides;

use std::sync::Arc;

#[cfg(all(feature = "engine-column", feature = "ingest-read"))]
use super::column::{table::TableEngine, SnapshotSegment};
#[cfg(feature = "engine-column")]
use super::column::{BatchEngine, FileSegment};
use super::cross_mode;
use super::row::RowEngine;
use super::stream::StreamEngine;
use super::{
    single_statement, Cell, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, TableInfo,
};
use crate::codegen::batch::{self as planner, split_qualified, PlanError, TableStats};
use crate::parser::ast::{JoinConstraint, Select, TableRefKind};
use crate::parser::ParseError;
use crate::storage::stream::{ColumnRequest, StreamSegment};
use crate::vm::batch::{Batch, ScanSource, Segment as VmSegment, Value};
use crate::vm::engine::{run_join_segments, JoinProgram, MultiJoinProgram};
use crate::vm::stream::Scope;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Cap on rows materialized into one in-memory [`Batch`] for the windowed
/// stream-to-stream join's build side (`materialize_segments`). Without a
/// cap, a wide `SINCE`/`UNTIL` window (or a stream with a large retained
/// ring) has no ceiling on the single allocation `materialize_segments`
/// builds -- unlike the stream/SQLite path, where the SQLite side is
/// already bounded by construction. Matches the order of magnitude of
/// [`crate::storage::stream::segment::SEGMENT_MAX_ROWS`] times a modest
/// number of segments; revisit if a real workload needs more.
const MAX_STREAM_STREAM_BUILD_ROWS: usize = 1_000_000;

fn plan_err(e: PlanError) -> EngineError {
    match e {
        PlanError::Internal(msg) => EngineError::new(
            ErrorKind::Execute,
            format!("planner invariant violated: {msg}"),
        ),
        other => EngineError::new(ErrorKind::Compile, other),
    }
}

fn parse(sql: &str) -> Result<Select, EngineError> {
    let stmt = single_statement(sql)?;
    let mut select = crate::parser::parse(&stmt)
        .map_err(|e: ParseError| EngineError::new(ErrorKind::Parse, e))?;
    StreamEngine::rewrite_severity_literals(&mut select)?;
    Ok(select)
}

/// Which side of the query's `FROM`/`JOIN` is the stream table `log`, and
/// the name of the SQLite lookup table on the other side -- in either
/// grammar position (ADR-0021, #371). Rejects anything that isn't exactly
/// one `JOIN` between `log` and one known SQLite table, and rejects a
/// `LEFT JOIN` written with the SQLite table as `FROM` (see module docs).
fn resolve_sides(
    select: &Select,
    driving: &str,
    lookup: &RowEngine,
    built: &[String],
) -> Result<String, EngineError> {
    let from = select
        .from
        .as_ref()
        .ok_or_else(|| EngineError::new(ErrorKind::Compile, "SELECT without FROM"))?;
    if from.joins.len() > 1 {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            "cross-mode joins support exactly one JOIN clause",
        ));
    }
    let Some(join) = from.joins.first() else {
        return Err(EngineError::new(
            ErrorKind::Compile,
            "not a join: use the stream/row engine directly for a single-table query",
        ));
    };
    let from_name = from.first.name().ok_or_else(|| {
        EngineError::new(
            ErrorKind::Unsupported,
            "a subquery FROM is not supported in a cross-mode join",
        )
    })?;
    let join_name = join.table.name().ok_or_else(|| {
        EngineError::new(
            ErrorKind::Unsupported,
            "a subquery JOIN target is not supported in a cross-mode join",
        )
    })?;

    let lookup_name = if from_name.eq_ignore_ascii_case(driving) {
        join_name
    } else if join_name.eq_ignore_ascii_case(driving) {
        if join.op != crate::parser::ast::JoinOp::Inner {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                format!(
                    "a LEFT JOIN with the SQLite table as FROM would need to \
                     keep all SQLite rows, but the `{driving}` side must always \
                     be the probe side -- write this as `{driving} LEFT JOIN \
                     ...` instead, or use INNER JOIN"
                ),
            ));
        }
        from_name
    } else {
        return Err(EngineError::new(
            ErrorKind::Compile,
            format!(
                "cross-mode join requires `{driving}` joined to a SQLite \
                 lookup table; got `{from_name}` JOIN `{join_name}`"
            ),
        ));
    };

    let known = built.iter().any(|b| b.eq_ignore_ascii_case(lookup_name))
        || lookup
            .tables()?
            .into_iter()
            .any(|t| t.name.eq_ignore_ascii_case(lookup_name));
    if !known {
        return Err(EngineError::new(
            ErrorKind::Compile,
            format!("unknown lookup table: {lookup_name}"),
        ));
    }
    Ok(lookup_name.to_string())
}

/// Resolves a cross-mode *star* join (#394): `log` (the stream engine's
/// table) as the sole `FROM`/driving/probe side, joined to N SQLite lookup
/// tables, one per `JOIN` clause, in `JOIN` order -- e.g. `log JOIN hosts
/// ON log.host_id = hosts.id JOIN users ON log.user_id = users.id`. Unlike
/// [`resolve_sides`] (exactly one `JOIN`, either grammar position), this
/// requires `log` written first: with N lookup sides there's no single
/// "other side" position to swap into, so the driving table must always be
/// `FROM`. Used only when `from.joins.len() > 1`; the single-`JOIN` case
/// keeps using [`resolve_sides`] (which also accepts the SQLite-first
/// grammar `resolve_multi_sides` does not).
fn resolve_multi_sides(
    select: &Select,
    driving: &str,
    lookup: &RowEngine,
    built: &[String],
) -> Result<Vec<String>, EngineError> {
    let from = select
        .from
        .as_ref()
        .ok_or_else(|| EngineError::new(ErrorKind::Compile, "SELECT without FROM"))?;
    let from_name = from.first.name().ok_or_else(|| {
        EngineError::new(
            ErrorKind::Unsupported,
            "a subquery FROM is not supported in a cross-mode join",
        )
    })?;
    if !from_name.eq_ignore_ascii_case(driving) {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            format!(
                "a multi-way cross-mode join requires `{driving}` written \
                 first (`{driving} JOIN a JOIN b ...`); it cannot be the \
                 build side when there is more than one JOIN"
            ),
        ));
    }

    let known: Vec<TableInfo> = lookup.tables()?;
    let mut lookup_tables = Vec::with_capacity(from.joins.len());
    for join in &from.joins {
        let join_name = join.table.name().ok_or_else(|| {
            EngineError::new(
                ErrorKind::Unsupported,
                "a subquery JOIN target is not supported in a cross-mode join",
            )
        })?;
        if join_name.eq_ignore_ascii_case(driving) {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                format!(
                    "a multi-way cross-mode join supports `{driving}` only \
                     once, as the FROM side"
                ),
            ));
        }
        let is_known = built.iter().any(|b| b.eq_ignore_ascii_case(join_name))
            || known.iter().any(|t| t.name.eq_ignore_ascii_case(join_name));
        if !is_known {
            return Err(EngineError::new(
                ErrorKind::Compile,
                format!("unknown lookup table: {join_name}"),
            ));
        }
        if lookup_tables
            .iter()
            .any(|t: &String| t.eq_ignore_ascii_case(join_name))
        {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                format!(
                    "a multi-way cross-mode join cannot join the same \
                     lookup table (`{join_name}`) twice -- aliasing a \
                     repeated JOIN target isn't supported yet"
                ),
            ));
        }
        lookup_tables.push(join_name.to_string());
    }
    Ok(lookup_tables)
}

/// Rekeys a materialized lookup [`Batch`] from unqualified column names
/// (what [`cross_mode::scan_table_as_batch`] resolves against the SQLite
/// schema) to the qualified names `compile_join`'s `LoadColumn` opcodes
/// expect (e.g. `hosts.region`) -- the query's own qualification, not the
/// table's schema, is the batch's key space on the build side.
fn requalify(batch: Batch, raw: &[String], qualified: &[String]) -> Batch {
    let mut out = Batch::new(batch.num_rows);
    let mut columns = batch.columns;
    for (r, q) in raw.iter().zip(qualified) {
        if let Some(values) = columns.remove(r) {
            out.columns.insert(q.clone(), values);
        }
    }
    out
}

/// `select` with its `GROUP BY` names resolved against `columns`, the
/// joined tables' columns: a real column wins over a `SELECT`-list alias
/// (#623), as [`planner::expand_star`] does for a single table.
fn resolve_group_by(select: Select, columns: Vec<String>) -> Result<Select, EngineError> {
    planner::resolve_group_by(&select, &columns).map_err(plan_err)
}

/// One column `*` or `t.*` can stand for.
/// A cross-mode join's `SELECT` with its names resolved
/// ([`planner::resolve_join_columns`]): `log`'s columns plus those of every
/// SQLite table the query names.
fn resolve_cross_mode_group_by(
    select: Select,
    driving: &str,
    driving_columns: Vec<String>,
    lookup: &RowEngine,
) -> Result<Resolved, EngineError> {
    resolve_cross_mode_tables(select, &[(driving.to_string(), driving_columns)], lookup)
}

/// [`resolve_cross_mode_group_by`] where several tables are not SQLite's:
/// `column_tables` are each Parquet table's name and columns (#782).
fn resolve_cross_mode_tables(
    select: Select,
    column_tables: &[(String, Vec<String>)],
    lookup: &RowEngine,
) -> Result<Resolved, EngineError> {
    let known = lookup.tables()?;
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    for name in select
        .from
        .iter()
        .flat_map(|f| std::iter::once(&f.first).chain(f.joins.iter().map(|j| &j.table)))
        .filter_map(|t| t.name())
    {
        let columns = if let Some((_, columns)) = column_tables
            .iter()
            .find(|(table, _)| name.eq_ignore_ascii_case(table))
        {
            columns.clone()
        } else {
            let info = known
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    EngineError::new(ErrorKind::Compile, format!("unknown lookup table: {name}"))
                })?;
            info.columns.iter().map(|c| c.name.clone()).collect()
        };
        tables.push((name.to_string(), columns));
    }
    let planner::ResolvedJoin { select, headers } = planner::resolve_join_columns(&select, &tables)
        .map_err(|e| match e {
            planner::JoinColumnsError::Plan(e) => plan_err(e),
            other => EngineError::new(ErrorKind::Compile, other),
        })?;
    Ok(Resolved { select, headers })
}

/// A cross-mode join's `SELECT` with its names resolved, and the result
/// headers it had before they were.
struct Resolved {
    select: Select,
    headers: Vec<String>,
}

/// Runs a cross-mode `SELECT`: `log` (the stream engine's table) joined to
/// one SQLite lookup table on `lookup`. See the module docs for the
/// exactly-one-JOIN, stream-drives-lookup-builds v1 scope.
pub fn run_query(
    driving: &StreamEngine,
    lookup: &RowEngine,
    sql: &str,
) -> Result<QueryResult, EngineError> {
    let Resolved { select, headers } =
        resolve_cross_mode_group_by(parse(sql)?, super::stream::TABLE, driving.columns(), lookup)?;
    let table = super::stream::TABLE;
    let load = |t: &str, raw: &[String]| cross_mode::scan_table_as_batch(lookup, t, raw);
    if join_count(&select) > 1 {
        let plan = plan_multi_join(&select, table, lookup, &[])?;
        let requests = driving.column_requests(&plan.left_columns)?;
        return execute_multi_join(driving.segments(&requests), &load, headers, &plan);
    }
    let (lookup_table, plan) = plan_join(&select, table, lookup, &[])?;
    let requests = driving.column_requests(&plan.left_columns)?;
    execute_join(
        driving.segments(&requests),
        &load,
        headers,
        &lookup_table,
        &plan,
    )
}

/// A column table of a cross-mode join (#598, #772): a Parquet file or a
/// table directory. The join entry points are written once over it.
#[cfg(feature = "engine-column")]
#[derive(Clone, Copy)]
enum ColumnSource<'e> {
    File(&'e BatchEngine),
    #[cfg(feature = "ingest-read")]
    Dir(&'e TableEngine),
}

/// A probe segment of either kind, so one join runs over both.
#[cfg(feature = "engine-column")]
enum ProbeSegment<'a, 'm> {
    File(FileSegment<'a, 'm>),
    #[cfg(feature = "ingest-read")]
    Dir(SnapshotSegment<'a, 'm>),
}

#[cfg(feature = "engine-column")]
impl ProbeSegment<'_, '_> {
    /// Whether this is a row group of a file, not a live page: what a
    /// join's footer-statistics pruning can keep or drop.
    fn is_row_group(&self) -> bool {
        match self {
            ProbeSegment::File(_) => true,
            #[cfg(feature = "ingest-read")]
            ProbeSegment::Dir(s) => s.is_row_group(),
        }
    }
}

#[cfg(feature = "engine-column")]
impl VmSegment for ProbeSegment<'_, '_> {
    fn load(&self) -> Result<Arc<Batch>, crate::vm::batch::VmError> {
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

    fn load_columns(&self, columns: &[String]) -> Result<Arc<Batch>, crate::vm::batch::VmError> {
        match self {
            ProbeSegment::File(s) => s.load_columns(columns),
            #[cfg(feature = "ingest-read")]
            ProbeSegment::Dir(s) => s.load_columns(columns),
        }
    }

    fn gather_at(
        &self,
        columns: &[String],
        positions: &[u32],
    ) -> Option<Result<Batch, crate::vm::batch::VmError>> {
        match self {
            ProbeSegment::File(s) => s.gather_at(columns, positions),
            #[cfg(feature = "ingest-read")]
            ProbeSegment::Dir(s) => s.gather_at(columns, positions),
        }
    }
}

#[cfg(feature = "engine-column")]
impl ColumnSource<'_> {
    fn parse_join(&self, sql: &str) -> Result<Select, EngineError> {
        match self {
            ColumnSource::File(e) => e.parse_join(sql),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.parse_join(sql),
        }
    }

    fn resolve_timestamp_literals(&self, select: &mut Select) -> Result<(), EngineError> {
        match self {
            ColumnSource::File(e) => e.resolve_timestamp_literals(select),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.resolve_timestamp_literals(select),
        }
    }

    fn table_name(&self) -> &str {
        match self {
            ColumnSource::File(e) => e.table_name(),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.table_name(),
        }
    }

    fn columns(&self) -> Vec<String> {
        match self {
            ColumnSource::File(e) => e.columns(),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.columns(),
        }
    }

    fn path(&self) -> &Path {
        match self {
            ColumnSource::File(e) => e.path(),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.path(),
        }
    }

    fn table_stats(&self) -> TableStats {
        match self {
            ColumnSource::File(e) => e.table_stats(),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.table_stats(),
        }
    }

    /// Row groups of the file or the archive files, live pages left out.
    fn row_groups(&self) -> usize {
        match self {
            ColumnSource::File(e) => e.table_stats().row_groups,
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.archive_row_groups(),
        }
    }

    /// The whole table's `columns` as one batch: a join's build side. The
    /// `bit_pattern` columns read a `UINT_64` above `i64::MAX` as its signed
    /// bit pattern (#600).
    fn read_table(&self, columns: &[String], bit_pattern: &[String]) -> Result<Batch, EngineError> {
        match self {
            ColumnSource::File(e) => e.read_table(columns, bit_pattern),
            #[cfg(feature = "ingest-read")]
            ColumnSource::Dir(e) => e.read_table(columns, bit_pattern),
        }
    }

    fn with_probe_segments<T>(
        &self,
        columns: &[String],
        prune: Option<&crate::parser::ast::Expr>,
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
#[cfg(feature = "engine-column")]
fn file_sources<'a>(driving: &'a BatchEngine, others: &'a [BatchEngine]) -> Vec<ColumnSource<'a>> {
    std::iter::once(driving)
        .chain(others.iter())
        .map(ColumnSource::File)
        .collect()
}

/// The parts of a Parquet/SQLite query every entry point shares: the
/// resolved `SELECT` (the probe table first in its `FROM`), the headers it
/// had before names were resolved, and which of the session's Parquet
/// tables probe and build (indexes into the caller's `tables`).
#[cfg(feature = "engine-column")]
struct ColumnQuery {
    select: Select,
    headers: Vec<String>,
    probe: usize,
    builds: Vec<usize>,
}

/// Parses and resolves `stmt` over the Parquet `tables` and the SQLite
/// `lookup`, and picks the probe: the only Parquet table the query names,
/// or, for several, the largest one it can be a star around (see [`sides`]).
#[cfg(feature = "engine-column")]
fn plan_column_query(
    tables: &[ColumnSource<'_>],
    lookup: &RowEngine,
    stmt: &str,
) -> Result<ColumnQuery, EngineError> {
    let first = tables.first().ok_or_else(|| {
        EngineError::new(
            ErrorKind::Compile,
            "a Parquet/SQLite join needs a Parquet table",
        )
    })?;
    let mut parsed = first.parse_join(stmt)?;
    for table in tables.iter().skip(1) {
        table.resolve_timestamp_literals(&mut parsed)?;
    }
    let columns: Vec<(String, Vec<String>)> = tables
        .iter()
        .map(|t| (t.table_name().to_string(), t.columns()))
        .collect();
    let Resolved { select, headers } = resolve_cross_mode_tables(parsed, &columns, lookup)?;
    let (select, probe) = sides::choose_probe(select, tables)?;
    let named: Vec<&str> = select
        .from
        .iter()
        .flat_map(|f| std::iter::once(&f.first).chain(f.joins.iter().map(|j| &j.table)))
        .filter_map(|t| t.name())
        .collect();
    let builds = tables
        .iter()
        .enumerate()
        .filter(|(i, t)| {
            *i != probe && named.iter().any(|n| n.eq_ignore_ascii_case(t.table_name()))
        })
        .map(|(i, _)| i)
        .collect();
    Ok(ColumnQuery {
        select,
        headers,
        probe,
        builds,
    })
}

/// Materializes the build table `table` (the `raw` column names): the built
/// Parquet file of that name, or else the SQLite table (#782). The columns
/// the query joins on (`keys`) read a `UINT_64` above `i64::MAX` as its signed
/// bit pattern, as the probe's do (#600).
#[cfg(feature = "engine-column")]
fn load_build_table(
    lookup: &RowEngine,
    built: &[ColumnSource<'_>],
    keys: &[String],
    table: &str,
    raw: &[String],
) -> Result<Batch, EngineError> {
    match built
        .iter()
        .find(|b| b.table_name().eq_ignore_ascii_case(table))
    {
        Some(engine) => {
            let bit_pattern: Vec<String> = raw
                .iter()
                .filter(|c| {
                    keys.iter()
                        .any(|k| split_qualified(k).1.eq_ignore_ascii_case(c))
                })
                .cloned()
                .collect();
            engine.read_table(raw, &bit_pattern)
        }
        None => cross_mode::scan_table_as_batch(lookup, table, raw),
    }
}

/// Runs a cross-mode `SELECT` whose probe side is a Parquet file: the
/// file's table (named after its stem) joined to SQLite lookup tables.
/// Same shapes as [`run_query`]: the SQLite side always builds, `JOIN` is
/// inner, and a `LEFT JOIN` must keep the Parquet table on the left.
#[cfg(feature = "engine-column")]
pub fn run_column_query(
    driving: &BatchEngine,
    lookup: &RowEngine,
    sql: &str,
) -> Result<QueryResult, EngineError> {
    run_column_tables_query(&[driving], lookup, sql)
}

/// [`run_column_query`] over several Parquet files (#782): a star join of
/// two Parquet tables and SQLite lookup tables, e.g. `events JOIN
/// transactions ON ... JOIN channels ON ...`. The Parquet table with the
/// most rows (by its footer) probes, whatever order the query writes them
/// in; the other is read into memory next to the SQLite tables. A filter
/// on a built table prunes the probe's row groups, as a SQLite one does.
#[cfg(feature = "engine-column")]
pub fn run_column_tables_query(
    tables: &[&BatchEngine],
    lookup: &RowEngine,
    sql: &str,
) -> Result<QueryResult, EngineError> {
    let sources: Vec<ColumnSource<'_>> = tables.iter().copied().map(ColumnSource::File).collect();
    run_sources_query(&sources, lookup, sql)
}

/// [`run_column_tables_query`] over files and table directories alike (#772).
#[cfg(feature = "engine-column")]
fn run_sources_query(
    tables: &[ColumnSource<'_>],
    lookup: &RowEngine,
    sql: &str,
) -> Result<QueryResult, EngineError> {
    let stmt = single_statement(sql)?;
    let ColumnQuery {
        select,
        headers,
        probe,
        builds,
    } = plan_column_query(tables, lookup, &stmt)?;
    let driving = tables.get(probe).ok_or_else(|| {
        EngineError::new(
            ErrorKind::Compile,
            "the chosen probe table is not in the session",
        )
    })?;
    let built: Vec<ColumnSource<'_>> = builds
        .iter()
        .filter_map(|&i| tables.get(i).copied())
        .collect();
    let built_names: Vec<String> = built.iter().map(|b| b.table_name().to_string()).collect();
    let keys = join_named_columns(&select);
    let load = |t: &str, raw: &[String]| load_build_table(lookup, &built, &keys, t, raw);
    let table = driving.table_name();
    let pruning = prune::probe_pruning(&select, &load)?;
    // A build filter that matches no row leaves an inner join empty:
    // probe nothing rather than the whole file.
    if join_count(&select) > 1 {
        let plan = plan_multi_join(&select, table, lookup, &built_names)?;
        return driving.with_probe_segments(
            &plan.left_columns,
            pruning.predicate.as_ref(),
            &join_key_columns(&select, &plan.left_columns),
            |mut segments| {
                if pruning.empty {
                    segments.clear();
                }
                execute_multi_join(segments, &load, headers, &plan)
            },
        );
    }
    let (lookup_table, plan) = plan_join(&select, table, lookup, &built_names)?;
    driving.with_probe_segments(
        &plan.left_columns,
        pruning.predicate.as_ref(),
        &join_key_columns(&select, &plan.left_columns),
        |mut segments| {
            if pruning.empty {
                segments.clear();
            }
            execute_join(segments, &load, headers, &lookup_table, &plan)
        },
    )
}

/// What `EXPLAIN`'s `SCAN` line reports for a SQLite lookup table (#789):
/// its real row count, and no row groups -- a SQLite table has none, which
/// the renderer drops from the line for a `sqlite` source.
fn sqlite_scan_stats(
    lookup: &RowEngine,
    table: &str,
    lookup_file: &Path,
) -> Result<TableStats, EngineError> {
    Ok(TableStats {
        row_groups: 0,
        rows: lookup.table_row_count(table)?,
        source: Some(format!("sqlite {}", lookup_file.display())),
    })
}

/// `EXPLAIN QUERY PLAN` for a Parquet/SQLite join (#770), labelling each
/// side by mode and file as [`explain_plan`] does for stream/SQLite, plus
/// a row for the row-group pruning the `WHERE` implies on the probe file
/// (see [`prune`]). A star join (#782) shows every joined table, and, when
/// it names several Parquet tables, which one probes.
#[cfg(feature = "engine-column")]
fn explain_column_plan(
    tables: &[ColumnSource<'_>],
    lookup: &RowEngine,
    lookup_file: &Path,
    sql: &str,
) -> Result<Vec<PlanRow>, EngineError> {
    let stmt = single_statement(sql)?;
    let ColumnQuery {
        select,
        probe,
        builds,
        ..
    } = plan_column_query(tables, lookup, &stmt)?;
    let driving = tables.get(probe).ok_or_else(|| {
        EngineError::new(
            ErrorKind::Compile,
            "the chosen probe table is not in the session",
        )
    })?;
    let built: Vec<ColumnSource<'_>> = builds
        .iter()
        .filter_map(|&i| tables.get(i).copied())
        .collect();
    let built_names: Vec<String> = built.iter().map(|b| b.table_name().to_string()).collect();
    let table = driving.table_name();
    let (build_tables, left_columns) = if join_count(&select) > 1 {
        let plan = plan_multi_join(&select, table, lookup, &built_names)?;
        let names = plan.builds.iter().map(|b| b.table_name.clone()).collect();
        (names, plan.left_columns)
    } else {
        let (lookup_table, plan) = plan_join(&select, table, lookup, &built_names)?;
        (vec![lookup_table], plan.left_columns)
    };

    // The `SCAN` stats of every table the query names, by name.
    let stats_of = |name: &str| -> Result<TableStats, EngineError> {
        if name.eq_ignore_ascii_case(table) {
            return Ok(TableStats {
                source: Some(format!("batch {}", driving.path().display())),
                ..driving.table_stats()
            });
        }
        if let Some(engine) = built
            .iter()
            .find(|b| b.table_name().eq_ignore_ascii_case(name))
        {
            return Ok(TableStats {
                source: Some(format!("batch {}", engine.path().display())),
                ..engine.table_stats()
            });
        }
        if build_tables.iter().any(|b| b.eq_ignore_ascii_case(name)) {
            return sqlite_scan_stats(lookup, name, lookup_file);
        }
        Ok(TableStats {
            row_groups: 0,
            rows: 0,
            source: None,
        })
    };
    let mut stats = Vec::new();
    for name in std::iter::once(table).chain(build_tables.iter().map(String::as_str)) {
        stats.push((name.to_string(), stats_of(name)?));
    }
    let nodes = planner::explain(&select, |name| {
        stats
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map_or_else(
                || TableStats {
                    row_groups: 0,
                    rows: 0,
                    source: None,
                },
                |(_, s)| s.clone(),
            )
    })
    .map_err(plan_err)?;
    let mut rows: Vec<PlanRow> = nodes
        .into_iter()
        .map(|n| PlanRow {
            id: i64::from(n.id),
            parent: i64::from(n.parent),
            detail: n.detail,
        })
        .collect();
    let next_id = |rows: &[PlanRow]| {
        rows.iter()
            .map(|r| r.id)
            .max()
            .map_or(0, |m| m.saturating_add(1))
    };

    // With several Parquet tables, say which one probes and which build.
    if !builds.is_empty() {
        let parts: Vec<String> = std::iter::once(("PROBE", table))
            .chain(built.iter().map(|b| ("BUILD", b.table_name())))
            .filter_map(|(role, name)| {
                stats
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, s)| format!("{role} {name} (~{} rows)", s.rows))
            })
            .collect();
        let id = next_id(&rows);
        rows.push(PlanRow {
            id,
            parent: 0,
            detail: format!("JOIN SIDES: {}", parts.join(", ")),
        });
    }

    let load = |t: &str, raw: &[String]| {
        load_build_table(lookup, &built, &join_named_columns(&select), t, raw)
    };
    let pruning = prune::probe_pruning(&select, &load)?;
    let kept = match (pruning.empty, pruning.predicate.as_ref()) {
        (true, _) => Some(0),
        (false, Some(predicate)) => Some(driving.with_probe_segments(
            &left_columns,
            Some(predicate),
            &[],
            |segments| Ok(segments.iter().filter(|s| s.is_row_group()).count()),
        )?),
        (false, None) => None,
    };
    if let Some(kept) = kept {
        let id = next_id(&rows);
        rows.push(PlanRow {
            id,
            parent: 0,
            detail: format!(
                "PRUNE ROW GROUPS OF {table} BY {} FILTER ({kept} of {} kept)",
                build_tables.join(", "),
                driving.row_groups()
            ),
        });
    }
    Ok(rows)
}

/// `EXPLAIN` (bare opcode listing) for a Parquet/SQLite join (#770),
/// mirroring [`explain_opcodes_query`] with the plan execution compiles:
/// each SQLite build side is `"row"`, the Parquet probe, any built Parquet
/// table (#782) and the body `"batch"`.
#[cfg(feature = "engine-column")]
fn explain_column_opcodes(
    tables: &[ColumnSource<'_>],
    lookup: &RowEngine,
    sql: &str,
) -> Result<Vec<(&'static str, planner::OpcodeSection)>, EngineError> {
    let stmt = single_statement(sql)?;
    let ColumnQuery {
        select,
        probe,
        builds,
        ..
    } = plan_column_query(tables, lookup, &stmt)?;
    let driving = tables.get(probe).ok_or_else(|| {
        EngineError::new(
            ErrorKind::Compile,
            "the chosen probe table is not in the session",
        )
    })?;
    let built: Vec<ColumnSource<'_>> = builds
        .iter()
        .filter_map(|&i| tables.get(i).copied())
        .collect();
    let built_names: Vec<String> = built.iter().map(|b| b.table_name().to_string()).collect();
    let table = driving.table_name();
    // (table, build program) per joined table, then the probe and the body.
    let (build_sections, probe_program, body_program) = if join_count(&select) > 1 {
        let plan = plan_multi_join(&select, table, lookup, &built_names)?;
        let sections: Vec<(String, Vec<planner::OpcodeRow>)> = plan
            .builds
            .iter()
            .map(|b| (b.table_name.clone(), planner::render_program(&b.build)))
            .collect();
        (
            sections,
            planner::render_program(&plan.probe),
            planner::render_program(&plan.body),
        )
    } else {
        let (name, plan) = plan_join(&select, table, lookup, &built_names)?;
        (
            vec![(name, planner::render_program(&plan.build))],
            planner::render_program(&plan.probe),
            planner::render_program(&plan.body),
        )
    };
    let mut sections: Vec<(&'static str, planner::OpcodeSection)> = Vec::new();
    let only_one = build_sections.len() == 1;
    for (name, rows) in build_sections {
        let (mode, label) = if built
            .iter()
            .any(|b| b.table_name().eq_ignore_ascii_case(&name))
        {
            ("batch", format!("JOIN build (parquet {name})"))
        } else if only_one {
            ("row", "JOIN build (sqlite)".to_string())
        } else {
            ("row", format!("JOIN build (sqlite {name})"))
        };
        sections.push((mode, planner::OpcodeSection { label, rows }));
    }
    sections.push((
        "batch",
        planner::OpcodeSection {
            label: format!("JOIN probe (parquet {table})"),
            rows: probe_program,
        },
    ));
    sections.push((
        "batch",
        planner::OpcodeSection {
            label: "JOIN body".to_string(),
            rows: body_program,
        },
    ));
    Ok(sections)
}

/// The probe-side `columns` named in a `JOIN`'s `ON`/`USING`: the columns
/// a `UINT_64` above `i64::MAX` may read as its signed bit pattern, since a
/// join only tests them for equality against a SQLite key stored the same
/// way (#600).
fn join_key_columns(select: &Select, columns: &[String]) -> Vec<String> {
    let named = join_named_columns(select);
    columns
        .iter()
        .filter(|c| {
            named
                .iter()
                .any(|n| n.eq_ignore_ascii_case(c) || n.eq_ignore_ascii_case(split_qualified(c).1))
        })
        .cloned()
        .collect()
}

/// The columns named in any `JOIN`'s `ON` or `USING`.
fn join_named_columns(select: &Select) -> Vec<String> {
    let mut named: Vec<String> = Vec::new();
    for join in select.from.iter().flat_map(|f| &f.joins) {
        match &join.constraint {
            Some(JoinConstraint::On(expr)) => {
                named.extend(planner::bool_expr_columns(expr));
            }
            Some(JoinConstraint::Using(names)) => named.extend(names.iter().cloned()),
            None => {}
        }
    }
    named
}

fn join_count(select: &Select) -> usize {
    select.from.as_ref().map_or(0, |f| f.joins.len())
}

/// Resolves which table builds and compiles the single-`JOIN` plan, the
/// part of [`run_query`] the stream and column probe sides share.
fn plan_join(
    select: &Select,
    driving: &str,
    lookup: &RowEngine,
    built: &[String],
) -> Result<(String, JoinProgram), EngineError> {
    let lookup_table = resolve_sides(select, driving, lookup, built)?;
    let plan =
        planner::compile_join_build_side(select, &lookup_table, planner::BuildSourceKind::RowTable)
            .map_err(plan_err)?;
    Ok((lookup_table, plan))
}

/// Builds the lookup side from SQLite and probes it with `segments`.
fn execute_join<S: VmSegment>(
    segments: Vec<S>,
    load: &impl Fn(&str, &[String]) -> Result<Batch, EngineError>,
    headers: Vec<String>,
    lookup_table: &str,
    plan: &JoinProgram,
) -> Result<QueryResult, EngineError> {
    let raw_lookup_cols: Vec<String> = plan
        .right_columns
        .iter()
        .map(|n| split_qualified(n).1.to_string())
        .collect();
    // Resolves `ScanSource::RowTable` by scanning `lookup_table` through
    // `engine::row` (ADR 0024, #385) -- the same materialization
    // `cross_mode::scan_table_as_batch` always did, now reached via
    // `run_join_segments`'s opcode-named build side instead of being
    // materialized before that call. Requalifies raw SQLite column names to
    // the query's qualified names (`requalify`), since only the resolver
    // sees the raw scan. A closure, not a hand-written `struct
    // Resolver<'a>` -- the qualified subset forbids the named lifetime
    // that would need (`make check-mvl-limit`).
    let resolver = |_source: &ScanSource| -> crate::vm::batch::Result<Batch> {
        let batch = load(lookup_table, &raw_lookup_cols).map_err(|e| {
            crate::vm::batch::VmError::SegmentLoad {
                reason: e.to_string(),
            }
        })?;
        Ok(requalify(batch, &raw_lookup_cols, &plan.right_columns))
    };
    let source = ScanSource::RowTable {
        table: Cow::Owned(lookup_table.to_string()),
        columns: raw_lookup_cols
            .iter()
            .map(|c| Cow::Owned(c.clone()))
            .collect(),
    };

    let rows = run_join_segments(segments, source, plan, &resolver)
        .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
    Ok(query_result(headers, rows.into_rows()))
}

fn query_result(columns: Vec<String>, rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult {
        columns,
        rows: rows
            .into_iter()
            .map(|r| r.into_iter().map(Cell::from).collect())
            .collect(),
        scope_report: None,
    }
}

/// [`plan_join`] for a star join (#394): `driving` joined to N SQLite
/// lookup tables. See [`resolve_multi_sides`] for the accepted query shape
/// and [`crate::codegen::batch::compile_cross_mode_multi_join`] for how
/// the star-join plan is built.
fn plan_multi_join(
    select: &Select,
    driving: &str,
    lookup: &RowEngine,
    built: &[String],
) -> Result<MultiJoinProgram, EngineError> {
    let lookup_tables = resolve_multi_sides(select, driving, lookup, built)?;
    planner::compile_cross_mode_multi_join(select, &lookup_tables).map_err(plan_err)
}

/// [`execute_join`] for a star join.
fn execute_multi_join<S: VmSegment>(
    segments: Vec<S>,
    load: &impl Fn(&str, &[String]) -> Result<Batch, EngineError>,
    headers: Vec<String>,
    plan: &MultiJoinProgram,
) -> Result<QueryResult, EngineError> {
    // Every build's raw (unqualified) SQLite column names, in the same
    // `plan.builds` order the resolver below is called for (see
    // `execute_join`'s matching comment for why this is a closure).
    let raw_cols: Vec<Vec<String>> = plan
        .builds
        .iter()
        .map(|b| {
            b.right_columns
                .iter()
                .map(|n| split_qualified(n).1.to_string())
                .collect()
        })
        .collect();
    let resolver = |source: &ScanSource| -> crate::vm::batch::Result<Batch> {
        let ScanSource::RowTable { table, .. } = source else {
            return Err(crate::vm::batch::VmError::MalformedProgram {
                opcode: "ScanSource",
                reason: format!("expected a RowTable source, got {source:?}"),
            });
        };
        let (build, raw) = plan
            .builds
            .iter()
            .zip(&raw_cols)
            .find(|(b, _)| b.table_name == table.as_ref())
            .ok_or_else(|| crate::vm::batch::VmError::MalformedProgram {
                opcode: "ScanSource",
                reason: format!("no planned build side for table `{table}`"),
            })?;
        let batch =
            load(&build.table_name, raw).map_err(|e| crate::vm::batch::VmError::SegmentLoad {
                reason: e.to_string(),
            })?;
        Ok(requalify(batch, raw, &build.right_columns))
    };
    let sources: Vec<ScanSource> = plan
        .builds
        .iter()
        .zip(&raw_cols)
        .map(|(b, raw)| ScanSource::RowTable {
            table: Cow::Owned(b.table_name.clone()),
            columns: raw.iter().map(|c| Cow::Owned(c.clone())).collect(),
        })
        .collect();

    let rows = crate::vm::engine::run_multi_join_segments(segments, sources, plan, &resolver)
        .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
    Ok(query_result(headers, rows.into_rows()))
}

/// `EXPLAIN QUERY PLAN` for a cross-mode `SELECT`, labelling each side of
/// the join by execution mode and file (#315): `log` gets `driving.path()`
/// under `stream`, the lookup table gets `lookup_file`'s display path
/// under `sqlite`.
pub fn explain_plan(
    driving: &StreamEngine,
    lookup: &RowEngine,
    lookup_file: &std::path::Path,
    sql: &str,
) -> Result<Vec<PlanRow>, EngineError> {
    let select = parse(sql)?;
    if select.from.as_ref().is_some_and(|f| f.joins.len() > 1) {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            "EXPLAIN QUERY PLAN for a multi-way cross-mode join (#394) is \
             not yet supported; run the query directly instead",
        ));
    }
    let lookup_table = resolve_sides(&select, super::stream::TABLE, lookup, &[])?;

    let stream_stats = TableStats {
        source: Some(format!("stream {}", driving.path().display())),
        ..driving.table_stats()
    };
    let lookup_stats = sqlite_scan_stats(lookup, &lookup_table, lookup_file)?;

    let nodes = planner::explain(&select, |table| {
        if table.eq_ignore_ascii_case(super::stream::TABLE) {
            stream_stats.clone()
        } else if table.eq_ignore_ascii_case(&lookup_table) {
            lookup_stats.clone()
        } else {
            TableStats {
                row_groups: 0,
                rows: 0,
                source: None,
            }
        }
    })
    .map_err(plan_err)?;

    Ok(nodes
        .into_iter()
        .map(|n| PlanRow {
            id: i64::from(n.id),
            parent: i64::from(n.parent),
            detail: n.detail,
        })
        .collect())
}

/// Validates a windowed stream-to-stream self-join and returns
/// `(left_alias, right_alias)`: exactly one `JOIN`, both sides literally
/// the stream table `log`, both aliased (required -- a self-join can't
/// otherwise disambiguate `a.col` from `b.col`) with distinct aliases,
/// and a `SINCE`/`UNTIL` window on the query. See the module docs.
fn resolve_stream_stream_sides(select: &Select) -> Result<(String, String), EngineError> {
    let from = select
        .from
        .as_ref()
        .ok_or_else(|| EngineError::new(ErrorKind::Compile, "SELECT without FROM"))?;
    if from.joins.len() > 1 {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            "cross-mode joins support exactly one JOIN clause",
        ));
    }
    let Some(join) = from.joins.first() else {
        return Err(EngineError::new(
            ErrorKind::Compile,
            "not a join: use the stream engine directly for a single-table query",
        ));
    };
    let from_name = from.first.name().ok_or_else(|| {
        EngineError::new(
            ErrorKind::Unsupported,
            "a subquery FROM is not supported in a stream-to-stream join",
        )
    })?;
    let join_name = join.table.name().ok_or_else(|| {
        EngineError::new(
            ErrorKind::Unsupported,
            "a subquery JOIN target is not supported in a stream-to-stream join",
        )
    })?;

    if !from_name.eq_ignore_ascii_case(super::stream::TABLE)
        || !join_name.eq_ignore_ascii_case(super::stream::TABLE)
    {
        return Err(EngineError::new(
            ErrorKind::Compile,
            format!(
                "a stream-to-stream join requires both sides to be `log` \
                 (the stream table); got `{from_name}` JOIN `{join_name}`"
            ),
        ));
    }

    let left_alias = from.first.alias.as_deref().ok_or_else(|| {
        EngineError::new(
            ErrorKind::Unsupported,
            "a stream-to-stream self-join requires an alias on the FROM \
             side, e.g. `FROM log AS a`",
        )
    })?;
    let right_alias = join.table.alias.as_deref().ok_or_else(|| {
        EngineError::new(
            ErrorKind::Unsupported,
            "a stream-to-stream self-join requires an alias on the JOIN \
             side, e.g. `JOIN log AS b`",
        )
    })?;
    if left_alias.eq_ignore_ascii_case(right_alias) {
        return Err(EngineError::new(
            ErrorKind::Compile,
            format!("the FROM and JOIN aliases must differ, got `{left_alias}` twice"),
        ));
    }

    match select.scope.as_ref() {
        None => {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                "a stream-to-stream join requires a SINCE/UNTIL window: with no \
                 SQLite side to bound the query, the window is what makes the \
                 join finite (ADR-0022)",
            ));
        }
        Some(_) if !matches!(super::stream::resolve_scope(select), Scope::Time(_)) => {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                "a stream-to-stream join requires a time-based SINCE/UNTIL window \
                 (e.g. `SINCE 1 hour`); `LINES`/`BYTES` scopes don't bound either \
                 side by time and can't make the join finite (ADR-0022)",
            ));
        }
        Some(_) => {}
    }

    Ok((left_alias.to_string(), right_alias.to_string()))
}

/// Rewrites both sides' [`TableRefKind`] from the literal table name
/// (`log`, on both sides of a self-join) to their alias, and strips
/// `scope` (mirroring `codegen::stream`'s own stripping of `SINCE`/`UNTIL`
/// before delegating to the batch planner) -- the window was already
/// consumed by [`resolve_stream_stream_sides`]/[`resolve_scope`] before
/// this runs. After this, `codegen::batch`'s existing table-name-based
/// column classification (`a.col` vs `b.col`) works unmodified.
fn alias_normalize(mut select: Select, left_alias: &str, right_alias: &str) -> Select {
    if let Some(from) = &mut select.from {
        from.first.kind = TableRefKind::Name(left_alias.to_string());
        if let Some(join) = from.joins.first_mut() {
            join.table.kind = TableRefKind::Name(right_alias.to_string());
        }
    }
    select.scope = None;
    select
}

/// Loads every segment in `segments` and concatenates them, column-wise,
/// into one [`Batch`] -- the windowed stream-to-stream join's build side
/// needs one materialized batch (`run_join_segments`'s `right: &Batch`),
/// same shape as [`cross_mode::scan_table_as_batch`] produces for a
/// SQLite lookup table, just assembled from segments instead of a table
/// scan. `columns` (the same requests `segments` were read with) is
/// always used for the output's column set -- a window matching zero
/// segments must still produce a zero-*row*, but not zero-*column*,
/// `Batch`: `codegen::batch`'s `LoadColumn` addresses the build side by
/// name regardless of whether any row was found, so every requested
/// column must exist (empty) rather than only whichever columns the
/// first loaded segment happened to carry.
fn materialize_segments(
    segments: &[StreamSegment],
    columns: &[ColumnRequest],
) -> Result<Batch, EngineError> {
    let loaded: Vec<Arc<Batch>> = segments
        .iter()
        .map(|s| {
            s.load()
                .map_err(|e| EngineError::new(ErrorKind::Execute, e))
        })
        .collect::<Result<_, _>>()?;

    let num_rows: usize = loaded.iter().map(|b| b.num_rows).sum();
    if num_rows > MAX_STREAM_STREAM_BUILD_ROWS {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            format!(
                "stream-to-stream join build side has {num_rows} rows in this \
                 window, over the {MAX_STREAM_STREAM_BUILD_ROWS}-row limit; \
                 narrow the SINCE/UNTIL window"
            ),
        ));
    }
    let mut out = Batch::new(num_rows);
    for request in columns {
        let mut values: Vec<Value> = Vec::with_capacity(num_rows);
        for batch in &loaded {
            if let Some(col) = batch.columns.get(&request.key) {
                values.extend(col.iter().cloned());
            }
        }
        out.columns.insert(request.key.clone(), Arc::new(values));
    }
    Ok(out)
}

/// Runs a windowed stream-to-stream self-join: `log AS <left_alias>`
/// joined to `log AS <right_alias>`, `left` and `right` each backing one
/// alias (positionally: `left` is the `FROM` side, `right` is the `JOIN`
/// side). See the module docs for the required-window, alias-disambiguated
/// self-join shape (ADR-0022, #372).
pub fn run_stream_stream_query(
    left: &StreamEngine,
    right: &StreamEngine,
    sql: &str,
) -> Result<QueryResult, EngineError> {
    let select = resolve_group_by(parse(sql)?, left.columns())?;
    let (left_alias, right_alias) = resolve_stream_stream_sides(&select)?;
    let scope = super::stream::resolve_scope(&select);
    let normalized = alias_normalize(select, &left_alias, &right_alias);
    let plan =
        planner::compile_join(&normalized, planner::BuildSourceKind::Stream).map_err(plan_err)?;

    let left_cols = left.column_requests(&plan.left_columns)?;
    let left_segments = left.segments_in_range(&left_cols, scope);

    let right_cols = right.column_requests(&plan.right_columns)?;
    let right_segments = right.segments_in_range(&right_cols, scope);
    // Resolves `ScanSource::Stream` by materializing `right_segments` (ADR
    // 0022's windowed build side, ADR 0024's opcode-named routing, #385) --
    // the same materialization `materialize_segments` always did. A
    // closure, not a hand-written `struct Resolver<'a>` -- see the
    // matching comment in `run_query`, above.
    let resolver = |_source: &ScanSource| -> crate::vm::batch::Result<Batch> {
        materialize_segments(&right_segments, &right_cols).map_err(|e| {
            crate::vm::batch::VmError::SegmentLoad {
                reason: e.to_string(),
            }
        })
    };
    let source = ScanSource::Stream {
        handle: 0,
        columns: right_cols
            .iter()
            .map(|c| Cow::Owned(c.key.clone()))
            .collect(),
        scope: Some(scope),
    };

    let rows = run_join_segments(left_segments, source, &plan, &resolver)
        .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
    let rows = rows.into_rows();

    Ok(QueryResult {
        columns: planner::output_column_names(&normalized),
        rows: rows
            .into_iter()
            .map(|r| r.into_iter().map(Cell::from).collect())
            .collect(),
        scope_report: None,
    })
}

/// `EXPLAIN QUERY PLAN` for a windowed stream-to-stream join, labelling
/// each side by its alias and file path (mirroring [`explain_plan`]'s
/// stream/SQLite labelling).
pub fn explain_stream_stream_plan(
    left: &StreamEngine,
    right: &StreamEngine,
    sql: &str,
) -> Result<Vec<PlanRow>, EngineError> {
    let select = parse(sql)?;
    let (left_alias, right_alias) = resolve_stream_stream_sides(&select)?;
    let normalized = alias_normalize(select, &left_alias, &right_alias);

    let left_stats = TableStats {
        source: Some(format!("stream {}", left.path().display())),
        ..left.table_stats()
    };
    let right_stats = TableStats {
        source: Some(format!("stream {}", right.path().display())),
        ..right.table_stats()
    };

    let nodes = planner::explain(&normalized, |table| {
        if table.eq_ignore_ascii_case(&left_alias) {
            left_stats.clone()
        } else if table.eq_ignore_ascii_case(&right_alias) {
            right_stats.clone()
        } else {
            TableStats {
                row_groups: 0,
                rows: 0,
                source: None,
            }
        }
    })
    .map_err(plan_err)?;

    Ok(nodes
        .into_iter()
        .map(|n| PlanRow {
            id: i64::from(n.id),
            parent: i64::from(n.parent),
            detail: n.detail,
        })
        .collect())
}

/// `EXPLAIN` (bare opcode listing) for a stream/SQLite cross-mode query
/// (ADR 0024, #382/#387): the same `compile_join` plan [`run_query`]
/// executes, rendered via `codegen::batch`'s existing opcode renderer --
/// the same mechanism a single-engine batch query's `EXPLAIN` already
/// uses, not a divergent cross-mode-only format.
/// `(lane, section)` pairs: `lane` is `"row"`/`"stream"`/`"batch"` (ADR
/// 0024, #382/#388), naming which physical engine executes that section's
/// opcodes -- a stream/SQLite join's build side is a SQLite scan (`"row"`),
/// its probe side the driving tail (`"stream"`), its body always `vm::batch`
/// (`"batch"`) regardless of either side's origin.
fn explain_opcodes_query(
    driving: &StreamEngine,
    lookup: &RowEngine,
    sql: &str,
) -> Result<Vec<(&'static str, planner::OpcodeSection)>, EngineError> {
    let select =
        resolve_cross_mode_group_by(parse(sql)?, super::stream::TABLE, driving.columns(), lookup)?
            .select;
    if select.from.as_ref().is_some_and(|f| f.joins.len() > 1) {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            "EXPLAIN for a multi-way cross-mode join (#394) is not yet \
             supported; run the query directly instead",
        ));
    }
    resolve_sides(&select, super::stream::TABLE, lookup, &[])?;
    let plan =
        planner::compile_join(&select, planner::BuildSourceKind::RowTable).map_err(plan_err)?;
    Ok(vec![
        (
            "row",
            planner::OpcodeSection {
                label: "JOIN build (sqlite)".to_string(),
                rows: planner::render_program(&plan.build),
            },
        ),
        (
            "stream",
            planner::OpcodeSection {
                label: "JOIN probe (stream log)".to_string(),
                rows: planner::render_program(&plan.probe),
            },
        ),
        (
            "batch",
            planner::OpcodeSection {
                label: "JOIN body".to_string(),
                rows: planner::render_program(&plan.body),
            },
        ),
    ])
}

/// `EXPLAIN` (bare opcode listing) for a windowed stream-to-stream join
/// (ADR-0022, ADR 0024, #382/#388), mirroring [`explain_opcodes_query`].
/// Both sides are `"stream"` here (ADR-0022 has no SQLite side); the body
/// is still `"batch"`.
fn explain_opcodes_stream_stream_query(
    left: &StreamEngine,
    sql: &str,
) -> Result<Vec<(&'static str, planner::OpcodeSection)>, EngineError> {
    let select = resolve_group_by(parse(sql)?, left.columns())?;
    let (left_alias, right_alias) = resolve_stream_stream_sides(&select)?;
    let normalized = alias_normalize(select, &left_alias, &right_alias);
    let plan =
        planner::compile_join(&normalized, planner::BuildSourceKind::Stream).map_err(plan_err)?;
    Ok(vec![
        (
            "stream",
            planner::OpcodeSection {
                label: format!("JOIN build (stream {right_alias})"),
                rows: planner::render_program(&plan.build),
            },
        ),
        (
            "stream",
            planner::OpcodeSection {
                label: format!("JOIN probe (stream {left_alias})"),
                rows: planner::render_program(&plan.probe),
            },
        ),
        (
            "batch",
            planner::OpcodeSection {
                label: "JOIN body".to_string(),
                rows: planner::render_program(&plan.body),
            },
        ),
    ])
}

/// `codegen::batch::OpcodeSection`/`OpcodeRow` (this crate's planner-level
/// opcode listing) -> `engine::OpcodeSection`/`OpcodeRow` (the
/// engine-facing shape every [`Engine::explain_opcodes`] impl returns) --
/// the same conversion `engine::column::BatchEngine::explain_opcodes`
/// already does, passing a planner row's `comment` and `is_finalize`
/// through untouched, plus the `lane` each `(lane, section)` pair names.
fn to_engine_opcode_sections(
    sections: Vec<(&'static str, planner::OpcodeSection)>,
) -> Vec<OpcodeSection> {
    sections
        .into_iter()
        .map(|(lane, s)| OpcodeSection {
            label: s.label,
            lane,
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
        .collect()
}

/// A cross-mode engine spanning two files at once (ADR-0019/ADR-0022,
/// #382/#387): either a stream `FROM` joined to one SQLite lookup table,
/// or two stream engines joined via a windowed self-join. Routing only --
/// every variant dispatches to the free-function query logic this module
/// already had (`run_query`/`run_stream_stream_query`/`explain_plan`/
/// `explain_stream_stream_plan`), which in turn compiles through
/// `codegen::batch::compile_join` and executes through
/// `vm::engine::run_join_segments` exactly as before #385/#386 -- this
/// struct adds no new query behavior, only a real [`Engine`] impl so
/// cross-mode `EXPLAIN`/`explain_opcodes` has a home (closing the gap
/// db-studio#54's F3 view worked around with a placeholder).
///
/// [`Engine::open`] takes one `path`, which cannot express the two files a
/// cross-mode query needs -- construct via
/// [`CrossModeEngine::open_stream_sqlite`]/
/// [`CrossModeEngine::open_stream_stream`] instead; `open` itself always
/// returns a typed [`ErrorKind::Unsupported`] error (never a panic or a
/// silently wrong single-file open) so implementing the trait doesn't
/// imply `open` is a usable construction path for this engine.
#[derive(Debug)]
pub enum CrossModeEngine {
    /// A stream `FROM` (`log`) joined to one SQLite lookup table (ADR-0019).
    StreamSqlite {
        /// The stream engine's `FROM` side.
        driving: StreamEngine,
        /// The SQLite `JOIN`-target lookup engine.
        lookup: RowEngine,
        /// `lookup`'s file path, for `EXPLAIN`'s per-side file labels.
        lookup_path: PathBuf,
    },
    /// A Parquet `FROM` joined to SQLite lookup tables (#598).
    #[cfg(feature = "engine-column")]
    ColumnSqlite {
        /// The Parquet file's engine, the probe side -- or, with `others`,
        /// the first of the Parquet tables, one of which probes.
        driving: BatchEngine,
        /// More Parquet tables (#782): the largest one the query is a star
        /// around probes, and the rest build, next to the SQLite tables.
        others: Vec<BatchEngine>,
        /// The SQLite lookup engine, the build side.
        lookup: RowEngine,
        /// `lookup`'s file path, for `EXPLAIN`'s per-side file labels.
        lookup_path: PathBuf,
    },
    /// A table directory `FROM` joined to SQLite lookup tables (#772): its
    /// archive files and live pages are the probe side.
    #[cfg(all(feature = "engine-column", feature = "ingest-read"))]
    TableSqlite {
        /// The table directory's engine, the probe side. Boxed: its reader
        /// is much larger than the other variants' engines.
        driving: Box<TableEngine>,
        /// The SQLite lookup engine, the build side.
        lookup: RowEngine,
        /// `lookup`'s file path, for `EXPLAIN`'s per-side file labels.
        lookup_path: PathBuf,
    },
    /// Two stream engines joined via a windowed self-join (ADR-0022).
    StreamStream {
        /// The `FROM` side's stream engine (query alias: `FROM log AS <alias>`).
        left: StreamEngine,
        /// The `JOIN` side's stream engine (query alias: `JOIN log AS <alias>`).
        right: StreamEngine,
    },
}

impl CrossModeEngine {
    /// Opens a stream/SQLite cross-mode engine (ADR-0019): `driving_path`
    /// backs the query's `log` `FROM` side, `lookup_path` its SQLite
    /// `JOIN`-target lookup table.
    pub fn open_stream_sqlite(
        driving_path: &Path,
        lookup_path: &Path,
    ) -> Result<Self, EngineError> {
        Ok(CrossModeEngine::StreamSqlite {
            driving: StreamEngine::open(driving_path)?,
            lookup: RowEngine::open(lookup_path)?,
            lookup_path: lookup_path.to_path_buf(),
        })
    }

    /// Opens a Parquet/SQLite cross-mode engine (#598): the Parquet file's
    /// table (named after its stem) probes SQLite lookup tables.
    #[cfg(feature = "engine-column")]
    pub fn open_column_sqlite(parquet: &Path, lookup_path: &Path) -> Result<Self, EngineError> {
        Ok(CrossModeEngine::ColumnSqlite {
            driving: BatchEngine::open(parquet)?,
            others: Vec::new(),
            lookup: RowEngine::open(lookup_path)?,
            lookup_path: lookup_path.to_path_buf(),
        })
    }

    /// [`CrossModeEngine::open_column_sqlite`] for several Parquet files
    /// (#782): each is a table named after its stem. A query joining two of
    /// them (and SQLite lookup tables) probes with the larger and builds the
    /// other in memory. At least one file is required.
    #[cfg(feature = "engine-column")]
    pub fn open_column_tables_sqlite(
        parquets: &[&Path],
        lookup_path: &Path,
    ) -> Result<Self, EngineError> {
        let Some((first, rest)) = parquets.split_first() else {
            return Err(EngineError::new(
                ErrorKind::Open,
                "a Parquet/SQLite engine needs at least one Parquet file",
            ));
        };
        Ok(CrossModeEngine::ColumnSqlite {
            driving: BatchEngine::open(first)?,
            others: rest
                .iter()
                .map(|p| BatchEngine::open(p))
                .collect::<Result<_, _>>()?,
            lookup: RowEngine::open(lookup_path)?,
            lookup_path: lookup_path.to_path_buf(),
        })
    }

    /// Opens a table-directory/SQLite cross-mode engine (#772): the
    /// directory's table (named after the directory) probes SQLite lookup
    /// tables. Each query reads a fresh snapshot, so rows still in the live
    /// pages take part and flushing them to `archive/` changes nothing.
    #[cfg(all(feature = "engine-column", feature = "ingest-read"))]
    pub fn open_table_sqlite(table_dir: &Path, lookup_path: &Path) -> Result<Self, EngineError> {
        Ok(CrossModeEngine::TableSqlite {
            driving: Box::new(TableEngine::open(table_dir)?),
            lookup: RowEngine::open(lookup_path)?,
            lookup_path: lookup_path.to_path_buf(),
        })
    }

    /// Opens a windowed stream-to-stream cross-mode engine (ADR-0022):
    /// `left_path`/`right_path` back the self-join's `FROM`/`JOIN` `log`
    /// aliases respectively.
    pub fn open_stream_stream(left_path: &Path, right_path: &Path) -> Result<Self, EngineError> {
        Ok(CrossModeEngine::StreamStream {
            left: StreamEngine::open(left_path)?,
            right: StreamEngine::open(right_path)?,
        })
    }
}

impl Engine for CrossModeEngine {
    fn open(_path: &Path) -> Result<Self, EngineError> {
        Err(EngineError::new(
            ErrorKind::Unsupported,
            "a cross-mode engine spans two files; use \
             CrossModeEngine::open_stream_sqlite or \
             CrossModeEngine::open_stream_stream instead of Engine::open",
        ))
    }

    fn mode(&self) -> Mode {
        Mode::Cross
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        match self {
            CrossModeEngine::StreamSqlite {
                driving, lookup, ..
            } => run_query(driving, lookup, sql),
            #[cfg(feature = "engine-column")]
            CrossModeEngine::ColumnSqlite {
                driving,
                others,
                lookup,
                ..
            } => run_sources_query(&file_sources(driving, others), lookup, sql),
            #[cfg(all(feature = "engine-column", feature = "ingest-read"))]
            CrossModeEngine::TableSqlite {
                driving, lookup, ..
            } => {
                driving.refresh()?;
                run_sources_query(&[ColumnSource::Dir(driving)], lookup, sql)
            }
            CrossModeEngine::StreamStream { left, right } => {
                run_stream_stream_query(left, right, sql)
            }
        }
    }

    fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        match self {
            CrossModeEngine::StreamSqlite {
                driving,
                lookup,
                lookup_path,
            } => explain_plan(driving, lookup, lookup_path, sql),
            #[cfg(feature = "engine-column")]
            CrossModeEngine::ColumnSqlite {
                driving,
                others,
                lookup,
                lookup_path,
            } => explain_column_plan(&file_sources(driving, others), lookup, lookup_path, sql),
            #[cfg(all(feature = "engine-column", feature = "ingest-read"))]
            CrossModeEngine::TableSqlite {
                driving,
                lookup,
                lookup_path,
            } => explain_column_plan(&[ColumnSource::Dir(driving)], lookup, lookup_path, sql),
            CrossModeEngine::StreamStream { left, right } => {
                explain_stream_stream_plan(left, right, sql)
            }
        }
    }

    fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError> {
        let sections = match self {
            CrossModeEngine::StreamSqlite {
                driving, lookup, ..
            } => explain_opcodes_query(driving, lookup, sql)?,
            #[cfg(feature = "engine-column")]
            CrossModeEngine::ColumnSqlite {
                driving,
                others,
                lookup,
                ..
            } => explain_column_opcodes(&file_sources(driving, others), lookup, sql)?,
            #[cfg(all(feature = "engine-column", feature = "ingest-read"))]
            CrossModeEngine::TableSqlite {
                driving, lookup, ..
            } => explain_column_opcodes(&[ColumnSource::Dir(driving)], lookup, sql)?,
            CrossModeEngine::StreamStream { left, .. } => {
                explain_opcodes_stream_stream_query(left, sql)?
            }
        };
        Ok(to_engine_opcode_sections(sections))
    }

    fn stats(&self) -> FileStats {
        match self {
            CrossModeEngine::StreamSqlite { driving, .. } => driving.stats(),
            #[cfg(feature = "engine-column")]
            CrossModeEngine::ColumnSqlite { driving, .. } => driving.stats(),
            #[cfg(all(feature = "engine-column", feature = "ingest-read"))]
            CrossModeEngine::TableSqlite { driving, .. } => driving.stats(),
            CrossModeEngine::StreamStream { left, .. } => left.stats(),
        }
    }

    fn tables(&self) -> Result<Vec<TableInfo>, EngineError> {
        match self {
            CrossModeEngine::StreamSqlite {
                driving, lookup, ..
            } => {
                let mut tables = driving.tables()?;
                tables.extend(lookup.tables()?);
                Ok(tables)
            }
            #[cfg(feature = "engine-column")]
            CrossModeEngine::ColumnSqlite {
                driving,
                others,
                lookup,
                ..
            } => {
                let mut tables = driving.tables()?;
                for other in others {
                    tables.extend(other.tables()?);
                }
                tables.extend(lookup.tables()?);
                Ok(tables)
            }
            #[cfg(all(feature = "engine-column", feature = "ingest-read"))]
            CrossModeEngine::TableSqlite {
                driving, lookup, ..
            } => {
                let mut tables = driving.tables()?;
                tables.extend(lookup.tables()?);
                Ok(tables)
            }
            CrossModeEngine::StreamStream { left, right } => {
                let mut tables = left.tables()?;
                tables.extend(right.tables()?);
                Ok(tables)
            }
        }
    }
}
