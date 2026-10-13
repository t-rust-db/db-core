// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `ORDER BY` and `DISTINCT` over an aggregate's output (#597): `GROUP
//! BY ... ORDER BY`, `SELECT COUNT(*) ... ORDER BY 1`, `SELECT DISTINCT
//! ... GROUP BY`, as sqlite3 runs them.
//!
//! The aggregate runs as usual, on a copy of the query without `ORDER
//! BY`, `LIMIT` and `DISTINCT`. Each output row then goes through this
//! step instead of straight to the sink:
//!
//! 1. `DISTINCT`: skipped if an identical row (on the visible columns,
//!    under their collations) was seen already;
//! 2. `ORDER BY`: buffered in a sorter, else emitted;
//! 3. after the aggregate, the sorter is read back in order and each row
//!    emitted; `LIMIT`/`OFFSET` apply at emission.
//!
//! An `ORDER BY` term is, as in sqlite3, a result column position
//! (`ORDER BY 2`), a result column alias, or any other expression, which
//! is evaluated in the aggregate's context as a hidden extra result
//! column (`ORDER BY SUM(n)` when `SUM(n)` is not selected). Hidden
//! columns are sorted on and never emitted.

use super::limit_scan::{compile_limit_setup, emit_limit_guard, emit_offset_guard, LimitState};
use super::projection::{emit_dedup_check, result_column_collations, result_columns};
use super::*;
use std::rc::Rc;

/// Whether `select` aggregates (`GROUP BY`, or an aggregate call) and
/// then orders or deduplicates the result. Not when the groups can be
/// formed in ORDER BY's order to begin with ([`group_order`], #607).
pub(super) fn needs_post_group(select: &Select, schema: &TableSchema) -> bool {
    let aggregates = !select.group_by.is_empty() || select_has_aggregate(select);
    let distinct = select.distinct == Some(Distinctness::Distinct);
    aggregates && (!select.order_by.is_empty() || distinct) && group_order(select, schema).is_none()
}

/// #607: each GROUP BY key's direction (`true` = DESC) when ORDER BY
/// lists exactly the GROUP BY keys, in order, each a plain column
/// (written out, or by result position or alias) with the default NULLS
/// placement, and there is no DISTINCT. The groups are then formed in
/// that order and need no second sort, as in sqlite3. `None` otherwise.
pub(super) fn group_order(select: &Select, schema: &TableSchema) -> Option<Vec<bool>> {
    if select.group_by.is_empty()
        || select.order_by.len() != select.group_by.len()
        || select.distinct == Some(Distinctness::Distinct)
    {
        return None;
    }
    select
        .order_by
        .iter()
        .zip(&select.group_by)
        .map(|(term, key)| {
            if term.nulls_last.is_some() {
                return None;
            }
            let ordered = ordered_column(&term.expr, select, schema)?;
            let grouped = bare_column(key, schema)?;
            (ordered == grouped).then_some(term.desc.unwrap_or(false))
        })
        .collect()
}

/// The schema column a bare (possibly parenthesized) column reference
/// names.
fn bare_column(expr: &Expr, schema: &TableSchema) -> Option<usize> {
    let mut inner = expr;
    while let ExprKind::Paren(e) = &inner.kind {
        inner = e;
    }
    match &inner.kind {
        ExprKind::Column { name, .. } => column_index(schema, name),
        _ => None,
    }
}

/// The schema column an ORDER BY term names: by result position, by a
/// result alias, or directly.
fn ordered_column(expr: &Expr, select: &Select, schema: &TableSchema) -> Option<usize> {
    let result_expr = |column: Option<&ResultColumn>| match column {
        Some(ResultColumn::Expr { expr, .. }) => bare_column(expr, schema),
        _ => None,
    };
    match &expr.kind {
        ExprKind::Literal(Literal::Integer(n)) => {
            let position = usize::try_from(*n).ok()?.checked_sub(1)?;
            result_expr(select.columns.get(position))
        }
        ExprKind::Column {
            table: None, name, ..
        } => {
            let aliased = select.columns.iter().find(|c| {
                matches!(c, ResultColumn::Expr { alias: Some(a), .. } if a.eq_ignore_ascii_case(name))
            });
            match aliased {
                Some(column) => result_expr(Some(column)),
                None => bare_column(expr, schema),
            }
        }
        _ => bare_column(expr, schema),
    }
}

/// The output column an `ORDER BY` term names by position or alias;
/// `star` is how many columns a `*` expands to.
fn named_output(
    expr: &Expr,
    select: &Select,
    visible: usize,
    star: usize,
) -> Result<Option<usize>, CodegenError> {
    match &order_by::strip_collate(expr).kind {
        ExprKind::Literal(Literal::Integer(n)) => {
            let index = usize::try_from(*n)
                .ok()
                .and_then(|k| k.checked_sub(1))
                .filter(|k| *k < visible)
                .ok_or_else(|| CodegenError::Unsupported {
                    reason: format!(
                        "ORDER BY position {n} is out of range for a {visible}-column result set"
                    ),
                })?;
            Ok(Some(index))
        }
        ExprKind::Column {
            table: None, name, ..
        } => {
            // Positions of the visible columns, `*` counting as its
            // expansion, so an alias's index is its output position.
            let mut position = 0usize;
            for column in &select.columns {
                match column {
                    ResultColumn::Expr {
                        alias: Some(alias), ..
                    } if alias.eq_ignore_ascii_case(name) => return Ok(Some(position)),
                    ResultColumn::Expr { .. } => position = position.saturating_add(1),
                    // A single-table SELECT: `*` is the whole schema.
                    ResultColumn::Star | ResultColumn::TableStar { .. } => {
                        position = position.saturating_add(star);
                    }
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// The aggregate [`compile_post_group`] runs for `select`: `select`
/// without `ORDER BY`, `LIMIT` and `DISTINCT`, plus a hidden result
/// column for each `ORDER BY` term that names no output column. Also each
/// term's column index in that aggregate's rows. Shared with `EXPLAIN
/// QUERY PLAN`, which must describe the same aggregate.
pub(super) fn aggregate_query(
    select: &Select,
    schema: &TableSchema,
) -> Result<(Select, Vec<usize>), CodegenError> {
    let visible = result_columns(select, schema).len();
    let mut columns = select.columns.clone();
    let mut hidden = 0usize;
    let mut indices = Vec::with_capacity(select.order_by.len());
    for term in &select.order_by {
        let index = match named_output(&term.expr, select, visible, schema.columns.len())? {
            Some(index) => index,
            None => {
                columns.push(ResultColumn::Expr {
                    expr: term.expr.clone(),
                    alias: None,
                });
                hidden = hidden.saturating_add(1);
                visible.saturating_add(hidden).saturating_sub(1)
            }
        };
        indices.push(index);
    }
    let inner = Select {
        columns,
        order_by: Vec::new(),
        limit: None,
        distinct: None,
        ..select.clone()
    };
    Ok((inner, indices))
}

/// Compiles `select` (see [`needs_post_group`]) into `em`, emitting its
/// rows to `sink` in order.
#[allow(clippy::too_many_arguments)]
pub(super) fn compile_post_group<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    schema: &Rc<TableSchema>,
    cursors: ScanCursors,
    end_label: Label,
    catalog: &[TableSchema],
    stats: &crate::codegen::row::planner::Stats,
    sink: &mut F,
) -> Result<(), CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, i32) -> Result<(), CodegenError>,
{
    let outputs = result_columns(select, schema);
    let visible = outputs.len();
    let collations = result_column_collations(schema, &outputs);
    let scope = Scope::single_shared(schema, cursors.table).with_catalog(catalog);
    let (inner, indices) = aggregate_query(select, schema)?;

    let keys: Vec<SortKeyColumn> = select
        .order_by
        .iter()
        .zip(indices)
        .map(|(term, index)| {
            let descending = term.desc.unwrap_or(false);
            let collation = collation_of(&term.expr)
                .or_else(|| {
                    (index < visible)
                        .then(|| collations.get(index).copied())
                        .flatten()
                })
                .or_else(|| expr_collation(&scope, &term.expr))
                .unwrap_or(Collation::Binary);
            SortKeyColumn {
                index,
                descending,
                collation,
                nulls_first: term.nulls_last.map_or(!descending, |last| !last),
            }
        })
        .collect();
    // An aggregate without GROUP BY is one row: its ORDER BY (checked
    // above) has nothing to sort, as in sqlite3.
    let ordered = !keys.is_empty() && !select.group_by.is_empty();
    // Cursors of this program's own (#597), clear of the scan's.
    let (post_sort, post_pseudo, post_distinct) =
        (reg.alloc_cursor(), reg.alloc_cursor(), reg.alloc_cursor());
    let distinct = select.distinct == Some(Distinctness::Distinct);

    let limit: Option<LimitState> = compile_limit_setup(em, reg, &scope, select)?;
    if ordered {
        em.emit(Instruction::with_p4(
            Opcode::SorterOpen,
            post_sort,
            0,
            0,
            P4::SortKey(keys),
        ));
    }
    if distinct {
        em.emit(Instruction::new(Opcode::OpenEphemeral, post_distinct, 0, 0));
    }

    let visible_i32 = i32::try_from(visible).unwrap_or(0);
    let mut each_row = |em: &mut Emitter, reg: &mut RegAlloc, first: i32, count: i32| {
        let skip = em.new_label();
        if distinct {
            emit_dedup_check(em, post_distinct, first, collations.clone(), skip);
        }
        if ordered {
            let record = reg.alloc();
            em.emit(Instruction::new(Opcode::MakeRecord, first, count, record));
            em.emit(Instruction::new(Opcode::SorterInsert, post_sort, record, 0));
        } else {
            if let Some(limit) = &limit {
                emit_offset_guard(em, limit, skip);
                emit_limit_guard(em, limit, end_label);
            }
            sink(em, reg, first, visible_i32)?;
        }
        em.place(skip);
        Ok(())
    };
    let scanned = em.new_label();
    super::entry::compile_select_scan_body(
        em,
        reg,
        &inner,
        schema,
        cursors,
        scanned,
        catalog,
        stats,
        &mut each_row,
    )?;
    em.place(scanned);

    if ordered {
        let sort = em.emit(Instruction::new(Opcode::SorterSort, post_sort, 0, 0));
        em.patch_p2(sort, end_label);
        let sorted_loop = em.new_label();
        em.place(sorted_loop);
        let data = reg.alloc();
        em.emit(Instruction::new(Opcode::SorterData, post_sort, data, 0));
        em.emit(Instruction::new(Opcode::OpenPseudo, post_pseudo, data, 0));
        let row_skip = em.new_label();
        if let Some(limit) = &limit {
            emit_offset_guard(em, limit, row_skip);
            emit_limit_guard(em, limit, end_label);
        }
        let first = reg.peek();
        for i in 0..visible_i32 {
            let r = reg.alloc();
            em.emit(Instruction::new(Opcode::Column, post_pseudo, i, r));
        }
        sink(em, reg, first, visible_i32)?;
        em.place(row_skip);
        let next = em.emit(Instruction::new(Opcode::SorterNext, post_sort, 0, 0));
        em.patch_p2(next, sorted_loop);
    }
    Ok(())
}
