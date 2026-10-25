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
//! `EXPLAIN`/`explain_opcodes` don't support this shape yet.
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

use std::sync::Arc;

#[cfg(feature = "engine-column")]
use super::column::BatchEngine;
use super::cross_mode;
use super::row::RowEngine;
use super::stream::StreamEngine;
use super::{
    single_statement, Cell, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeRow,
    OpcodeSection, PlanRow, QueryResult, TableInfo,
};
use crate::codegen::batch::{self as planner, split_qualified, PlanError, TableStats};
use crate::parser::ast::{
    Expr, ExprKind, FunctionArgs, Join, JoinConstraint, ResultColumn, Select, TableRefKind,
};
use crate::parser::ParseError;
use crate::parser::Span;
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

    let known = lookup
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
        let is_known = known.iter().any(|t| t.name.eq_ignore_ascii_case(join_name));
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

/// `*` in the select list of a cross-mode join, expanded over every table
/// of the `FROM`/`JOIN` clauses in the order written: `driving`'s
/// `driving_columns`, then each SQLite table's own columns. A `JOIN ...
/// USING (k)` lists `k` once, from the table written first (#771).
fn expand_cross_mode_star(
    mut select: Select,
    driving: &str,
    driving_columns: &[String],
    lookup: &RowEngine,
) -> Result<Select, EngineError> {
    if !select
        .columns
        .iter()
        .any(|c| matches!(c, ResultColumn::Star))
    {
        return Ok(select);
    }
    let Some(from) = select.from.as_ref() else {
        return Ok(select);
    };
    let known = lookup.tables()?;
    let using_key = |j: &Join| match &j.constraint {
        Some(JoinConstraint::Using(columns)) => columns.first().cloned(),
        _ => None,
    };
    let tables = std::iter::once((&from.first, None))
        .chain(from.joins.iter().map(|j| (&j.table, using_key(j))));
    let mut star: Vec<(String, String)> = Vec::new();
    for (table, skip) in tables {
        let Some(name) = table.name() else {
            continue;
        };
        let columns: Vec<String> = if name.eq_ignore_ascii_case(driving) {
            driving_columns.to_vec()
        } else {
            let info = known
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    EngineError::new(ErrorKind::Compile, format!("unknown lookup table: {name}"))
                })?;
            info.columns.iter().map(|c| c.name.clone()).collect()
        };
        for column in columns {
            if skip
                .as_ref()
                .is_some_and(|k| k.eq_ignore_ascii_case(&column))
            {
                continue;
            }
            star.push((name.to_string(), column));
        }
    }
    // Headers are bare column names, duplicates included (`id`, `id`), as
    // in SQLite and DuckDB; a query names one of them with its table.
    let star: Vec<ResultColumn> = star
        .into_iter()
        .map(|(table, column)| ResultColumn::Expr {
            expr: column_expr(Some(table), column.clone()),
            alias: Some(column),
        })
        .collect();
    select.columns = select
        .columns
        .into_iter()
        .flat_map(|c| match c {
            ResultColumn::Star => star.clone(),
            other => vec![other],
        })
        .collect();
    Ok(select)
}

fn column_expr(table: Option<String>, name: String) -> Expr {
    Expr {
        kind: ExprKind::Column {
            table,
            catalog: None,
            name,
        },
        span: Span::UNKNOWN,
    }
}

/// The table an unqualified column of a cross-mode join belongs to, as in
/// SQLite and DuckDB: the one table of the `FROM`/`JOIN` clauses that has
/// it. Several tables having it is an ambiguity error, except for a
/// `USING (k)` key, which is merged and means the table written first.
/// `None` when no table has it (a select-list alias, or an unknown name
/// the planner reports).
fn column_owner(
    name: &str,
    tables: &[(String, Vec<String>)],
    using_keys: &[String],
) -> Result<Option<String>, EngineError> {
    let owners: Vec<&String> = tables
        .iter()
        .filter(|(_, columns)| columns.iter().any(|c| c.eq_ignore_ascii_case(name)))
        .map(|(table, _)| table)
        .collect();
    match owners.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some((*only).clone())),
        [first, ..] if using_keys.iter().any(|k| k.eq_ignore_ascii_case(name)) => {
            Ok(Some((*first).clone()))
        }
        _ => Err(EngineError::new(
            ErrorKind::Compile,
            format!(
                "ambiguous column name: {name} (in {}); qualify it with its table",
                owners
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// Qualifies every unqualified column of `expr` with its table, so the
/// planner never has to guess which side of the join a bare name is on.
fn qualify_columns(
    expr: &mut Expr,
    owner: &impl Fn(&str) -> Result<Option<String>, EngineError>,
) -> Result<(), EngineError> {
    match &mut expr.kind {
        ExprKind::Column {
            table: table @ None,
            name,
            ..
        } => {
            *table = owner(name)?;
        }
        ExprKind::FunctionCall { args, tail, .. } => {
            if let FunctionArgs::List(list) = args {
                for e in list {
                    qualify_columns(e, owner)?;
                }
            }
            if let Some(tail) = tail {
                if let Some(filter) = &mut tail.filter {
                    qualify_columns(filter, owner)?;
                }
            }
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::IsNull { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Collate { expr: inner, .. }
        | ExprKind::Paren(inner) => qualify_columns(inner, owner)?,
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            qualify_columns(lhs, owner)?;
            qualify_columns(rhs, owner)?;
        }
        ExprKind::Between {
            expr: inner,
            lo,
            hi,
            ..
        } => {
            qualify_columns(inner, owner)?;
            qualify_columns(lo, owner)?;
            qualify_columns(hi, owner)?;
        }
        ExprKind::In {
            expr: inner, list, ..
        } => {
            qualify_columns(inner, owner)?;
            for e in list {
                qualify_columns(e, owner)?;
            }
        }
        ExprKind::Like {
            expr: inner,
            pattern,
            escape,
            ..
        } => {
            qualify_columns(inner, owner)?;
            qualify_columns(pattern, owner)?;
            if let Some(e) = escape {
                qualify_columns(e, owner)?;
            }
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                qualify_columns(o, owner)?;
            }
            for (when, then) in whens {
                qualify_columns(when, owner)?;
                qualify_columns(then, owner)?;
            }
            if let Some(e) = else_ {
                qualify_columns(e, owner)?;
            }
        }
        // Literals and parameters hold no column; subqueries are refused by
        // the column validator before a select gets here.
        _ => {}
    }
    Ok(())
}

/// Qualifies every unqualified column of a cross-mode join's select list,
/// `WHERE`, `GROUP BY`, `HAVING` and `ORDER BY` with the table that has it
/// ([`column_owner`]). Before this, a bare name silently meant the `FROM`
/// table: `WHERE id = 10` on `a JOIN b ON a.id = b.id` filtered `a` where
/// SQLite and DuckDB report an ambiguous column.
fn qualify_cross_mode_columns(
    mut select: Select,
    driving: &str,
    driving_columns: &[String],
    lookup: &RowEngine,
) -> Result<Select, EngineError> {
    let Some(from) = select.from.as_ref() else {
        return Ok(select);
    };
    let known = lookup.tables()?;
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    for table in std::iter::once(&from.first).chain(from.joins.iter().map(|j| &j.table)) {
        let Some(name) = table.name() else {
            continue;
        };
        let columns = if name.eq_ignore_ascii_case(driving) {
            driving_columns.to_vec()
        } else {
            known
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(name))
                .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
                .unwrap_or_default()
        };
        tables.push((name.to_string(), columns));
    }
    let using_keys: Vec<String> = from
        .joins
        .iter()
        .filter_map(|j| match &j.constraint {
            Some(JoinConstraint::Using(columns)) => columns.first().cloned(),
            _ => None,
        })
        .collect();
    let owner = |name: &str| column_owner(name, &tables, &using_keys);
    for column in &mut select.columns {
        if let ResultColumn::Expr { expr, alias } = column {
            // A bare `SELECT id` keeps `id` as its header once the
            // expression is rewritten to `production.id`.
            if let ExprKind::Column {
                table: None, name, ..
            } = &expr.kind
            {
                alias.get_or_insert_with(|| name.clone());
            }
            qualify_columns(expr, &owner)?;
        }
    }
    let exprs = select
        .where_clause
        .iter_mut()
        .chain(select.group_by.iter_mut())
        .chain(select.having.iter_mut());
    for expr in exprs {
        qualify_columns(expr, &owner)?;
    }
    // An `ORDER BY` term naming a select item (its alias, or the column
    // an unaliased `t.col` item shows) stays that reference.
    let items: Vec<String> = select
        .columns
        .iter()
        .filter_map(|c| match c {
            ResultColumn::Expr {
                alias: Some(alias), ..
            } => Some(alias.clone()),
            ResultColumn::Expr {
                expr:
                    Expr {
                        kind: ExprKind::Column { name, .. },
                        ..
                    },
                ..
            } => Some(name.clone()),
            _ => None,
        })
        .collect();
    for term in &mut select.order_by {
        let names_item = match &term.expr.kind {
            ExprKind::Column {
                table: None, name, ..
            } => items.iter().any(|i| i.eq_ignore_ascii_case(name)),
            _ => false,
        };
        if !names_item {
            qualify_columns(&mut term.expr, &owner)?;
        }
    }
    Ok(select)
}

/// [`resolve_group_by`] for a cross-mode join: `log`'s columns plus those
/// of every SQLite table the query names. Also expands `*` first.
fn resolve_cross_mode_group_by(
    select: Select,
    driving: &str,
    driving_columns: Vec<String>,
    lookup: &RowEngine,
) -> Result<Select, EngineError> {
    let select = expand_cross_mode_star(select, driving, &driving_columns, lookup)?;
    let select = if select.group_by.is_empty() {
        select
    } else {
        resolve_cross_mode_group_by_names(select, &driving_columns, lookup)?
    };
    qualify_cross_mode_columns(select, driving, &driving_columns, lookup)
}

/// The `GROUP BY` half of [`resolve_cross_mode_group_by`].
fn resolve_cross_mode_group_by_names(
    select: Select,
    driving_columns: &[String],
    lookup: &RowEngine,
) -> Result<Select, EngineError> {
    let named: Vec<&str> = select
        .from
        .iter()
        .flat_map(|f| std::iter::once(&f.first).chain(f.joins.iter().map(|j| &j.table)))
        .filter_map(|t| t.name())
        .collect();
    let mut columns = driving_columns.to_vec();
    for table in lookup.tables()? {
        if named.iter().any(|n| n.eq_ignore_ascii_case(&table.name)) {
            columns.extend(table.columns.into_iter().map(|c| c.name));
        }
    }
    resolve_group_by(select, columns)
}

/// Runs a cross-mode `SELECT`: `log` (the stream engine's table) joined to
/// one SQLite lookup table on `lookup`. See the module docs for the
/// exactly-one-JOIN, stream-drives-lookup-builds v1 scope.
pub fn run_query(
    driving: &StreamEngine,
    lookup: &RowEngine,
    sql: &str,
) -> Result<QueryResult, EngineError> {
    let select =
        resolve_cross_mode_group_by(parse(sql)?, super::stream::TABLE, driving.columns(), lookup)?;
    let table = super::stream::TABLE;
    if join_count(&select) > 1 {
        let plan = plan_multi_join(&select, table, lookup)?;
        let requests = driving.column_requests(&plan.left_columns)?;
        return execute_multi_join(driving.segments(&requests), lookup, &select, &plan);
    }
    let (lookup_table, plan) = plan_join(&select, table, lookup)?;
    let requests = driving.column_requests(&plan.left_columns)?;
    execute_join(
        driving.segments(&requests),
        lookup,
        &select,
        &lookup_table,
        &plan,
    )
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
    let stmt = single_statement(sql)?;
    let select = resolve_cross_mode_group_by(
        driving.parse_join(&stmt)?,
        driving.table_name(),
        driving.columns(),
        lookup,
    )?;
    let table = driving.table_name();
    let pruning = prune::probe_pruning(&select, lookup)?;
    // A lookup filter that matches no row leaves an inner join empty:
    // probe nothing rather than the whole file.
    if join_count(&select) > 1 {
        let plan = plan_multi_join(&select, table, lookup)?;
        return driving.with_probe_segments(
            &plan.left_columns,
            pruning.predicate.as_ref(),
            |mut segments| {
                if pruning.empty {
                    segments.clear();
                }
                execute_multi_join(segments, lookup, &select, &plan)
            },
        );
    }
    let (lookup_table, plan) = plan_join(&select, table, lookup)?;
    driving.with_probe_segments(
        &plan.left_columns,
        pruning.predicate.as_ref(),
        |mut segments| {
            if pruning.empty {
                segments.clear();
            }
            execute_join(segments, lookup, &select, &lookup_table, &plan)
        },
    )
}

/// `EXPLAIN QUERY PLAN` for a Parquet/SQLite join (#770), labelling each
/// side by mode and file as [`explain_plan`] does for stream/SQLite, plus
/// a row for the row-group pruning the `WHERE` implies on the Parquet file
/// (see [`prune`]).
#[cfg(feature = "engine-column")]
fn explain_column_plan(
    driving: &BatchEngine,
    lookup: &RowEngine,
    lookup_file: &Path,
    sql: &str,
) -> Result<Vec<PlanRow>, EngineError> {
    let stmt = single_statement(sql)?;
    let select = resolve_cross_mode_group_by(
        driving.parse_join(&stmt)?,
        driving.table_name(),
        driving.columns(),
        lookup,
    )?;
    check_single_join_explain(&select)?;
    let table = driving.table_name();
    let (lookup_table, plan) = plan_join(&select, table, lookup)?;

    let probe_stats = TableStats {
        source: Some(format!("batch {}", driving.path().display())),
        ..driving.table_stats()
    };
    let lookup_stats = TableStats {
        row_groups: 1,
        rows: 0,
        source: Some(format!("sqlite {}", lookup_file.display())),
    };
    let nodes = planner::explain(&select, |name| {
        if name.eq_ignore_ascii_case(table) {
            probe_stats.clone()
        } else if name.eq_ignore_ascii_case(&lookup_table) {
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
    let mut rows: Vec<PlanRow> = nodes
        .into_iter()
        .map(|n| PlanRow {
            id: i64::from(n.id),
            parent: i64::from(n.parent),
            detail: n.detail,
        })
        .collect();

    let pruning = prune::probe_pruning(&select, lookup)?;
    let kept = match (pruning.empty, pruning.predicate.as_ref()) {
        (true, _) => Some(0),
        (false, Some(predicate)) => Some(driving.with_probe_segments(
            &plan.left_columns,
            Some(predicate),
            |segments| Ok(segments.len()),
        )?),
        (false, None) => None,
    };
    if let Some(kept) = kept {
        rows.push(PlanRow {
            id: rows
                .iter()
                .map(|r| r.id)
                .max()
                .map_or(0, |m| m.saturating_add(1)),
            parent: 0,
            detail: format!(
                "PRUNE ROW GROUPS OF {table} BY {lookup_table} FILTER ({kept} of {} kept)",
                probe_stats.row_groups
            ),
        });
    }
    Ok(rows)
}

/// `EXPLAIN` (bare opcode listing) for a Parquet/SQLite join (#770),
/// mirroring [`explain_opcodes_query`] with the plan execution compiles:
/// the SQLite build side is `"row"`, the Parquet probe and the body
/// `"batch"`.
#[cfg(feature = "engine-column")]
fn explain_column_opcodes(
    driving: &BatchEngine,
    lookup: &RowEngine,
    sql: &str,
) -> Result<Vec<(&'static str, planner::OpcodeSection)>, EngineError> {
    let stmt = single_statement(sql)?;
    let select = resolve_cross_mode_group_by(
        driving.parse_join(&stmt)?,
        driving.table_name(),
        driving.columns(),
        lookup,
    )?;
    check_single_join_explain(&select)?;
    let table = driving.table_name();
    let (_, plan) = plan_join(&select, table, lookup)?;
    Ok(vec![
        (
            "row",
            planner::OpcodeSection {
                label: "JOIN build (sqlite)".to_string(),
                rows: planner::render_program(&plan.build),
            },
        ),
        (
            "batch",
            planner::OpcodeSection {
                label: format!("JOIN probe (parquet {table})"),
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

/// A star join's `EXPLAIN` is not supported, as for stream/SQLite (#394).
#[cfg(feature = "engine-column")]
fn check_single_join_explain(select: &Select) -> Result<(), EngineError> {
    if join_count(select) > 1 {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            "EXPLAIN for a multi-way Parquet/SQLite join is not yet \
             supported; run the query directly instead",
        ));
    }
    Ok(())
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
) -> Result<(String, JoinProgram), EngineError> {
    let lookup_table = resolve_sides(select, driving, lookup)?;
    let plan =
        planner::compile_join_build_side(select, &lookup_table, planner::BuildSourceKind::RowTable)
            .map_err(plan_err)?;
    Ok((lookup_table, plan))
}

/// Builds the lookup side from SQLite and probes it with `segments`.
fn execute_join<S: VmSegment>(
    segments: Vec<S>,
    lookup: &RowEngine,
    select: &Select,
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
        let batch = cross_mode::scan_table_as_batch(lookup, lookup_table, &raw_lookup_cols)
            .map_err(|e| crate::vm::batch::VmError::SegmentLoad {
                reason: e.to_string(),
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
    Ok(query_result(select, rows.into_rows()))
}

fn query_result(select: &Select, rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult {
        columns: planner::output_column_names(select),
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
) -> Result<MultiJoinProgram, EngineError> {
    let lookup_tables = resolve_multi_sides(select, driving, lookup)?;
    planner::compile_cross_mode_multi_join(select, &lookup_tables).map_err(plan_err)
}

/// [`execute_join`] for a star join.
fn execute_multi_join<S: VmSegment>(
    segments: Vec<S>,
    lookup: &RowEngine,
    select: &Select,
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
            cross_mode::scan_table_as_batch(lookup, &build.table_name, raw).map_err(|e| {
                crate::vm::batch::VmError::SegmentLoad {
                    reason: e.to_string(),
                }
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
    Ok(query_result(select, rows.into_rows()))
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
    let lookup_table = resolve_sides(&select, super::stream::TABLE, lookup)?;

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
        resolve_cross_mode_group_by(parse(sql)?, super::stream::TABLE, driving.columns(), lookup)?;
    if select.from.as_ref().is_some_and(|f| f.joins.len() > 1) {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            "EXPLAIN for a multi-way cross-mode join (#394) is not yet \
             supported; run the query directly instead",
        ));
    }
    resolve_sides(&select, super::stream::TABLE, lookup)?;
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
        /// The Parquet file's engine, the probe side.
        driving: BatchEngine,
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
                driving, lookup, ..
            } => run_column_query(driving, lookup, sql),
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
                lookup,
                lookup_path,
            } => explain_column_plan(driving, lookup, lookup_path, sql),
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
                driving, lookup, ..
            } => explain_column_opcodes(driving, lookup, sql)?,
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
