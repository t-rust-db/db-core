//! `SELECT` codegen -- see `super`'s module doc.
//!
//! **Single-table scan + projection + `WHERE` + `LIMIT`** (db-core#92),
//! **a single `INNER`/`LEFT`/`FULL` equi-join and `ORDER BY` via the
//! existing single-key sorter** (db-core#102/#101, both without any
//! stats/access-path cost model), and `GROUP BY`/`HAVING`/aggregation
//! via [`super::aggregate`] (db-core#93). The join-order/access-path chooser and real `planner::Stats`
//! (needs a working `ANALYZE` VM implementation, #116) to #117; N-way
//! joins and multi-table catalogs to #118 (no consumer needs them yet);
//! `DISTINCT` is not yet supported.

use super::value::emit_column_read;
use super::{
    CodegenError, CondTargets, Emitter, Label, RegAlloc, Result, Scope, TableSchema, Target,
};
use crate::expr::{BinOp, Expr, Join, JoinKind, Query, SelectItem};
use crate::vm::row::{Collation, Instruction, Opcode, Program, SortKeyColumn, P4};

/// Compiles `query` (a single-table `SELECT`, no `JOIN`) against
/// `schema`, scanning the pre-wired cursor slot `cursor`. A query with a
/// `JOIN` must use [`compile_select_join`] instead, since resolving it
/// needs a second pre-wired cursor this signature has no room for.
pub fn compile_select(schema: &TableSchema, cursor: i32, query: &Query) -> Result<Program> {
    if !query.joins.is_empty() {
        return Err(CodegenError::Unsupported {
            reason: "SELECT with a JOIN must be compiled via compile_select_join".to_string(),
        });
    }
    compile_select_inner(schema, cursor, None, query)
}

/// Compiles `query` (a `SELECT` with a single `INNER`/`LEFT`/`FULL`
/// equi-join) against `schema`/`cursor` (the `FROM` table) joined to
/// `right_schema`/`right_cursor` (`query.joins[0].table`), both
/// pre-wired cursor slots. `Right`/`Cross` joins, more than one `JOIN`,
/// and any join-order/access-path cost model are deferred to #117
/// (needs `planner::Stats`, which db-core doesn't have yet); N-way
/// joins and a multi-table catalog `Scope` to #118 (no consumer needs
/// them yet).
pub fn compile_select_join(
    schema: &TableSchema,
    cursor: i32,
    right_schema: &TableSchema,
    right_cursor: i32,
    query: &Query,
) -> Result<Program> {
    compile_select_inner(schema, cursor, Some((right_schema, right_cursor)), query)
}

fn compile_select_inner(
    schema: &TableSchema,
    cursor: i32,
    right: Option<(&TableSchema, i32)>,
    query: &Query,
) -> Result<Program> {
    if query.distinct {
        return Err(CodegenError::Unsupported {
            reason: "DISTINCT is not yet supported".to_string(),
        });
    }
    if query.joins.len() > 1 {
        return Err(CodegenError::Unsupported {
            reason: "only a single JOIN is supported; N-way joins are deferred to #101".to_string(),
        });
    }
    let join = query.joins.first();
    match (join, right) {
        (Some(join), Some(_)) => {
            if !matches!(join.kind, JoinKind::Inner | JoinKind::Left | JoinKind::Full) {
                return Err(CodegenError::Unsupported {
                    reason: "only INNER/LEFT/FULL JOIN are supported; the join-order/access-path \
chooser is deferred to #117, N-way joins to #118"
                        .to_string(),
                });
            }
        }
        (Some(_), None) => {
            return Err(CodegenError::Unsupported {
                reason: "SELECT with a JOIN must be compiled via compile_select_join".to_string(),
            });
        }
        (None, Some(_)) => {
            return Err(CodegenError::Unsupported {
                reason: "a right cursor was supplied but the query has no JOIN".to_string(),
            });
        }
        (None, None) => {}
    }

    let scope = match right {
        Some((right_schema, right_cursor)) => {
            Scope::join(schema.clone(), cursor, right_schema.clone(), right_cursor)
        }
        None => Scope::single(schema.clone(), cursor),
    };

    if !query.group_by.is_empty() || super::aggregate::query_has_aggregate(query) {
        return compile_aggregate_select(schema, cursor, right, query, &scope);
    }

    let mut columns = Vec::with_capacity(query.columns.len());
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => columns.push(name.clone()),
            SelectItem::Star => {
                columns.extend(schema.columns.iter().cloned());
                if let Some((right_schema, _)) = right {
                    columns.extend(
                        right_schema
                            .columns
                            .iter()
                            .map(|c| format!("{}.{c}", right_schema.name)),
                    );
                }
            }
            SelectItem::Agg(..) | SelectItem::Window(_) => {
                return Err(CodegenError::Unsupported {
                    reason: "aggregate/window SELECT items are deferred to #93".to_string(),
                });
            }
        }
    }

    // When there's an `ORDER BY`, rows are buffered into a sorter instead
    // of being emitted directly, and `LIMIT` applies to the sorted
    // output rather than scan order -- see `sort_key`/`output_count`
    // below and the drain loop at the end of this function. `columns`
    // gains the sort key as an extra, trailing record column when it
    // isn't already part of the projection; `output_count` stays at the
    // original projection width so the drain loop never emits it.
    let output_count = columns.len();
    let sort_key = query.order_by.as_ref().map(|order_by| {
        let index = columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&order_by.column))
            .unwrap_or_else(|| {
                columns.push(order_by.column.clone());
                columns.len() - 1
            });
        SortKeyColumn {
            index,
            descending: order_by.descending,
            collation: Collation::Binary,
            nulls_first: false,
        }
    });

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    let limit_reg = match query.limit {
        Some(limit) => {
            let p1 = i32::try_from(limit).map_err(|_| CodegenError::Unsupported {
                reason: format!("LIMIT {limit} does not fit in a p1 operand"),
            })?;
            let r = reg.alloc();
            em.emit(Instruction::new(Opcode::Integer, p1, r, 0));
            Some(r)
        }
        None => None,
    };

    // The sorter cursor uses a slot past every cursor the caller wired
    // up -- `Opcode::SorterOpen` opens it itself at runtime, so it needs
    // no caller-side wiring, just an id that can't collide.
    let sorter_cursor = right.map_or(cursor, |(_, c)| cursor.max(c)) + 1;
    if let Some(key) = sort_key {
        em.emit(Instruction::with_p4(
            Opcode::SorterOpen,
            sorter_cursor,
            0,
            0,
            P4::SortKey(vec![key]),
        ));
    }

    let end_label = em.new_label();
    let outer_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, cursor, 0, 0));
    em.patch_p2(outer_rewind_addr, end_label);

    let outer_loop_start = em.new_label();
    em.place(outer_loop_start);
    let outer_row_skip = em.new_label();

    // `FULL OUTER`'s limit-guard target must skip *both* passes, not
    // just this outer scan -- `end_label` here is where pass two (if
    // any) begins, not the true program end, so it can't double as the
    // limit target the way it does for every other join kind (where
    // there is no second pass, and the two labels are the same point).
    let final_label = if matches!(join.map(|j| j.kind), Some(JoinKind::Full)) {
        em.new_label()
    } else {
        end_label
    };

    match (join, right) {
        (Some(join), Some((_, right_cursor))) => {
            compile_join_body(
                &mut em,
                &mut reg,
                &scope,
                join,
                right_cursor,
                query,
                &columns,
                sort_key,
                sorter_cursor,
                limit_reg,
                final_label,
                outer_row_skip,
            )?;
        }
        _ => {
            if let Some(where_expr) = &query.where_clause {
                super::compile_cond(
                    &mut em,
                    &mut reg,
                    &scope,
                    where_expr,
                    // `WHERE` is where SQL's three-valued logic collapses
                    // to two: an unknown predicate excludes the row
                    // exactly like a false one.
                    CondTargets::null_is_false(Target::Fallthrough, Target::Jump(outer_row_skip)),
                )?;
            }
            emit_row(
                &mut em,
                &mut reg,
                &scope,
                &columns,
                None,
                sort_key,
                sorter_cursor,
                limit_reg,
                end_label,
            )?;
        }
    }

    em.place(outer_row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, cursor, 0, 0));
    em.patch_p2(next_addr, outer_loop_start);

    em.place(end_label);

    if let (Some(join), Some((_, right_cursor))) = (join, right) {
        if join.kind == JoinKind::Full {
            compile_full_outer_right_pass(
                &mut em,
                &mut reg,
                &scope,
                join,
                cursor,
                right_cursor,
                &columns,
                sort_key,
                sorter_cursor,
                limit_reg,
                final_label,
            )?;
        }
    }

    if sort_key.is_some() {
        let sorter_end_label = em.new_label();
        let sort_addr = em.emit(Instruction::new(Opcode::SorterSort, sorter_cursor, 0, 0));
        em.patch_p2(sort_addr, sorter_end_label);

        let sorter_loop_start = em.new_label();
        em.place(sorter_loop_start);

        if let Some(limit_reg) = limit_reg {
            emit_limit_guard(&mut em, limit_reg, sorter_end_label);
        }

        let mut regs = Vec::with_capacity(output_count);
        for idx in 0..output_count {
            let r = reg.alloc();
            em.emit(Instruction::new(
                Opcode::Column,
                sorter_cursor,
                i32::try_from(idx).map_err(|_| CodegenError::Unsupported {
                    reason: format!("column index {idx} does not fit in a p2 operand"),
                })?,
                r,
            ));
            regs.push(r);
        }
        if output_count > 0 {
            em.emit(Instruction::new(
                Opcode::ResultRow,
                regs[0],
                i32::try_from(output_count).map_err(|_| CodegenError::Unsupported {
                    reason: format!(
                        "SELECT list of {output_count} columns does not fit in a p2 operand"
                    ),
                })?,
                0,
            ));
        } else {
            em.emit(Instruction::new(Opcode::ResultRow, reg.alloc(), 0, 0));
        }

        let sorter_next_addr = em.emit(Instruction::new(Opcode::SorterNext, sorter_cursor, 0, 0));
        em.patch_p2(sorter_next_addr, sorter_loop_start);

        em.place(sorter_end_label);
    }

    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));

    Ok(em.finish())
}

/// Compiles a `SELECT` that aggregates -- an explicit `GROUP BY`, or a
/// whole-table aggregate with no `GROUP BY` key at all -- through
/// [`super::aggregate`] (db-core#93).
///
/// `ORDER BY` and `DISTINCT` combined with aggregation are rejected
/// rather than composed, matching sqlite-rs's own documented
/// simplification for this slice.
fn compile_aggregate_select(
    schema: &TableSchema,
    cursor: i32,
    right: Option<(&TableSchema, i32)>,
    query: &Query,
    scope: &Scope,
) -> Result<Program> {
    if query.order_by.is_some() {
        return Err(CodegenError::Unsupported {
            reason: "ORDER BY combined with GROUP BY/aggregation is not yet supported".to_string(),
        });
    }

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();
    let limit_reg = compile_limit_setup(&mut em, &mut reg, query)?;
    let end_label = em.new_label();

    let highest = right.map_or(cursor, |(_, c)| cursor.max(c));
    let cursors = super::aggregate::ScanCursors::past(cursor, highest);
    let mut sink = |em: &mut Emitter, reg: &mut RegAlloc, first: i32, count: usize| -> Result<()> {
        emit_result_row(em, reg, first, count)
    };

    match right {
        Some((_, right_cursor)) => super::aggregate::compile_joined_grouped_scan(
            &mut em,
            &mut reg,
            query,
            scope,
            cursors,
            right_cursor,
            limit_reg,
            end_label,
            &mut sink,
        )?,
        None => super::aggregate::compile_aggregate_scan(
            &mut em, &mut reg, query, schema, cursors, limit_reg, end_label, &mut sink,
        )?,
    }

    em.place(end_label);
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// Emits the `LIMIT` counter register, if any -- `Opcode::IfNotZero`
/// decrements it per emitted row (see [`emit_limit_guard`]).
fn compile_limit_setup(em: &mut Emitter, reg: &mut RegAlloc, query: &Query) -> Result<Option<i32>> {
    let Some(limit) = query.limit else {
        return Ok(None);
    };
    let p1 = i32::try_from(limit).map_err(|_| CodegenError::Unsupported {
        reason: format!("LIMIT {limit} does not fit in a p1 operand"),
    })?;
    let r = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, p1, r, 0));
    Ok(Some(r))
}

fn emit_result_row(em: &mut Emitter, reg: &mut RegAlloc, first: i32, count: usize) -> Result<()> {
    if count == 0 {
        em.emit(Instruction::new(Opcode::ResultRow, reg.alloc(), 0, 0));
        return Ok(());
    }
    em.emit(Instruction::new(
        Opcode::ResultRow,
        first,
        i32::try_from(count).map_err(|_| CodegenError::Unsupported {
            reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
        })?,
        0,
    ));
    Ok(())
}

/// Strips an optional `table.` qualifier off `name`.
fn unqualified(name: &str) -> &str {
    match name.find('.') {
        Some(idx) => &name[idx + 1..],
        None => name,
    }
}

/// Builds `join`'s equi-join condition as an `Expr`, qualifying both
/// sides explicitly. `left_col`/`right_col` name which table they
/// belong to structurally (the `Join` type's own contract), unlike a
/// bare `Expr::Column` elsewhere in the query -- so each is qualified
/// here rather than resolved via `Scope`'s unqualified-defaults-to-left
/// convention, which would otherwise send an unqualified `right_col` to
/// the wrong table.
pub(super) fn build_join_cond(scope: &Scope, join: &Join) -> Expr {
    let right_table_name = scope
        .right
        .as_ref()
        .map(|(right_schema, _)| right_schema.name.clone())
        .unwrap_or_default();
    Expr::BinaryOp(
        Box::new(Expr::Column(format!(
            "{}.{}",
            scope.schema.name,
            unqualified(&join.left_col)
        ))),
        BinOp::Eq,
        Box::new(Expr::Column(format!(
            "{right_table_name}.{}",
            unqualified(&join.right_col)
        ))),
    )
}

/// Compiles the inner-join loop for `join` against `right_cursor`,
/// nested inside the already-open outer scan. Handles `LEFT`'s
/// null-extension: a `matched` flag tracks whether the `ON` condition
/// (not `WHERE`) matched any inner row, and a null-extended row is
/// emitted once, after the inner loop, when it never did.
#[allow(clippy::too_many_arguments)]
fn compile_join_body(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    join: &Join,
    right_cursor: i32,
    query: &Query,
    columns: &[String],
    sort_key: Option<SortKeyColumn>,
    sorter_cursor: i32,
    limit_reg: Option<i32>,
    end_label: Label,
    outer_row_skip: Label,
) -> Result<()> {
    // `LEFT` and `FULL` both null-extend an outer row that never matched
    // any inner row; `FULL` additionally needs a second pass (see
    // `compile_full_outer_right_pass`) for right rows no outer row ever
    // matched.
    let matched_reg = if matches!(join.kind, JoinKind::Left | JoinKind::Full) {
        let r = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, r, 0));
        Some(r)
    } else {
        None
    };

    let inner_end_label = em.new_label();
    let inner_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, right_cursor, 0, 0));
    em.patch_p2(inner_rewind_addr, inner_end_label);

    let inner_loop_start = em.new_label();
    em.place(inner_loop_start);
    let inner_row_skip = em.new_label();

    let join_cond = build_join_cond(scope, join);
    super::compile_cond(
        em,
        reg,
        scope,
        &join_cond,
        CondTargets::null_is_false(Target::Fallthrough, Target::Jump(inner_row_skip)),
    )?;

    if let Some(m) = matched_reg {
        em.emit(Instruction::new(Opcode::Integer, 1, m, 0));
    }

    if let Some(where_expr) = &query.where_clause {
        super::compile_cond(
            em,
            reg,
            scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(inner_row_skip)),
        )?;
    }

    emit_row(
        em,
        reg,
        scope,
        columns,
        None,
        sort_key,
        sorter_cursor,
        limit_reg,
        end_label,
    )?;

    em.place(inner_row_skip);
    let inner_next_addr = em.emit(Instruction::new(Opcode::Next, right_cursor, 0, 0));
    em.patch_p2(inner_next_addr, inner_loop_start);
    em.place(inner_end_label);

    if let Some(m) = matched_reg {
        let zero_reg = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, zero_reg, 0));
        let eq_addr = em.emit(Instruction::new(Opcode::Eq, m, 0, zero_reg));
        let null_ext_label = em.new_label();
        em.patch_p2(eq_addr, null_ext_label);
        em.goto(outer_row_skip);

        em.place(null_ext_label);
        emit_row(
            em,
            reg,
            scope,
            columns,
            Some(right_cursor),
            sort_key,
            sorter_cursor,
            limit_reg,
            end_label,
        )?;
    }

    Ok(())
}

/// `FULL OUTER`'s second pass (db-core#101): right-outer, left-inner,
/// emitting a right-null-extended row for every right row the first
/// pass's inner loop never matched. Matching here re-checks only the
/// `ON` condition (not `WHERE`) against every left row -- mirroring
/// `compile_join_body`'s own simplification of skipping `WHERE` on its
/// null-extension path -- so a right row already emitted (matched, or
/// filtered out by `WHERE`) in the first pass is never emitted twice:
/// once any left row satisfies `ON`, the first pass already produced
/// this right row's output (or explicitly filtered it), and this pass
/// only fires when no left row satisfies `ON` at all.
#[allow(clippy::too_many_arguments)]
fn compile_full_outer_right_pass(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    join: &Join,
    left_cursor: i32,
    right_cursor: i32,
    columns: &[String],
    sort_key: Option<SortKeyColumn>,
    sorter_cursor: i32,
    limit_reg: Option<i32>,
    final_label: Label,
) -> Result<()> {
    let right_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, right_cursor, 0, 0));
    em.patch_p2(right_rewind_addr, final_label);

    let pass_loop_start = em.new_label();
    em.place(pass_loop_start);
    let pass_row_skip = em.new_label();

    let matched_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, 0, matched_reg, 0));

    let left_end_label = em.new_label();
    let left_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, left_cursor, 0, 0));
    em.patch_p2(left_rewind_addr, left_end_label);

    let left_loop_start = em.new_label();
    em.place(left_loop_start);
    let left_row_skip = em.new_label();

    let join_cond = build_join_cond(scope, join);
    super::compile_cond(
        em,
        reg,
        scope,
        &join_cond,
        CondTargets::null_is_false(Target::Fallthrough, Target::Jump(left_row_skip)),
    )?;
    em.emit(Instruction::new(Opcode::Integer, 1, matched_reg, 0));

    em.place(left_row_skip);
    let left_next_addr = em.emit(Instruction::new(Opcode::Next, left_cursor, 0, 0));
    em.patch_p2(left_next_addr, left_loop_start);
    em.place(left_end_label);

    let zero_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, 0, zero_reg, 0));
    let eq_addr = em.emit(Instruction::new(Opcode::Eq, matched_reg, 0, zero_reg));
    let unmatched_label = em.new_label();
    em.patch_p2(eq_addr, unmatched_label);
    em.goto(pass_row_skip);

    em.place(unmatched_label);
    emit_row(
        em,
        reg,
        scope,
        columns,
        Some(left_cursor),
        sort_key,
        sorter_cursor,
        limit_reg,
        final_label,
    )?;

    em.place(pass_row_skip);
    let pass_next_addr = em.emit(Instruction::new(Opcode::Next, right_cursor, 0, 0));
    em.patch_p2(pass_next_addr, pass_loop_start);
    em.place(final_label);

    Ok(())
}

/// Emits one output row: either directly via `ResultRow` (no `ORDER BY`,
/// applying the `LIMIT` guard first), or into the sorter via
/// `MakeRecord`/`SorterInsert` (an `ORDER BY` is present, so `LIMIT`
/// applies later, during the post-sort drain). `null_cursor`, when set,
/// null-fills every column that would otherwise be read from it -- used
/// for `LEFT` join's unmatched-row null-extension.
#[allow(clippy::too_many_arguments)]
fn emit_row(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    columns: &[String],
    null_cursor: Option<i32>,
    sort_key: Option<SortKeyColumn>,
    sorter_cursor: i32,
    limit_reg: Option<i32>,
    end_label: Label,
) -> Result<()> {
    if sort_key.is_some() {
        let (first, count) = compile_row_values(em, reg, scope, columns, null_cursor)?;
        let blob_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::MakeRecord,
            first,
            i32::try_from(count).map_err(|_| CodegenError::Unsupported {
                reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
            })?,
            blob_reg,
        ));
        em.emit(Instruction::new(
            Opcode::SorterInsert,
            sorter_cursor,
            blob_reg,
            0,
        ));
        return Ok(());
    }

    if let Some(limit_reg) = limit_reg {
        emit_limit_guard(em, limit_reg, end_label);
    }
    let (first, count) = compile_row_values(em, reg, scope, columns, null_cursor)?;
    em.emit(Instruction::new(
        Opcode::ResultRow,
        first,
        i32::try_from(count).map_err(|_| CodegenError::Unsupported {
            reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
        })?,
        0,
    ));
    Ok(())
}

/// Emits the `LIMIT` stop-guard: called once per row, before emitting
/// it. `IfNotZero` decrements `limit_reg` only while it's positive and
/// jumps whenever it's nonzero -- a negative `LIMIT` (the "no limit"
/// convention) never reaches zero and always takes that jump, staying
/// unbounded -- so the `Goto` below is reached only when `limit_reg`
/// has hit exactly zero, stopping the scan before this row is emitted.
/// Checking before emitting (not after) matters for `LIMIT 0`: an
/// after-the-fact check would let the first row escape before the
/// guard ever ran.
pub(super) fn emit_limit_guard(em: &mut Emitter, limit_reg: i32, end_label: super::Label) {
    let has_budget_addr = em.emit(Instruction::new(Opcode::IfNotZero, limit_reg, 0, 0));
    let stop_addr = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
    em.patch_p2(stop_addr, end_label);
    let continue_label = em.new_label();
    em.patch_p2(has_budget_addr, continue_label);
    em.place(continue_label);
}

/// Compiles each of `names` into a register, returning the contiguous
/// `(first_register, count)` window `Opcode::ResultRow` reads. Columns
/// land in freshly allocated, and therefore already-adjacent, registers
/// by construction -- the contiguity check/`Copy`-based fallback mirrors
/// [`super::value::compile_value_depth`]'s `FunctionCall` handling
/// defensively rather than assuming it can never trip.
///
/// `null_cursor`, when set, null-fills every column resolved to that
/// cursor instead of reading it -- `LEFT` join's unmatched-row
/// null-extension, where the right cursor holds no current row to read.
fn compile_row_values(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    names: &[String],
    null_cursor: Option<i32>,
) -> Result<(i32, usize)> {
    if names.is_empty() {
        return Ok((reg.alloc(), 0));
    }
    let mut regs = Vec::with_capacity(names.len());
    for name in names {
        let (cursor, idx) = scope.resolve(name)?;
        let r = reg.alloc();
        if Some(cursor) == null_cursor {
            em.emit(Instruction::new(Opcode::Null, 0, r, 0));
        } else {
            let table_schema = match &scope.right {
                Some((right_schema, right_cursor)) if cursor == *right_cursor => right_schema,
                _ => &scope.schema,
            };
            emit_column_read(em, table_schema, cursor, idx, r)?;
        }
        regs.push(r);
    }
    let first = regs[0];
    let already_contiguous = regs
        .iter()
        .enumerate()
        .all(|(i, &r)| r == first.saturating_add(i32::try_from(i).unwrap_or(i32::MAX)));
    if already_contiguous {
        return Ok((first, regs.len()));
    }
    let dests: Vec<i32> = (0..regs.len()).map(|_| reg.alloc()).collect();
    for (&r, &dest) in regs.iter().zip(&dests) {
        em.emit(Instruction::new(Opcode::Copy, r, dest, 0));
    }
    Ok((dests[0], dests.len()))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use crate::expr::{BinOp, Expr};
    use crate::types::Literal;
    use crate::vm::row::{execute, InMemoryCursor, Value, Vm};

    fn schema(columns: &[&str]) -> TableSchema {
        schema_named("t", columns)
    }

    fn schema_named(name: &str, columns: &[&str]) -> TableSchema {
        TableSchema {
            name: name.into(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| String::new()).collect(),
            rowid_alias: None,
            root_page: 0,
            indexes: vec![],
        }
    }

    fn base_query(columns: Vec<SelectItem>) -> Query {
        Query {
            columns,
            from: "t".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
        }
    }

    fn run(schema: &TableSchema, query: &Query, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        let program = compile_select(schema, 0, query).unwrap();
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(rows)))
            .unwrap();
        execute(&mut vm, &program).unwrap()
    }

    fn run_join(
        schema: &TableSchema,
        right_schema: &TableSchema,
        query: &Query,
        left_rows: Vec<Vec<Value>>,
        right_rows: Vec<Vec<Value>>,
    ) -> Vec<Vec<Value>> {
        let program = compile_select_join(schema, 0, right_schema, 1, query).unwrap();
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(left_rows)))
            .unwrap();
        vm.open_cursor(1, Box::new(InMemoryCursor::new(right_rows)))
            .unwrap();
        execute(&mut vm, &program).unwrap()
    }

    #[test]
    fn scans_every_row_projecting_selected_columns() {
        let schema = schema(&["a", "b"]);
        let query = base_query(vec![SelectItem::Column("b".into())]);
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(
            rows,
            vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
        );
    }

    #[test]
    fn star_expands_to_every_schema_column_in_order() {
        let schema = schema(&["a", "b"]);
        let query = base_query(vec![SelectItem::Star]);
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Integer(1), Value::Integer(10)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(10)]]);
    }

    #[test]
    fn where_clause_filters_rows() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.where_clause = Some(Expr::BinaryOp(
            Box::new(Expr::Column("a".into())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(1))),
        ));
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn limit_stops_the_scan_early() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.limit = Some(2);
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn limit_zero_emits_no_rows() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.limit = Some(0);
        let rows = run(&schema, &query, vec![vec![Value::Integer(1)]]);
        assert!(rows.is_empty());
    }

    #[test]
    fn empty_table_scans_zero_rows() {
        let schema = schema(&["a"]);
        let query = base_query(vec![SelectItem::Column("a".into())]);
        let rows = run(&schema, &query, vec![]);
        assert!(rows.is_empty());
    }

    #[test]
    fn join_without_a_right_cursor_is_unsupported() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.joins = vec![Join {
            kind: JoinKind::Inner,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn right_join_is_unsupported() {
        let schema = schema(&["a"]);
        let right = schema_named("u", &["b"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.joins = vec![Join {
            kind: JoinKind::Right,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        assert!(matches!(
            compile_select_join(&schema, 0, &right, 1, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn full_outer_join_null_extends_both_unmatched_sides() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Full,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(3), Value::Integer(300)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Null],
                vec![Value::Null, Value::Integer(300)],
            ]
        );
    }

    #[test]
    fn full_outer_join_with_no_matches_null_extends_every_row() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Full,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)]],
            vec![vec![Value::Integer(2), Value::Integer(200)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Null, Value::Integer(200)],
            ]
        );
    }

    #[test]
    fn full_outer_join_with_empty_left_null_extends_every_right_row() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Full,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(200)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Null, Value::Integer(100)],
                vec![Value::Null, Value::Integer(200)],
            ]
        );
    }

    #[test]
    fn inner_join_matches_rows_on_equi_condition() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Inner,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(3), Value::Integer(300)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
    }

    #[test]
    fn left_join_null_extends_unmatched_rows() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Left,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![vec![Value::Integer(1), Value::Integer(100)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Null],
            ]
        );
    }

    #[test]
    fn order_by_sorts_rows() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: false,
        });
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ]
        );
    }

    #[test]
    fn order_by_descending_sorts_rows() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: true,
        });
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
                vec![Value::Integer(1)],
            ]
        );
    }

    #[test]
    fn order_by_column_absent_from_select_list_still_sorts() {
        let schema = schema(&["a", "b"]);
        let mut query = base_query(vec![SelectItem::Column("b".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: false,
        });
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(1), Value::Integer(10)],
            ],
        );
        assert_eq!(
            rows,
            vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
        );
    }

    #[test]
    fn order_by_respects_limit_on_sorted_output() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: false,
        });
        query.limit = Some(2);
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn full_outer_join_order_by_sorts_both_passes_together() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Full,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: false,
        });
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(200)],
            ],
        );
        // `a` is NULL for the right-unmatched row (b=1), so it sorts
        // last under this sorter's `nulls_first: false` default.
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(2), Value::Integer(200)],
                vec![Value::Null, Value::Integer(100)],
            ]
        );
    }

    #[test]
    fn full_outer_join_limit_applies_across_both_passes() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Full,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        query.limit = Some(1);
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(200)],
            ],
        );
        // The first pass alone hits LIMIT 1, so the second (right-outer)
        // pass never runs -- the unmatched right row is never emitted.
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
    }

    #[test]
    fn window_select_item_is_unsupported() {
        let schema = schema(&["a"]);
        let query = base_query(vec![SelectItem::Window(crate::expr::WindowSpec {
            func: crate::expr::WindowFunc::RowNumber,
            arg: None,
            offset: None,
            partition_by: vec![],
            order_by: vec![],
        })]);
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    // db-core#93: `GROUP BY`/`HAVING`/aggregation, ported alongside
    // `codegen::row::aggregate` from sqlite-rs's own codegen tests for
    // that slice.

    use crate::expr::AggFunc;

    fn agg_query(columns: Vec<SelectItem>, group_by: Vec<&str>) -> Query {
        let mut q = base_query(columns);
        q.group_by = group_by.iter().map(|c| (*c).to_string()).collect();
        q
    }

    #[test]
    fn whole_table_count_star_over_an_empty_table_still_emits_one_row() {
        let schema = schema(&["a"]);
        let query = base_query(vec![SelectItem::Agg(AggFunc::Count, None)]);
        let rows = run(&schema, &query, vec![]);
        assert_eq!(rows, vec![vec![Value::Integer(0)]]);
    }

    #[test]
    fn whole_table_aggregates_over_an_empty_table_finalize_to_null() {
        let schema = schema(&["a"]);
        let query = base_query(vec![
            SelectItem::Agg(AggFunc::Sum, Some("a".into())),
            SelectItem::Agg(AggFunc::Min, Some("a".into())),
            SelectItem::Agg(AggFunc::Max, Some("a".into())),
            SelectItem::Agg(AggFunc::Avg, Some("a".into())),
        ]);
        let rows = run(&schema, &query, vec![]);
        assert_eq!(
            rows,
            vec![vec![Value::Null, Value::Null, Value::Null, Value::Null]]
        );
    }

    #[test]
    fn whole_table_aggregates_fold_every_row() {
        let schema = schema(&["a"]);
        let query = base_query(vec![
            SelectItem::Agg(AggFunc::Count, None),
            SelectItem::Agg(AggFunc::Sum, Some("a".into())),
            SelectItem::Agg(AggFunc::Min, Some("a".into())),
            SelectItem::Agg(AggFunc::Max, Some("a".into())),
            SelectItem::Agg(AggFunc::Avg, Some("a".into())),
        ]);
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(5)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(3),
                Value::Integer(9),
                Value::Integer(1),
                Value::Integer(5),
                Value::Real(3.0),
            ]]
        );
    }

    #[test]
    fn whole_table_aggregate_honours_the_where_clause() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Agg(AggFunc::Count, None)]);
        query.where_clause = Some(Expr::BinaryOp(
            Box::new(Expr::Column("a".into())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(1))),
        ));
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn group_by_emits_one_row_per_group_in_key_order() {
        let schema = schema(&["g", "v"]);
        let query = agg_query(
            vec![
                SelectItem::Column("g".into()),
                SelectItem::Agg(AggFunc::Count, None),
                SelectItem::Agg(AggFunc::Sum, Some("v".into())),
            ],
            vec!["g"],
        );
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(2), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(1), Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
                vec![Value::Integer(2), Value::Integer(2), Value::Integer(30)],
            ]
        );
    }

    #[test]
    fn group_by_over_an_empty_table_emits_no_rows() {
        let schema = schema(&["g", "v"]);
        let query = agg_query(
            vec![
                SelectItem::Column("g".into()),
                SelectItem::Agg(AggFunc::Count, None),
            ],
            vec!["g"],
        );
        assert!(run(&schema, &query, vec![]).is_empty());
    }

    #[test]
    fn group_by_collects_null_keys_into_one_group() {
        let schema = schema(&["g", "v"]);
        let query = agg_query(
            vec![
                SelectItem::Column("g".into()),
                SelectItem::Agg(AggFunc::Count, None),
            ],
            vec!["g"],
        );
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Null, Value::Integer(1)],
                vec![Value::Integer(1), Value::Integer(2)],
                vec![Value::Null, Value::Integer(3)],
            ],
        );
        assert!(rows.contains(&vec![Value::Null, Value::Integer(2)]));
        assert!(rows.contains(&vec![Value::Integer(1), Value::Integer(1)]));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn group_by_two_keys_groups_on_the_pair() {
        let schema = schema(&["a", "b", "v"]);
        let query = agg_query(
            vec![
                SelectItem::Column("a".into()),
                SelectItem::Column("b".into()),
                SelectItem::Agg(AggFunc::Sum, Some("v".into())),
            ],
            vec!["a", "b"],
        );
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(1), Value::Integer(1), Value::Integer(5)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(1), Value::Integer(15)],
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(20)],
            ]
        );
    }

    #[test]
    fn having_filters_whole_groups_after_aggregation() {
        let schema = schema(&["g", "v"]);
        let mut query = agg_query(
            vec![
                SelectItem::Column("g".into()),
                SelectItem::Agg(AggFunc::Count, None),
            ],
            vec!["g"],
        );
        query.having = Some(Expr::BinaryOp(
            Box::new(Expr::Column("COUNT(*)".into())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(1))),
        ));
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(2), Value::Integer(30)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2), Value::Integer(2)]]);
    }

    /// A `HAVING` aggregate absent from the `SELECT` list still gets its
    /// own accumulator slot -- sqlite-rs's `collect_aggregates` scans the
    /// `HAVING` clause too.
    #[test]
    fn having_may_reference_an_aggregate_absent_from_the_select_list() {
        let schema = schema(&["g", "v"]);
        let mut query = agg_query(vec![SelectItem::Column("g".into())], vec!["g"]);
        query.having = Some(Expr::BinaryOp(
            Box::new(Expr::Column("SUM(v)".into())),
            BinOp::Ge,
            Box::new(Expr::Literal(Literal::Int(50))),
        ));
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(2), Value::Integer(30)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    /// A `HAVING` sharing its aggregate with a result column accumulates
    /// into a single slot, so both read the same finalized value.
    #[test]
    fn having_shares_one_slot_with_an_identical_result_column() {
        let schema = schema(&["g", "v"]);
        let mut query = agg_query(
            vec![
                SelectItem::Column("g".into()),
                SelectItem::Agg(AggFunc::Sum, Some("v".into())),
            ],
            vec!["g"],
        );
        query.having = Some(Expr::BinaryOp(
            Box::new(Expr::Column("SUM(v)".into())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(15))),
        ));
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2), Value::Integer(20)]]);
    }

    #[test]
    fn having_on_a_whole_table_aggregate_may_suppress_the_only_row() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Agg(AggFunc::Count, None)]);
        query.having = Some(Expr::BinaryOp(
            Box::new(Expr::Column("COUNT(*)".into())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(5))),
        ));
        let rows = run(&schema, &query, vec![vec![Value::Integer(1)]]);
        assert!(rows.is_empty());
    }

    #[test]
    fn limit_applies_to_groups_not_to_scanned_rows() {
        let schema = schema(&["g", "v"]);
        let mut query = agg_query(
            vec![
                SelectItem::Column("g".into()),
                SelectItem::Agg(AggFunc::Count, None),
            ],
            vec!["g"],
        );
        query.limit = Some(1);
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(11)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
    }

    /// A plain (non-aggregate) column that isn't a `GROUP BY` key takes
    /// an "arbitrary row" from the group -- the group's first row, which
    /// is what both strategies retain.
    #[test]
    fn a_non_key_plain_column_reads_the_groups_first_row() {
        let schema = schema(&["g", "v"]);
        let query = agg_query(
            vec![
                SelectItem::Column("v".into()),
                SelectItem::Agg(AggFunc::Count, None),
            ],
            vec!["g"],
        );
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(11)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(10), Value::Integer(2)]]);
    }

    #[test]
    fn star_alongside_an_aggregate_expands_to_every_schema_column() {
        let schema = schema(&["g", "v"]);
        let query = agg_query(
            vec![SelectItem::Star, SelectItem::Agg(AggFunc::Count, None)],
            vec!["g"],
        );
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Integer(1), Value::Integer(10)]],
        );
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(1),
                Value::Integer(10),
                Value::Integer(1)
            ]]
        );
    }

    #[test]
    fn aggregate_over_an_inner_join_folds_only_matched_rows() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Agg(AggFunc::Count, None),
            SelectItem::Agg(AggFunc::Sum, Some("u.c".into())),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Inner,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(3), Value::Integer(300)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
    }

    #[test]
    fn group_by_over_an_inner_join_groups_on_a_left_column() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = agg_query(
            vec![
                SelectItem::Column("a".into()),
                SelectItem::Agg(AggFunc::Sum, Some("u.c".into())),
            ],
            vec!["a"],
        );
        query.joins = vec![Join {
            kind: JoinKind::Inner,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(1), Value::Integer(50)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(150)],
                vec![Value::Integer(2), Value::Integer(20)],
            ]
        );
    }

    /// `LEFT`'s null-extended row still forms a group: `COUNT(*)` counts
    /// it, but `SUM` over the null-extended right column skips it.
    #[test]
    fn group_by_over_a_left_join_keeps_unmatched_outer_rows() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = agg_query(
            vec![
                SelectItem::Column("a".into()),
                SelectItem::Agg(AggFunc::Count, None),
                SelectItem::Agg(AggFunc::Sum, Some("u.c".into())),
            ],
            vec!["a"],
        );
        query.joins = vec![Join {
            kind: JoinKind::Left,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![vec![Value::Integer(1), Value::Integer(100)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Integer(1), Value::Null],
            ]
        );
    }

    #[test]
    fn having_combined_with_a_join_is_unsupported() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b"]);
        let mut query = agg_query(
            vec![
                SelectItem::Column("a".into()),
                SelectItem::Agg(AggFunc::Count, None),
            ],
            vec!["a"],
        );
        query.joins = vec![Join {
            kind: JoinKind::Inner,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        query.having = Some(Expr::Literal(Literal::Int(1)));
        assert!(matches!(
            compile_select_join(&left, 0, &right, 1, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn full_outer_join_combined_with_aggregation_is_unsupported() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b"]);
        let mut query = base_query(vec![SelectItem::Agg(AggFunc::Count, None)]);
        query.joins = vec![Join {
            kind: JoinKind::Full,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        assert!(matches!(
            compile_select_join(&left, 0, &right, 1, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn order_by_combined_with_aggregation_is_unsupported() {
        let schema = schema(&["g"]);
        let mut query = agg_query(
            vec![
                SelectItem::Column("g".into()),
                SelectItem::Agg(AggFunc::Count, None),
            ],
            vec!["g"],
        );
        query.order_by = Some(crate::expr::OrderBy {
            column: "g".into(),
            descending: false,
        });
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn group_by_an_unknown_column_is_rejected() {
        let schema = schema(&["a"]);
        let query = agg_query(vec![SelectItem::Agg(AggFunc::Count, None)], vec!["nope"]);
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::UnknownColumn(_))
        ));
    }

    /// The sort-then-group strategy is the fallback the hash strategy
    /// declines to; exercised directly here so both strategies are
    /// covered on an explicit `GROUP BY`, not just via the join path.
    #[test]
    fn the_sort_strategy_produces_the_same_groups_as_the_hash_strategy() {
        let schema = schema(&["g", "v"]);
        let query = agg_query(
            vec![
                SelectItem::Column("g".into()),
                SelectItem::Agg(AggFunc::Sum, Some("v".into())),
            ],
            vec!["g"],
        );
        let rows = vec![
            vec![Value::Integer(2), Value::Integer(10)],
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(2), Value::Integer(20)],
        ];

        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let end_label = em.new_label();
        let cursors = super::super::aggregate::ScanCursors::past(0, 0);
        super::super::aggregate::compile_grouped_scan(
            &mut em,
            &mut reg,
            &query,
            &schema,
            cursors,
            None,
            end_label,
            false,
            &mut |em: &mut Emitter, reg: &mut RegAlloc, first: i32, count: usize| {
                emit_result_row(em, reg, first, count)
            },
        )
        .unwrap();
        em.place(end_label);
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        let program = em.finish();

        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(rows.clone())))
            .unwrap();
        let sorted = execute(&mut vm, &program).unwrap();
        assert_eq!(sorted, run(&schema, &query, rows));
        assert_eq!(
            sorted,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(30)],
            ]
        );
    }
}
