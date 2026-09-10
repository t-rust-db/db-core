// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
use super::aggregate::{find_index_only_count, find_index_only_sum, IndexOnlyCount};
use super::entry::{scan_dispatch, ScanDispatch};
use super::join_access::{choose_auto_index_probe, choose_join_access, AutoIndexProbe, JoinAccess};
use super::limit_scan::{
    find_covering_index, find_skip_scan_index, is_rowid_reference, top_level_equality_operands,
};
use super::*;
use crate::codegen::row::subquery::{
    is_comparison_op, resolve_from_table_schema, resolve_subquery_schema, subquery_is_correlated,
    top_level_and_conjuncts,
};
/// One row of `EXPLAIN QUERY PLAN` output (#243) -- SQLite's own EQP
/// shape (`id, parent, notused, detail`), distinct from plain
/// `EXPLAIN`'s per-instruction [`crate::vm::row::explain::ExplainRow`].
/// `detail` reads like the oracle's own EQP (`SCAN ...`/`SEARCH ...
/// USING ...`) but isn't guaranteed byte-identical -- Requirement 10's
/// VM-diff guarantee is plain `EXPLAIN`'s job, not this human-readable
/// summary's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqpRow {
    /// This row's identifier within the plan.
    pub id: i32,
    /// The `id` of this row's parent in the plan tree (0 for a top-level row).
    pub parent: i32,
    /// Unused column, kept for SQLite's `EXPLAIN QUERY PLAN` shape.
    pub notused: i32,
    /// Human-readable plan step, e.g. `SCAN ...`/`SEARCH ... USING ...`.
    pub detail: String,
}

/// A table binding's `FROM`-clause display name for EQP output:
/// `name AS alias` when aliased, `name` otherwise -- matching how a
/// `Column` reference would need to qualify it.
pub(super) fn eqp_display_name(table_ref: &TableRef) -> String {
    let name = table_ref.name().unwrap_or("(subquery)");
    match &table_ref.alias {
        Some(alias) => format!("{name} AS {alias}"),
        None => name.to_string(),
    }
}

/// The identifier a [`TableBinding`] tracks alongside its `alias` —
/// a subquery-in-FROM (#257) has no catalog name of its own, so this
/// falls back to its (mandatory) alias.
pub(super) fn table_binding_name(table_ref: &TableRef) -> String {
    table_ref
        .name()
        .map(str::to_string)
        .or_else(|| table_ref.alias.clone())
        .unwrap_or_default()
}

/// Builds `EXPLAIN QUERY PLAN`'s output for `select` (#243): one row per
/// `FROM`-clause table, `SCAN` for a full `Rewind`/`Next` scan or
/// `SEARCH ... USING ...` for a `SeekRowid`/`SeekIndexEq` point lookup --
/// reusing [`choose_join_access`] (the join codegen's own decision
/// function) for a join's inner tables, and the same rowid-equality
/// check [`try_compile_rowid_seek`] uses for a single-table `SELECT`'s
/// `WHERE` clause, so the report can never drift from what
/// [`compile_select_joined`]/[`compile_direct_scan`] actually compile.
///
/// #250 note: this reports FROM-clause order, not the RIGHT-JOIN
/// execution reordering `compile_select_joined` may apply internally --
/// `choose_join_access` is still evaluated against each table's
/// FROM-order-preceding siblings, matching what a RIGHT-JOIN-free query
/// actually executes; a query that also has a RIGHT JOIN keeps working
/// via the ordinary (unseeked) fallback below since `on_expr` there
/// won't resolve against these FROM-order-built `prior_bindings`.
///
/// #470's cost-model reordering of a pure `INNER`/`CROSS` chain *is*
/// reflected here (unlike the RIGHT-JOIN case above): `execution_order`
/// mirrors `join_order::plan_join_order`'s decision exactly, rows are
/// emitted in that order, and each join's `ON` clause is associated
/// with the execution level where every table it references is first
/// fully bound (via `join_order::referenced_binding_indices`) rather
/// than assumed adjacent -- matching `compile_select_joined`'s own
/// `LevelCheck` placement for a reordered chain. A level with more than
/// one such check (a multi-table `ON` chain landing on the same level)
/// falls back to reporting a plain `SCAN`, same as
/// `compile_join_level_traverse`'s single-check-only seek optimization.
pub fn explain_query_plan(
    select: &Select,
    schemas: &[TableSchema],
    stats_by_table: &std::collections::HashMap<String, crate::codegen::row::planner::Stats>,
    catalog: &[TableSchema],
) -> Result<Vec<EqpRow>, CodegenError> {
    // #539: a `UNION`/`UNION ALL` compound reports each arm's own plan
    // nested under a synthetic `COMPOUND QUERY` root, matching the
    // oracle's own EQP shape -- `schemas` (already resolved by the
    // caller for `select`'s own `FROM`) only covers the left-most arm;
    // every other arm resolves its own `FROM` against `catalog` here,
    // the same way the subquery recursion below does.
    if !select.compound.is_empty() {
        return explain_compound_query_plan(select, schemas, stats_by_table, catalog);
    }
    let Some(from) = &select.from else {
        return Err(CodegenError::NoFromClause);
    };
    let table_refs: Vec<&TableRef> = std::iter::once(&from.first)
        .chain(from.joins.iter().map(|j| &j.table))
        .collect();
    if schemas.len() != table_refs.len() {
        return Err(CodegenError::Unsupported {
            reason: format!(
                "explain_query_plan needs one schema per FROM table ({} tables, {} schemas \
                 given)",
                table_refs.len(),
                schemas.len()
            ),
        });
    }
    let bindings: Vec<TableBinding> = table_refs
        .iter()
        .zip(schemas.iter())
        .enumerate()
        .map(|(i, (table_ref, schema))| TableBinding {
            alias: table_ref.alias.clone(),
            name: table_binding_name(table_ref),
            schema: std::rc::Rc::new(schema.clone()),
            cursor: i32::try_from(i).unwrap_or(0),
            forced_null: false,
            stats: stats_by_table
                .get(&schema.name)
                .cloned()
                .unwrap_or_default(),
        })
        .collect();
    let n = bindings.len();

    let reorder = super::join_order::is_reorderable_inner_chain(from).then(|| {
        let on_exprs: Vec<Option<Expr>> = from
            .joins
            .iter()
            .map(|j| match &j.constraint {
                Some(JoinConstraint::On(e)) => Some(e.clone()),
                _ => None,
            })
            .collect();
        let seekable = super::join_order::seekable_tables(schemas, &on_exprs);
        let costs = super::join_order::scan_costs(schemas, stats_by_table, &seekable);
        super::join_order::plan_join_order(&costs)
    });
    let execution_order: Vec<usize> = reorder.clone().unwrap_or_else(|| (0..n).collect());
    let mut pos_of = vec![0usize; n];
    for (pos, &orig) in execution_order.iter().enumerate() {
        if let Some(slot) = pos_of.get_mut(orig) {
            *slot = pos;
        }
    }
    // Only populated when `reorder` fired: `level_joins[level]` lists
    // every join index whose `ON` clause is checkable once execution
    // reaches `level` (i.e. every table it references has
    // `pos_of[..] <= level`) -- mirrors `compile_select_joined_scan`'s
    // reordered-chain `LevelCheck` placement.
    let mut level_joins: Vec<Vec<usize>> = vec![Vec::new(); n];
    if reorder.is_some() {
        for (j, join) in from.joins.iter().enumerate() {
            let right_idx = j.saturating_add(1);
            let on_expr = match &join.constraint {
                Some(JoinConstraint::On(e)) => Some(e),
                _ => None,
            };
            // EQP is explanatory text, not the executed plan: a dangling
            // reference falls back to level 0 rather than failing the
            // EXPLAIN (the real compile reports it as `Internal`, #232).
            let level = match on_expr {
                Some(e) => super::join_order::referenced_binding_indices(e, &bindings)
                    .into_iter()
                    .chain(std::iter::once(right_idx))
                    .filter_map(|i| pos_of.get(i).copied())
                    .max()
                    .unwrap_or(0),
                None => pos_of.get(right_idx).copied().unwrap_or(0),
            };
            if let Some(slot) = level_joins.get_mut(level) {
                slot.push(j);
            }
        }
    }

    let dispatch = scan_dispatch(select);
    let mut rows = Vec::with_capacity(n);
    let mut next_id: i32 = 0;
    for (level, &orig) in execution_order.iter().enumerate() {
        let Some(&table_ref) = table_refs.get(orig) else {
            continue;
        };
        let Some(binding) = bindings.get(orig) else {
            continue;
        };
        let on_expr = if reorder.is_some() {
            match level_joins.get(level).map(Vec::as_slice) {
                Some([j]) => from.joins.get(*j).and_then(|join| match &join.constraint {
                    Some(JoinConstraint::On(e)) => Some(e),
                    _ => None,
                }),
                _ => None,
            }
        } else {
            level
                .checked_sub(1)
                .and_then(|i| from.joins.get(i))
                .and_then(|j| j.constraint.as_ref())
                .and_then(|c| match c {
                    JoinConstraint::On(e) => Some(e),
                    JoinConstraint::Using(_) => None,
                })
        };
        let prior_bindings: Vec<TableBinding> = if reorder.is_some() {
            bindings
                .iter()
                .enumerate()
                .filter(|&(i, _)| pos_of.get(i).is_some_and(|&p| p < level))
                .map(|(_, b)| b.clone())
                .collect()
        } else {
            bindings.get(..level).unwrap_or(&[]).to_vec()
        };
        let prior_bindings = prior_bindings.as_slice();
        // #282: `compile_direct_scan`'s seeks (rowid, covering index,
        // range, skip-scan) are only ever compiled when
        // `compile_select_scan` dispatches there -- an aggregate,
        // `GROUP BY` or `ORDER BY` single-table query `Rewind`s the
        // table (or walks an index end to end) instead, so reporting
        // those seeks for it would describe a plan the program never
        // took. The joined path is left as before.
        let direct_scan = !from.joins.is_empty() || dispatch == ScanDispatch::Direct;
        let access = if level == 0 && direct_scan {
            // The outermost table has no `ON` clause to seek against --
            // an equality `WHERE` predicate against its rowid still
            // gets `try_compile_rowid_seek`'s single-table fast path
            // (#137), so report that here too rather than a blanket
            // SCAN.
            select
                .where_clause
                .as_ref()
                .and_then(|where_expr| top_level_equality_operands(where_expr))
                .and_then(|(lhs, rhs)| {
                    if is_rowid_reference(&binding.schema, lhs) {
                        Some(JoinAccess::Rowid(rhs.clone()))
                    } else if is_rowid_reference(&binding.schema, rhs) {
                        Some(JoinAccess::Rowid(lhs.clone()))
                    } else {
                        None
                    }
                })
        } else {
            on_expr.and_then(|e| choose_join_access(binding, e, prior_bindings))
        };
        // #545/#547: when no structural seek exists for this level,
        // report the transient automatic index `choose_auto_index_probe`
        // would build instead of falling through to a blanket `SCAN` --
        // mirrors `joins/level.rs`'s own precedence (tried only once
        // `access` above comes up empty).
        let auto_index_probe = if access.is_none() {
            on_expr.and_then(|e| choose_auto_index_probe(binding, e, prior_bindings))
        } else {
            None
        };
        // #444: a covering-index scan only applies to the outermost
        // table's own `WHERE` clause (like the rowid-seek check above),
        // and only when `access` didn't already find a rowid seek --
        // `find_covering_index` only fires for a non-rowid indexed
        // column, so the two never actually overlap, but checking
        // `access.is_none()` keeps this branch's precedence explicit.
        let covering = if level == 0 && direct_scan && access.is_none() {
            find_covering_index(&binding.schema, select)
        } else {
            None
        };
        // #485: a skip-scan only applies to the outermost table's own
        // `WHERE` clause (like the rowid-seek/covering-index checks
        // above), and only once neither of those already found a
        // cheaper access path -- mirrors `compile_direct_scan`'s
        // dispatch precedence (rowid seek, then covering index, then
        // skip-scan, then plain scan) exactly, so this report can
        // never drift from what actually gets compiled.
        let skip_scan = if level == 0 && direct_scan && access.is_none() && covering.is_none() {
            find_skip_scan_index(&binding.schema, select, &binding.stats)
        } else {
            None
        };
        // #606: BETWEEN/IN/LIKE-prefix range-seek fast paths, checked
        // with the same precedence as `compile_direct_scan`'s dispatch
        // (rowid seek, covering index, then these) — only applies to
        // the outermost table's own `WHERE` clause, like the checks
        // above.
        let range_seek = if level == 0 && direct_scan && access.is_none() && covering.is_none() {
            super::range_scan::find_range_seek_detail(
                &binding.schema,
                select,
                &eqp_display_name(table_ref),
                catalog,
            )
        } else {
            None
        };
        let index_walk = if level == 0 && from.joins.is_empty() {
            aggregate_index_walk_detail(
                select,
                &binding.schema,
                dispatch,
                &eqp_display_name(table_ref),
                catalog,
            )?
        } else {
            None
        };
        let detail = if let Some(index_walk_detail) = index_walk {
            index_walk_detail
        } else if let Some(range_detail) = range_seek {
            range_detail
        } else {
            match (
                access,
                covering
                    .as_ref()
                    .and_then(|m| binding.schema.indexes.get(m.index_position)),
                skip_scan
                    .as_ref()
                    .and_then(|m| binding.schema.indexes.get(m.index_position))
                    .zip(skip_scan.as_ref().map(|m| m.column_position)),
            ) {
                (_, Some(index), _) => format!(
                    "SEARCH {} USING COVERING INDEX {} ({}=?)",
                    eqp_display_name(table_ref),
                    index.name,
                    index
                        .columns
                        .first()
                        .map_or_else(String::new, |c| c.name.clone())
                ),
                (None, None, Some((index, column_position))) => {
                    // Oracle sqlite3's own skip-scan EQP text, confirmed
                    // empirically (sqlite3 3.51.0): `SEARCH t USING INDEX
                    // idx (ANY(category) AND price=?)` -- one `ANY(col)`
                    // per unconstrained leading column, then `col=?` for
                    // the actually-probed column.
                    let parts: Vec<String> = index
                        .columns
                        .get(..column_position)
                        .unwrap_or_default()
                        .iter()
                        .map(|c| format!("ANY({})", c.name))
                        .chain(std::iter::once(format!(
                            "{}=?",
                            index
                                .columns
                                .get(column_position)
                                .map_or_else(String::new, |c| c.name.clone())
                        )))
                        .collect();
                    format!(
                        "SEARCH {} USING INDEX {} ({})",
                        eqp_display_name(table_ref),
                        index.name,
                        parts.join(" AND ")
                    )
                }
                // #545/#547: this level builds a transient automatic index
                // rather than falling back to a plain scan -- real sqlite3's
                // own wording for the equivalent case (confirmed empirically,
                // sqlite3 3.51.0): `SEARCH t USING AUTOMATIC COVERING INDEX
                // (col=?)`, no index name (it has none).
                (None, None, None) => match &auto_index_probe {
                    Some(AutoIndexProbe { key_column, .. }) => format!(
                        "SEARCH {} USING AUTOMATIC COVERING INDEX ({}=?)",
                        eqp_display_name(table_ref),
                        binding
                            .schema
                            .columns
                            .get(*key_column)
                            .map_or_else(String::new, |c| c.clone())
                    ),
                    None => format!("SCAN {}", eqp_display_name(table_ref)),
                },
                (Some(JoinAccess::Rowid(_)), None, _) => format!(
                    "SEARCH {} USING INTEGER PRIMARY KEY (rowid=?)",
                    eqp_display_name(table_ref)
                ),
                (Some(JoinAccess::UniqueIndex { index, .. }), None, _) => format!(
                    "SEARCH {} USING INDEX {} ({}=?)",
                    eqp_display_name(table_ref),
                    index.name,
                    index
                        .columns
                        .first()
                        .map_or_else(String::new, |c| c.name.clone())
                ),
            }
        };
        let row_id = next_id;
        next_id = next_id.saturating_add(1);
        rows.push(EqpRow {
            id: row_id,
            parent: 0,
            notused: 0,
            detail,
        });

        // #532: a materialized `FROM`-subquery/view has its own inner
        // scan (`materialize_from_subquery`'s `compile_select_scan` --
        // possibly now an index seek, once a WHERE conjunct got pushed
        // into `table_ref`'s own `where_clause`) that this row's plain
        // "SCAN ..."/"SEARCH ..." text can't describe on its own, since
        // it describes the *outer* query's access to the materialized
        // result, not what filled it. Recurse into the subquery's own
        // plan and nest its rows underneath this one, offsetting ids so
        // they stay unique across the whole (possibly further-nested)
        // tree.
        if let TableRefKind::Subquery(inner) = &table_ref.kind {
            if let Some(inner_from) = &inner.from {
                let inner_table_refs: Vec<&TableRef> = std::iter::once(&inner_from.first)
                    .chain(inner_from.joins.iter().map(|j| &j.table))
                    .collect();
                let inner_schemas: Result<Vec<TableSchema>, CodegenError> = inner_table_refs
                    .iter()
                    .map(|table_ref| resolve_from_table_schema(table_ref, catalog))
                    .collect();
                if let Ok(inner_schemas) = inner_schemas {
                    if let Ok(child_rows) =
                        explain_query_plan(inner, &inner_schemas, stats_by_table, catalog)
                    {
                        let offset = next_id;
                        for mut child in child_rows {
                            let was_top_level = child.parent == 0;
                            child.id = child.id.saturating_add(offset);
                            child.parent = if was_top_level {
                                row_id
                            } else {
                                child.parent.saturating_add(offset)
                            };
                            next_id = next_id.max(child.id.saturating_add(1));
                            rows.push(child);
                        }
                    }
                }
            }
        }
    }
    // #282: every scalar subquery compared against in a top-level
    // `WHERE` conjunct gets its own node with the subquery's own plan
    // nested underneath, like sqlite3's `SCALAR SUBQUERY n` rows --
    // until now it was invisible in EQP. `CORRELATED` mirrors whether
    // `hoist_uncorrelated_where_subqueries` evaluates it once up front
    // (uncorrelated) or the scan re-evaluates it per row (`Once`-less).
    if let Some(where_expr) = &select.where_clause {
        let mut subquery_no: i32 = 0;
        for conjunct in top_level_and_conjuncts(where_expr) {
            let ExprKind::Binary { op, lhs, rhs } = &conjunct.kind else {
                continue;
            };
            if !is_comparison_op(*op) {
                continue;
            }
            for side in [lhs.as_ref(), rhs.as_ref()] {
                let ExprKind::Subquery(inner) = &side.kind else {
                    continue;
                };
                subquery_no = subquery_no.saturating_add(1);
                explain_scalar_subquery(
                    inner,
                    subquery_no,
                    stats_by_table,
                    catalog,
                    &mut rows,
                    &mut next_id,
                );
            }
        }
    }
    // #654: a single-table `GROUP BY` that can't walk an existing index
    // in key order needs a `Sorter` (`compile_grouped_scan`'s own doc
    // comment) to group rows -- reusing
    // `aggregate::group_by_index_ordering`, the exact eligibility check
    // `compile_select_scan`'s dispatch itself gates
    // `try_compile_index_ordered_group_by` on, so this report can never
    // drift from which strategy actually got compiled. Real sqlite3's
    // own wording for the Sorter-backed case (confirmed empirically,
    // sqlite3 3.53.4): `USE TEMP B-TREE FOR GROUP BY`. Joined `GROUP BY`
    // (`compile_joined_grouped_scan`) isn't covered here -- out of scope
    // for #654, which only found this gap via a single-table scenario.
    if from.joins.is_empty() && !select.group_by.is_empty() {
        if let Some(binding) = bindings.first() {
            let index_ordered =
                super::aggregate::group_by_index_ordering(select, &binding.schema, false)?
                    .is_some();
            if !index_ordered {
                rows.push(EqpRow {
                    id: next_id,
                    parent: 0,
                    notused: 0,
                    detail: "USE TEMP B-TREE FOR GROUP BY".to_string(),
                });
            }
        }
    }
    Ok(rows)
}

/// The `SCAN t USING [COVERING] INDEX ix` detail for a single-table
/// aggregate or `GROUP BY` whose compiled program walks an index instead
/// of `Rewind`ing the table (#282): `find_index_only_count`'s
/// `SeekIndexEq` probe (a `SEARCH`), `find_index_only_sum`'s end-to-end
/// `IdxRewind`/`IdxNext` walk, or `group_by_index_ordering`'s
/// key-ordered walk with per-entry rowid lookups. `None` when the arm
/// `Rewind`s the table (plain `SCAN`) or isn't an aggregate arm at all.
fn aggregate_index_walk_detail(
    select: &Select,
    schema: &TableSchema,
    dispatch: ScanDispatch,
    table_display: &str,
    catalog: &[TableSchema],
) -> Result<Option<String>, CodegenError> {
    let index_name = |position: usize| {
        schema
            .indexes
            .get(position)
            .map(|index| index.name.clone())
            .ok_or_else(|| CodegenError::Internal {
                reason: format!("EQP index-walk plan names index #{position} the schema lacks"),
            })
    };
    match dispatch {
        ScanDispatch::Aggregate => {
            match find_index_only_count(select, schema) {
                Some(IndexOnlyCount::TableCount) => Ok(None),
                Some(IndexOnlyCount::IndexProbe { index_position }) => {
                    let index = schema.indexes.get(index_position).ok_or_else(|| {
                        CodegenError::Internal {
                            reason: format!(
                                "EQP count(*) probe names index #{index_position} the schema lacks"
                            ),
                        }
                    })?;
                    Ok(Some(format!(
                        "SEARCH {table_display} USING COVERING INDEX {} ({}=?)",
                        index.name,
                        index
                            .columns
                            .first()
                            .map_or_else(String::new, |c| c.name.clone())
                    )))
                }
                None => match find_index_only_sum(select, schema) {
                    Some(index_position) => Ok(Some(format!(
                        "SCAN {table_display} USING COVERING INDEX {}",
                        index_name(index_position)?
                    ))),
                    None => Ok(aggregate_range_seek_detail(
                        select,
                        schema,
                        table_display,
                        catalog,
                    )),
                },
            }
        }
        ScanDispatch::GroupBy => {
            match super::aggregate::group_by_index_ordering(select, schema, false)? {
                Some((index_position, _forward)) => Ok(Some(format!(
                    "SCAN {table_display} USING INDEX {}",
                    index_name(index_position)?
                ))),
                None => Ok(aggregate_range_seek_detail(
                    select,
                    schema,
                    table_display,
                    catalog,
                )),
            }
        }
        ScanDispatch::Direct | ScanDispatch::Sorted => Ok(None),
    }
}

/// #279: `try_compile_direct_agg_scan`/`compile_grouped_scan` try
/// `try_compile_range_row_seek` before falling back to a full `Rewind`
/// -- report the `SEARCH ... USING INDEX` it emits exactly when its
/// shared eligibility check (`range_row_seek_index_position`) fires,
/// reusing the direct-scan report's own wording.
fn aggregate_range_seek_detail(
    select: &Select,
    schema: &TableSchema,
    table_display: &str,
    catalog: &[TableSchema],
) -> Option<String> {
    let where_expr = select.where_clause.as_ref()?;
    super::range_scan::range_row_seek_index_position(where_expr, schema, catalog)?;
    super::range_scan::find_range_seek_detail(schema, select, table_display, catalog)
}

/// Appends a `SCALAR SUBQUERY n` node (#282) for `inner` -- a scalar
/// subquery operand of a top-level `WHERE` comparison -- with the
/// subquery's own plan grafted underneath. `CORRELATED` iff the
/// compiled scan re-evaluates it per outer row, i.e. exactly when
/// `hoist_uncorrelated_where_subqueries`' `subquery_hoistable` check
/// declines to hoist it (a `FROM`-less or joined subquery, or one
/// referencing the outer table). EQP is explanatory text: a subquery
/// whose `FROM` this can't resolve gets a node without children rather
/// than failing the EXPLAIN.
fn explain_scalar_subquery(
    inner: &Select,
    subquery_no: i32,
    stats_by_table: &std::collections::HashMap<String, crate::codegen::row::planner::Stats>,
    catalog: &[TableSchema],
    rows: &mut Vec<EqpRow>,
    next_id: &mut i32,
) {
    let hoisted = match resolve_subquery_schema(inner, catalog) {
        Ok(Some(schema)) => !subquery_is_correlated(inner, Some(&schema)),
        Ok(None) | Err(_) => false,
    };
    let node_id = *next_id;
    *next_id = next_id.saturating_add(1);
    rows.push(EqpRow {
        id: node_id,
        parent: 0,
        notused: 0,
        detail: if hoisted {
            format!("SCALAR SUBQUERY {subquery_no}")
        } else {
            format!("CORRELATED SCALAR SUBQUERY {subquery_no}")
        },
    });
    let Some(inner_from) = &inner.from else {
        return;
    };
    let inner_schemas: Result<Vec<TableSchema>, CodegenError> = std::iter::once(&inner_from.first)
        .chain(inner_from.joins.iter().map(|j| &j.table))
        .map(|table_ref| resolve_from_table_schema(table_ref, catalog))
        .collect();
    if let Ok(inner_schemas) = inner_schemas {
        if let Ok(child_rows) = explain_query_plan(inner, &inner_schemas, stats_by_table, catalog) {
            graft_child_plan(rows, next_id, node_id, child_rows);
        }
    }
}

/// A `CompoundSelect` arm carries the same core fields as a `Select`
/// (columns/from/where/group-by/having) but none of the whole-statement
/// ones (`with_clause`/further `compound`/`order_by`/`limit`) -- this
/// rebuilds a plain `Select` from an arm so [`explain_query_plan`] can
/// analyze it exactly like a top-level `SELECT`.
fn compound_arm_as_select(arm: &CompoundSelect) -> Select {
    Select {
        with_clause: None,
        distinct: arm.distinct,
        columns: arm.columns.clone(),
        from: arm.from.clone(),
        where_clause: arm.where_clause.clone(),
        group_by: arm.group_by.clone(),
        having: arm.having.clone(),
        compound: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        span: arm.span,
    }
}

/// Oracle sqlite3's own compound-operator EQP text, confirmed
/// empirically (sqlite3 3.51.0): plain `UNION` dedups via an ephemeral
/// index (matching #377/#378's actual codegen), so its EQP text calls
/// that out; `UNION ALL` keeps every row and needs no such step.
fn compound_op_label(op: CompoundOp) -> &'static str {
    match op {
        CompoundOp::Union => "UNION USING TEMP B-TREE",
        CompoundOp::UnionAll => "UNION ALL",
    }
}

/// #539: `explain_query_plan`'s compound-select branch -- one
/// `COMPOUND QUERY` root row, a `LEFT-MOST SUBQUERY` child holding
/// `select`'s own (non-compound) plan, then one `UNION`/`UNION ALL`
/// child per arm holding that arm's plan. Every nested tree's ids are
/// offset so they stay unique across the whole result, the same
/// offsetting scheme the `FROM`-subquery recursion above uses.
fn explain_compound_query_plan(
    select: &Select,
    schemas: &[TableSchema],
    stats_by_table: &std::collections::HashMap<String, crate::codegen::row::planner::Stats>,
    catalog: &[TableSchema],
) -> Result<Vec<EqpRow>, CodegenError> {
    let mut rows = Vec::new();
    let mut next_id: i32 = 0;

    let compound_id = next_id;
    next_id = next_id.saturating_add(1);
    rows.push(EqpRow {
        id: compound_id,
        parent: 0,
        notused: 0,
        detail: "COMPOUND QUERY".to_string(),
    });

    let leftmost_id = next_id;
    next_id = next_id.saturating_add(1);
    rows.push(EqpRow {
        id: leftmost_id,
        parent: compound_id,
        notused: 0,
        detail: "LEFT-MOST SUBQUERY".to_string(),
    });
    let leftmost = Select {
        compound: Vec::new(),
        ..select.clone()
    };
    let leftmost_rows = explain_query_plan(&leftmost, schemas, stats_by_table, catalog)?;
    graft_child_plan(&mut rows, &mut next_id, leftmost_id, leftmost_rows);

    for arm in &select.compound {
        let op_id = next_id;
        next_id = next_id.saturating_add(1);
        rows.push(EqpRow {
            id: op_id,
            parent: compound_id,
            notused: 0,
            detail: compound_op_label(arm.op).to_string(),
        });

        let arm_select = compound_arm_as_select(arm);
        let arm_schemas: Vec<TableSchema> = match &arm.from {
            Some(arm_from) => std::iter::once(&arm_from.first)
                .chain(arm_from.joins.iter().map(|j| &j.table))
                .map(|table_ref| resolve_from_table_schema(table_ref, catalog))
                .collect::<Result<Vec<_>, CodegenError>>()?,
            None => Vec::new(),
        };
        let arm_rows = explain_query_plan(&arm_select, &arm_schemas, stats_by_table, catalog)?;
        graft_child_plan(&mut rows, &mut next_id, op_id, arm_rows);
    }

    Ok(rows)
}

/// Appends `child_rows` (a nested `explain_query_plan` result) into
/// `rows` under `parent_id`, offsetting every id by `*next_id` so ids
/// stay unique across the whole tree -- the same offsetting scheme the
/// `FROM`-subquery recursion in [`explain_query_plan`] uses.
fn graft_child_plan(
    rows: &mut Vec<EqpRow>,
    next_id: &mut i32,
    parent_id: i32,
    child_rows: Vec<EqpRow>,
) {
    let offset = *next_id;
    for mut row in child_rows {
        let was_top_level = row.parent == 0;
        row.id = row.id.saturating_add(offset);
        row.parent = if was_top_level {
            parent_id
        } else {
            row.parent.saturating_add(offset)
        };
        *next_id = (*next_id).max(row.id.saturating_add(1));
        rows.push(row);
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<file-stem>_<line>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#219/#235).

    use crate::codegen::row::{explain_query_plan, IndexSchema, IndexedColumn, TableSchema};
    use crate::parser::ast::Select;
    use crate::parser::row::{parse_select, ParseOutcome};
    use std::collections::HashMap;

    fn table(name: &str, root_page: u32, columns: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            root_page,
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| "INTEGER".to_string()).collect(),
            sql: format!("CREATE TABLE {name} ({})", columns.join(", ")),
            ..Default::default()
        }
    }

    fn with_index(
        mut schema: TableSchema,
        index: &str,
        root_page: u32,
        column: &str,
    ) -> TableSchema {
        schema.indexes.push(IndexSchema {
            name: index.to_string(),
            root_page,
            unique: false,
            columns: vec![IndexedColumn {
                name: column.to_string(),
                desc: false,
                collation: Default::default(),
            }],
        });
        schema
    }

    fn sel(sql: &str) -> Select {
        match parse_select(sql) {
            ParseOutcome::Accepted(select) => *select,
            other => panic!("{sql:?} must parse, got {other:?}"),
        }
    }

    // eqp_230 / eqp_268 / eqp_280 / eqp_290 / eqp_299 / eqp_483 -- `explain_query_plan` details.
    /// `t(a, b)` at root 2 with `ia(a)` at 5 and `ib(b)` at 6; `u(b)` at 3.
    fn eqp_catalog() -> Vec<TableSchema> {
        let t = with_index(
            with_index(table("t", 2, &["a", "b"]), "ia", 5, "a"),
            "ib",
            6,
            "b",
        );
        vec![t, with_index(table("u", 3, &["b"]), "iub", 7, "b")]
    }

    fn eqp_details(sql: &str) -> Vec<String> {
        let catalog = eqp_catalog();
        let select = sel(sql);
        let mut schemas = vec![catalog[0].clone()];
        if select.from.as_ref().is_some_and(|f| !f.joins.is_empty()) {
            schemas.push(catalog[1].clone());
        }
        explain_query_plan(&select, &schemas, &HashMap::new(), &catalog)
            .unwrap()
            .into_iter()
            .map(|r| r.detail)
            .collect()
    }

    // eqp_280 / eqp_290: `level == 0 && direct_scan && access.is_none() && covering.is_none()`
    fn range_seek_detail_present(d: &[String]) -> bool {
        d[0].contains("SEARCH t USING INDEX ib")
    }

    // eqp_485: `from.joins.is_empty() && !select.group_by.is_empty()`
    const TEMP_BTREE: &str = "USE TEMP B-TREE FOR GROUP BY";

    // eqp_268: `level == 0 && direct_scan && access.is_none()` (covering index)
    #[test]
    fn mcdc__eqp_268__v1_outer_table_without_a_seek_is_a_scan() {
        let d = eqp_details("SELECT a FROM t WHERE a + b = 1");
        assert!(d[0].starts_with("SCAN t"), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_268__v2_outer_table_with_a_rowid_seek_is_a_search() {
        let d = eqp_details("SELECT a FROM t WHERE rowid = 1");
        assert!(
            d[0].contains("SEARCH t") && d[0].contains("rowid=?"),
            "{d:?}"
        );
    }

    #[test]
    fn mcdc__eqp_268__v3_inner_join_level_reports_its_own_access() {
        let d = eqp_details("SELECT a FROM t JOIN u ON u.b = t.a");
        assert!(d.iter().any(|x| x.contains('u')), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_280__v1_all_true_reaches_the_range_seek_report() {
        let d = eqp_details("SELECT a, b FROM t WHERE b BETWEEN 1 AND 5");
        assert!(range_seek_detail_present(&d), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_280__v2_inner_level_never_reports_a_range_seek() {
        // Only the outermost table's WHERE is consulted for a range seek, so
        // the inner level (`u`, indexed on `b`) reports its join access, never
        // a `b>? AND b<?` range.
        let d = eqp_details("SELECT a FROM t JOIN u ON u.b = t.a WHERE a + b = 1");
        assert!(d.len() == 2 && !d[1].contains("b>?"), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_280__v3_rowid_seek_takes_precedence() {
        let d = eqp_details("SELECT a, b FROM t WHERE rowid = 1");
        assert!(
            d[0].contains("rowid=?") && !range_seek_detail_present(&d),
            "{d:?}"
        );
    }

    #[test]
    fn mcdc__eqp_280__v4_covering_index_takes_precedence() {
        let d = eqp_details("SELECT a FROM t WHERE a = 1");
        assert!(d[0].contains("COVERING INDEX ia"), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_290__v1_all_true_reaches_the_range_seek_report() {
        let d = eqp_details("SELECT a, b FROM t WHERE b BETWEEN 1 AND 5");
        assert!(range_seek_detail_present(&d), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_290__v2_inner_level_never_reports_a_range_seek() {
        // Only the outermost table's WHERE is consulted for a range seek, so
        // the inner level (`u`, indexed on `b`) reports its join access, never
        // a `b>? AND b<?` range.
        let d = eqp_details("SELECT a FROM t JOIN u ON u.b = t.a WHERE a + b = 1");
        assert!(d.len() == 2 && !d[1].contains("b>?"), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_290__v3_rowid_seek_takes_precedence() {
        let d = eqp_details("SELECT a, b FROM t WHERE rowid = 1");
        assert!(
            d[0].contains("rowid=?") && !range_seek_detail_present(&d),
            "{d:?}"
        );
    }

    #[test]
    fn mcdc__eqp_290__v4_covering_index_takes_precedence() {
        let d = eqp_details("SELECT a FROM t WHERE a = 1");
        assert!(d[0].contains("COVERING INDEX ia"), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_485__v1_single_table_group_by_without_an_index_uses_a_temp_btree() {
        let d = eqp_details("SELECT a + b, count(*) FROM t GROUP BY a + b");
        assert!(d.iter().any(|x| x == TEMP_BTREE), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_485__v2_joined_group_by_is_not_reported() {
        let d = eqp_details("SELECT t.a, count(*) FROM t JOIN u ON u.b = t.a GROUP BY t.a");
        assert!(!d.iter().any(|x| x == TEMP_BTREE), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_485__v3_single_table_without_group_by_is_not_reported() {
        let d = eqp_details("SELECT a FROM t");
        assert!(!d.iter().any(|x| x == TEMP_BTREE), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_268__v4_aggregate_never_reports_a_covering_index_seek() {
        // `direct_scan` false: an aggregate never takes the covering-index
        // path. Since #298 its `WHERE a = 1` does seek -- through the
        // #279 row seek, as `SEARCH ... USING INDEX`, never `COVERING`.
        let d = eqp_details("SELECT sum(b) FROM t WHERE a = 1");
        assert_eq!(d[0], "SEARCH t USING INDEX ia (a=?)", "{d:?}");
        assert!(!d[0].contains("COVERING"), "{d:?}");
    }

    // `IN` is a direct-scan range shape but not a #279 row-seek shape, so
    // an aggregate over it is the one range predicate that must still SCAN.
    #[test]
    fn mcdc__eqp_280__v5_aggregate_never_reports_the_direct_scans_skip_scan() {
        let d = eqp_details("SELECT sum(a) FROM t WHERE b IN (1, 2)");
        assert_eq!(d[0], "SCAN t", "{d:?}");
    }

    #[test]
    fn mcdc__eqp_290__v5_aggregate_never_reports_the_direct_scans_in_list_seek() {
        let d = eqp_details("SELECT count(*) FROM t WHERE b IN (1, 2)");
        assert_eq!(d[0], "SCAN t", "{d:?}");
    }

    // eqp_230: `level == 0 && direct_scan` (rowid seek)
    #[test]
    fn mcdc__eqp_230__v1_direct_outer_table_reports_its_rowid_seek() {
        let d = eqp_details("SELECT a FROM t WHERE rowid = 5");
        assert!(d[0].contains("rowid=?"), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_230__v2_inner_level_uses_join_access_not_the_where_clause() {
        let d = eqp_details("SELECT t.a FROM t JOIN u ON u.b = t.a WHERE t.rowid = 5");
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(!d[1].contains("rowid=?"), "{d:?}");
    }

    #[test]
    fn mcdc__eqp_230__v3_aggregate_outer_table_scans_despite_a_rowid_equality() {
        let d = eqp_details("SELECT count(*) FROM t WHERE rowid = 5");
        assert_eq!(d[0], "SCAN t", "{d:?}");
    }

    // eqp_300: `level == 0 && from.joins.is_empty()` (aggregate index walk)
    #[test]
    fn mcdc__eqp_300__v1_single_table_index_only_sum_reports_the_index_walk() {
        let d = eqp_details("SELECT sum(a) FROM t");
        assert_eq!(d[0], "SCAN t USING COVERING INDEX ia", "{d:?}");
    }

    #[test]
    fn mcdc__eqp_300__v2_joined_outer_table_is_not_an_index_walk() {
        let d = eqp_details("SELECT sum(t.a) FROM t JOIN u ON u.b = t.a");
        assert_eq!(d[0], "SCAN t", "{d:?}");
    }

    #[test]
    fn mcdc__eqp_300__v3_inner_level_is_not_an_index_walk() {
        let d = eqp_details("SELECT sum(t.a) FROM t JOIN u ON u.b = t.a");
        assert!(!d[1].contains("COVERING INDEX"), "{d:?}");
    }

    // ---- #282: the plan must describe the program that was compiled ----

    use crate::codegen::row::compile_select_with_catalog;
    use crate::vm::row::{Opcode, Program};

    fn compile(sql: &str) -> Program {
        let catalog = eqp_catalog();
        compile_select_with_catalog(&sel(sql), &catalog[0], &catalog).unwrap()
    }

    fn has(program: &Program, opcode: Opcode) -> bool {
        program.instructions.iter().any(|i| i.opcode == opcode)
    }

    /// Whether `program` seeks (a `SEARCH`) rather than walking a b-tree
    /// end to end (a `SCAN`): a `SeekIndex*` probe, or a `SeekRowid` that
    /// isn't the per-entry rowid lookup of an `IdxRewind`/`Rewind` walk.
    fn program_seeks(program: &Program) -> bool {
        has(program, Opcode::SeekIndexEq)
            || has(program, Opcode::SeekIndexGE)
            || (has(program, Opcode::SeekRowid)
                && !has(program, Opcode::Rewind)
                && !has(program, Opcode::IdxRewind))
    }

    /// Acceptance criterion 1 of #282: for every single-table scan
    /// strategy `compile_select_scan` dispatches to, the outermost EQP
    /// row says `SEARCH` iff the compiled program seeks and `SCAN` iff
    /// it `Rewind`s (or `Count`s) -- `find_range_seek_detail` and friends
    /// used to fire for aggregate/`GROUP BY`/`ORDER BY` shapes whose
    /// programs never reach `compile_direct_scan`.
    #[test]
    fn eqp_outer_row_matches_the_compiled_programs_access_path() {
        let shapes = [
            // compile_direct_scan
            "SELECT a FROM t WHERE a + b = 1",
            "SELECT a FROM t WHERE rowid = 5",
            "SELECT a FROM t WHERE a = 1",
            "SELECT a FROM t WHERE b > 5",
            "SELECT a FROM t WHERE b BETWEEN 1 AND 5",
            "SELECT a FROM t WHERE b IN (1, 2)",
            // sorted scans
            "SELECT a FROM t WHERE b > 5 ORDER BY a",
            "SELECT a FROM t ORDER BY a",
            // implicit-group aggregates
            "SELECT count(*) FROM t",
            "SELECT count(*) FROM t WHERE b = 5",
            "SELECT count(*) FROM t WHERE b > 5",
            "SELECT count(*) FROM t WHERE b BETWEEN 1 AND 5",
            "SELECT count(*) FROM t WHERE b IN (1, 2)",
            "SELECT sum(a) FROM t",
            "SELECT sum(a) FROM t WHERE b > 5",
            "SELECT count(*) FROM t WHERE b > (SELECT avg(b) FROM t)",
            "SELECT count(*) FROM t WHERE b >= 5 AND a = 1",
            "SELECT sum(a) FROM t WHERE b < 5",
            "SELECT count(DISTINCT a) FROM t WHERE b > 5",
            // GROUP BY
            "SELECT a, count(*) FROM t GROUP BY a",
            "SELECT a, count(*) FROM t WHERE b > 5 GROUP BY a",
            "SELECT a, count(*) FROM t WHERE b BETWEEN 1 AND 5 GROUP BY a",
            "SELECT a, count(*) FROM t WHERE b IN (1, 2) GROUP BY a",
        ];
        for sql in shapes {
            let program = compile(sql);
            let d = eqp_details(sql);
            let outer = d.first().expect("at least one EQP row");
            let says_search = outer.starts_with("SEARCH t");
            let says_scan = outer.starts_with("SCAN t");
            assert!(says_search || says_scan, "{sql}: {outer}");
            assert_eq!(
                says_search,
                program_seeks(&program),
                "{sql}: EQP says {outer:?} but the program's opcodes are {:?}",
                program
                    .instructions
                    .iter()
                    .map(|i| i.opcode)
                    .collect::<Vec<_>>()
            );
        }
    }

    /// The issue's own reproduction: with #279's aggregate range seek
    /// compiled, EQP reports the seek; the `IN` shape the row seek doesn't
    /// recognize stays a scan.
    #[test]
    fn aggregate_over_range_predicate_reports_the_compiled_seek() {
        let d = eqp_details("SELECT count(*) FROM t WHERE b > 5");
        assert_eq!(d[0], "SEARCH t USING INDEX ib (b>?)", "{d:?}");
        let d = eqp_details("SELECT sum(a) FROM t WHERE b BETWEEN 1 AND 5");
        assert_eq!(d[0], "SEARCH t USING INDEX ib (b>? AND b<?)", "{d:?}");
        let d = eqp_details("SELECT a, count(*) FROM t WHERE b > 5 GROUP BY a");
        assert_eq!(d[0], "SEARCH t USING INDEX ib (b>?)", "{d:?}");
        let d = eqp_details("SELECT count(*) FROM t WHERE b IN (1, 2)");
        assert_eq!(d[0], "SCAN t", "{d:?}");
    }

    #[test]
    fn index_only_aggregate_fast_paths_report_their_index() {
        let d = eqp_details("SELECT count(*) FROM t WHERE b = 5");
        assert_eq!(d[0], "SEARCH t USING COVERING INDEX ib (b=?)", "{d:?}");
        let d = eqp_details("SELECT sum(a) FROM t");
        assert_eq!(d[0], "SCAN t USING COVERING INDEX ia", "{d:?}");
        let d = eqp_details("SELECT a, count(*) FROM t GROUP BY a");
        assert_eq!(d[0], "SCAN t USING INDEX ia", "{d:?}");
    }

    #[test]
    fn order_by_with_range_predicate_reports_a_scan() {
        let d = eqp_details("SELECT a FROM t WHERE b > 5 ORDER BY a");
        assert_eq!(d[0], "SCAN t", "{d:?}");
    }

    /// Acceptance criterion 2 of #282: a scalar subquery is its own EQP
    /// node, with its plan nested underneath (sqlite3's own shape).
    #[test]
    fn scalar_subquery_in_where_is_its_own_node() {
        let catalog = eqp_catalog();
        let select = sel("SELECT count(*) FROM t WHERE b > (SELECT avg(b) FROM t)");
        let rows = explain_query_plan(&select, &catalog[..1], &HashMap::new(), &catalog).unwrap();
        let details: Vec<&str> = rows.iter().map(|r| r.detail.as_str()).collect();
        // The hoisted `avg(b)` takes `try_compile_index_only_sum`'s
        // `IdxRewind` walk of `ib` -- and its nested plan says so. #280
        // additionally makes the outer `b > (SELECT avg(b) FROM t)` a
        // real range seek against `ib` (the subquery bound is
        // uncorrelated, hence loop-constant), so the outer plan is a
        // `SEARCH`, not a `SCAN`.
        let program = compile("SELECT count(*) FROM t WHERE b > (SELECT avg(b) FROM t)");
        assert!(has(&program, Opcode::IdxRewind), "{program:?}");
        assert!(has(&program, Opcode::SeekIndexGE), "{program:?}");
        assert_eq!(
            details,
            [
                "SEARCH t USING INDEX ib (b>?)",
                "SCALAR SUBQUERY 1",
                "SCAN t USING COVERING INDEX ib"
            ],
            "{rows:?}"
        );
        assert_eq!(rows[1].parent, 0);
        assert_eq!(rows[2].parent, rows[1].id, "{rows:?}");
        assert_ne!(rows[2].id, rows[1].id);
    }

    #[test]
    fn correlated_scalar_subquery_is_labelled_correlated() {
        let d = eqp_details("SELECT a FROM t WHERE b = (SELECT max(u.b) FROM u WHERE u.b = t.a)");
        assert_eq!(d[1], "CORRELATED SCALAR SUBQUERY 1", "{d:?}");
        assert_eq!(d[2], "SCAN u", "{d:?}");
    }
}
