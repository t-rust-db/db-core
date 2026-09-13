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
//! side, so the query must write it as the `JOIN` target, with the stream
//! table `log` as `FROM`. Writing it the other way round (SQLite driving)
//! is out of scope for v1 (epic #317's "out of scope" section) and is
//! rejected as [`ErrorKind::Unsupported`], not silently reinterpreted.
//!
//! A sibling seam to [`cross_mode`](super::cross_mode), for the same
//! reason: ADR 0000 §(c) forbids the SQLite side from naming `vm::batch`,
//! so this module -- which names `row::RowEngine`, `stream::StreamEngine`
//! and `vm::batch` together -- cannot live inside any of `engine::row`,
//! `engine::stream` or `engine::column` (`tests/unit/layer_isolation_test.rs`
//! enforces this by source scan).
//!
//! v1 scope, per epic #317: exactly one `JOIN`, the stream table as the
//! driving side, one SQLite lookup table. SQLite as the driving side and
//! key-restricted materialization are still out of scope.
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

use std::sync::Arc;

use super::cross_mode;
use super::row::RowEngine;
use super::stream::StreamEngine;
use super::{single_statement, Cell, Engine, EngineError, ErrorKind, PlanRow, QueryResult};
use crate::codegen::batch::{self as planner, split_qualified, PlanError, TableStats};
use crate::parser::ast::{Select, TableRefKind};
use crate::parser::ParseError;
use crate::storage::stream::{ColumnRequest, StreamSegment};
use crate::vm::batch::{Batch, Segment as VmSegment, Value};
use crate::vm::engine::run_join_segments;
use crate::vm::stream::Scope;

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
/// the name of the SQLite lookup table on the other side. Rejects
/// anything that isn't exactly "stream `FROM`, SQLite `JOIN`".
fn resolve_sides(select: &Select, lookup: &RowEngine) -> Result<String, EngineError> {
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

    if !from_name.eq_ignore_ascii_case(super::stream::TABLE) {
        if join_name.eq_ignore_ascii_case(super::stream::TABLE) {
            return Err(EngineError::new(
                ErrorKind::Unsupported,
                "the stream table `log` must be the driving (FROM) side; \
                 SQLite as the driving side is out of scope (#317)",
            ));
        }
        return Err(EngineError::new(
            ErrorKind::Compile,
            format!(
                "cross-mode join requires `log` (the stream table) joined \
                 to a SQLite lookup table; got `{from_name}` JOIN `{join_name}`"
            ),
        ));
    }

    let known = lookup
        .tables()?
        .into_iter()
        .any(|t| t.name.eq_ignore_ascii_case(join_name));
    if !known {
        return Err(EngineError::new(
            ErrorKind::Compile,
            format!("unknown lookup table: {join_name}"),
        ));
    }
    Ok(join_name.to_string())
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

/// Runs a cross-mode `SELECT`: `log` (the stream engine's table) joined to
/// one SQLite lookup table on `lookup`. See the module docs for the
/// exactly-one-JOIN, stream-drives-lookup-builds v1 scope.
pub fn run_query(
    driving: &StreamEngine,
    lookup: &RowEngine,
    sql: &str,
) -> Result<QueryResult, EngineError> {
    let select = parse(sql)?;
    let lookup_table = resolve_sides(&select, lookup)?;
    let plan = planner::compile_join(&select).map_err(plan_err)?;

    let stream_columns = driving.column_requests(&plan.left_columns)?;
    let segments = driving.segments(&stream_columns);

    let raw_lookup_cols: Vec<String> = plan
        .right_columns
        .iter()
        .map(|n| split_qualified(n).1.to_string())
        .collect();
    let lookup_batch = cross_mode::scan_table_as_batch(lookup, &lookup_table, &raw_lookup_cols)?;
    let lookup_batch = requalify(lookup_batch, &raw_lookup_cols, &plan.right_columns);

    let rows = run_join_segments(segments, &lookup_batch, &plan)
        .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;

    Ok(QueryResult {
        columns: planner::output_column_names(&select),
        rows: rows
            .into_iter()
            .map(|r| r.into_iter().map(Cell::from).collect())
            .collect(),
        scope_report: None,
    })
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
    let lookup_table = resolve_sides(&select, lookup)?;

    let stream_stats = TableStats {
        source: Some(format!("stream {}", driving.path().display())),
        ..driving.table_stats()
    };
    let lookup_stats = TableStats {
        row_groups: 1,
        rows: 0,
        source: Some(format!("sqlite {}", lookup_file.display())),
    };

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
    let select = parse(sql)?;
    let (left_alias, right_alias) = resolve_stream_stream_sides(&select)?;
    let scope = super::stream::resolve_scope(&select);
    let normalized = alias_normalize(select, &left_alias, &right_alias);
    let plan = planner::compile_join(&normalized).map_err(plan_err)?;

    let left_cols = left.column_requests(&plan.left_columns)?;
    let left_segments = left.segments_in_range(&left_cols, scope);

    let right_cols = right.column_requests(&plan.right_columns)?;
    let right_segments = right.segments_in_range(&right_cols, scope);
    let right_batch = materialize_segments(&right_segments, &right_cols)?;

    let rows = run_join_segments(left_segments, &right_batch, &plan)
        .map_err(|e| EngineError::new(ErrorKind::Execute, e))?;

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
