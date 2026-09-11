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
//! driving side, one SQLite lookup table. Two unbounded streams joined,
//! SQLite as the driving side, and key-restricted materialization are all
//! out of scope.

use super::cross_mode;
use super::row::RowEngine;
use super::stream::StreamEngine;
use super::{single_statement, Cell, Engine, EngineError, ErrorKind, PlanRow, QueryResult};
use crate::codegen::batch::{self as planner, split_qualified, PlanError, TableStats};
use crate::parser::ast::Select;
use crate::parser::ParseError;
use crate::vm::batch::Batch;
use crate::vm::engine::run_join_segments;

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
