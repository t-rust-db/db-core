// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
use super::super::join_access::{
    compile_join_level_for_sort, joined_column_offset, named_result_column, resolve_scope_column,
    JoinOrderPlan, NamedColumn,
};
use super::super::joins::LevelPlan;
use super::super::limit_scan::{
    compile_limit_setup, emit_limit_guard, emit_offset_guard, LimitState,
};
use super::super::order_by::strip_collate;
use super::super::projection::{emit_dedup_check, joined_result_collations};
use super::super::*;
use super::accum::{collect_aggregates, map_subexprs, substitute_aggregates, AggSlot};

/// [`super::compile_grouped_scan`]'s joined counterpart (#333):
/// `GROUP BY`/an implicit whole-table aggregate combined with a JOIN.
/// Shares the same sort-then-group shape, generalized from a single
/// `TableSchema`/cursor to a joined [`Scope`] the same way #250
/// generalized `ORDER BY`/`DISTINCT` (see
/// [`super::super::join_access::compile_joined_sorted_scan`]): pass 1
/// reuses [`compile_join_level_for_sort`] to buffer every WHERE-matching
/// joined row (every binding's every column, flat and concatenated,
/// exactly as [`super::super::join_access::emit_full_joined_row`]
/// already does for the `ORDER BY` case) into the sorter, keyed by the
/// `GROUP BY` columns; pass 2 walks the sorted buffer through a shared
/// flat pseudo cursor, detecting group boundaries and accumulating via
/// `AggStep`/`AggFinal` against that flat row.
///
/// Every expression — `GROUP BY` keys (#610), aggregate arguments,
/// result columns, `HAVING` and `ORDER BY` terms (#612) — is compiled
/// against the flat row (or the flushed group record) by rewriting its
/// column references to that record's `__c<abs>` fields
/// ([`to_record_expr`]), each keeping its column's declared collation.
/// A subquery inside one of them is still refused.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn compile_joined_grouped_scan<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    exec_bindings: &[TableBinding],
    orig_bindings: &[TableBinding],
    pos_of: &[usize],
    levels: &[LevelPlan],
    dedup_star: &[std::collections::HashSet<String>],
    catalog: &[TableSchema],
    full_scope: &Scope,
    sort_cursor: i32,
    pseudo_cursor: i32,
    flush_cursor: i32,
    // #502: `Some((order_sort_cursor, order_pseudo_cursor))` when
    // `select.order_by` is non-empty — a second sorter pass that
    // re-orders every finalized group row (raw columns + finalized
    // aggregates) before it reaches `sink`, instead of `sink` being
    // called directly as each group is flushed.
    order_cursors: Option<(i32, i32)>,
    // #608: the ephemeral index `SELECT DISTINCT` dedups group rows in.
    distinct_cursor: Option<i32>,
    end_label: Label,
    implicit_group: bool,
    sink: &mut F,
) -> Result<(), CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, i32) -> Result<(), CodegenError>,
{
    if let Some(cursor) = distinct_cursor {
        em.emit(Instruction::new(Opcode::OpenEphemeral, cursor, 0, 0));
    }
    // #612: every expression is compiled against the flat joined row,
    // its column references rewritten to that row's `__c<abs>` fields.
    let group_keys: Vec<Expr> = select
        .group_by
        .iter()
        .map(|expr| to_record_expr(expr, full_scope))
        .collect::<Result<_, _>>()?;

    let aggs = collect_aggregates(select)?;
    // DISTINCT-aggregate ephemeral dedup cursors start right after the
    // highest cursor number this scan already uses — `flush_cursor`
    // normally, or past the two extra `ORDER BY` sorter cursors
    // (#502) when `order_cursors` is `Some`.
    let eph_base = match order_cursors {
        Some((_, order_pseudo_cursor)) => order_pseudo_cursor.saturating_add(1),
        None => flush_cursor.saturating_add(1),
    };
    let agg_slots: Vec<AggSlot> = aggs
        .into_iter()
        .enumerate()
        .map(|(slot, (call, name, arg, distinct))| AggSlot {
            call,
            name,
            arg,
            slot: i32::try_from(slot).unwrap_or(0),
            eph_cursor: distinct.then(|| eph_base.saturating_add(i32::try_from(slot).unwrap_or(0))),
        })
        .collect();

    let total_width = joined_column_offset(full_scope, full_scope.tables.len())?;

    // #502: a trailing `ORDER BY` sorts the *finalized* group rows —
    // `total_width` raw joined columns followed by `agg_slots.len()`
    // finalized aggregate values, exactly what `flush_joined_group`
    // already assembles into `dests` for every group. Resolved and
    // opened up front so `SorterInsert` calls inside the boundary/tail
    // flushes below have a P4 sort-key layout already in place.
    // #612: a computed key is evaluated per group and appended after the
    // record's `total_width + agg_slots.len()` fields.
    let mut order_exprs: Vec<Expr> = Vec::new();
    if let Some((order_sort_cursor, _)) = order_cursors {
        let record_width = total_width.saturating_add(agg_slots.len());
        let order_sort_keys: Vec<SortKeyColumn> = select
            .order_by
            .iter()
            .map(|term| {
                let (key, collation) = resolve_group_order_target(
                    select,
                    full_scope,
                    &agg_slots,
                    total_width,
                    &term.expr,
                )?;
                let index = match key {
                    GroupOrderKey::Offset(offset) => offset,
                    GroupOrderKey::Expr(expr) => {
                        order_exprs.push(expr);
                        record_width.saturating_add(order_exprs.len().saturating_sub(1))
                    }
                };
                let descending = term.desc.unwrap_or(false);
                let nulls_first = term
                    .nulls_last
                    .map_or(!descending, |nulls_last| !nulls_last);
                Ok(SortKeyColumn {
                    index,
                    descending,
                    collation,
                    nulls_first,
                })
            })
            .collect::<Result<_, CodegenError>>()?;
        em.emit(Instruction::with_p4(
            Opcode::SorterOpen,
            order_sort_cursor,
            0,
            0,
            P4::SortKey(order_sort_keys),
        ));
    }

    // Pass 1 sorts on a bare-column key in place, and on a computed key
    // (#610) evaluated against the join's cursors.
    let order_by_plans: Vec<JoinOrderPlan> = select
        .group_by
        .iter()
        .map(|expr| {
            let collation = collation_of(expr)
                .or_else(|| expr_collation(full_scope, expr))
                .unwrap_or(Collation::Binary);
            Ok(match joined_bare_column_offset(full_scope, expr) {
                Some(offset) => JoinOrderPlan::ascending_offset(offset?, collation),
                None => JoinOrderPlan::ascending_expr(expr.clone(), collation),
            })
        })
        .collect::<Result<_, CodegenError>>()?;

    // Pass 1: buffer every WHERE-matching joined row, sorted by the
    // GROUP BY key — reuses the exact traversal #250's joined `ORDER
    // BY` pass 1 uses, just keyed differently.
    let sorter_open_addr = em.emit(Instruction::with_p4(
        Opcode::SorterOpen,
        sort_cursor,
        0,
        0,
        P4::None,
    ));
    let mut null_mask = vec![false; exec_bindings.len()];
    let mut matched_regs: Vec<Option<i32>> = vec![None; exec_bindings.len()];
    compile_join_level_for_sort(
        em,
        reg,
        select,
        exec_bindings,
        orig_bindings,
        pos_of,
        levels,
        dedup_star,
        &mut null_mask,
        &mut matched_regs,
        0,
        catalog,
        &order_by_plans,
        sort_cursor,
        sorter_open_addr,
    )?;

    let sort_addr = em.emit(Instruction::new(Opcode::SorterSort, sort_cursor, 0, 0));
    let empty_sorter_target = if implicit_group {
        em.new_label()
    } else {
        end_label
    };
    em.patch_p2(sort_addr, empty_sorter_target);

    let limit = compile_limit_setup(em, reg, full_scope, select)?;

    let zero_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, 0, zero_reg, 0));
    let have_group_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, 0, have_group_reg, 0));

    let prev_key_regs: Vec<i32> = group_keys.iter().map(|_| reg.alloc()).collect();
    let snapshot_regs: Vec<i32> = (0..total_width).map(|_| reg.alloc()).collect();
    for &r in &snapshot_regs {
        em.emit(Instruction::new(Opcode::Null, 0, r, 0));
    }

    let sorted_loop = em.new_label();
    em.place(sorted_loop);
    let sorter_data_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::SorterData,
        sort_cursor,
        sorter_data_reg,
        0,
    ));
    em.emit(Instruction::new(
        Opcode::OpenPseudo,
        pseudo_cursor,
        sorter_data_reg,
        0,
    ));

    let row_schema = joined_record_schema(full_scope, &[]);
    let row_scope = record_scope(&row_schema, pseudo_cursor, full_scope);
    let cur_key_regs: Vec<i32> = group_keys
        .iter()
        .map(|key| compile_value(em, reg, &row_scope, key))
        .collect::<Result<_, _>>()?;

    let group_key_p4s: Vec<P4> = select
        .group_by
        .iter()
        .map(|expr| {
            let collation = collation_of(expr)
                .or_else(|| expr_collation(full_scope, expr))
                .unwrap_or(Collation::Binary);
            let affinity = comparison_affinity(expr_affinity(full_scope, expr), None);
            p4_coll_seq(collation, affinity)
        })
        .collect();

    let boundary_label = em.new_label();
    let not_boundary_label = em.new_label();
    let first_row_check = em.emit(Instruction::new(Opcode::Eq, have_group_reg, 0, zero_reg));
    em.patch_p2(first_row_check, boundary_label);
    for ((&cur, &prev), p4) in cur_key_regs.iter().zip(&prev_key_regs).zip(&group_key_p4s) {
        let a_null = em.new_label();
        let same_col = em.new_label();
        let a_null_addr = em.emit(Instruction::new(Opcode::IsNull, cur, 0, 0));
        em.patch_p2(a_null_addr, a_null);
        let b_null_addr = em.emit(Instruction::new(Opcode::IsNull, prev, 0, 0));
        em.patch_p2(b_null_addr, boundary_label);
        let eq_addr = em.emit(Instruction::with_p4(Opcode::Eq, cur, 0, prev, p4.clone()));
        em.patch_p2(eq_addr, same_col);
        let goto_boundary = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
        em.patch_p2(goto_boundary, boundary_label);
        em.place(a_null);
        let b_not_null_addr = em.emit(Instruction::new(Opcode::NotNull, prev, 0, 0));
        em.patch_p2(b_not_null_addr, boundary_label);
        em.place(same_col);
    }
    let goto_not_boundary = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
    em.patch_p2(goto_not_boundary, not_boundary_label);

    em.place(boundary_label);
    let skip_flush = em.new_label();
    let flush_check = em.emit(Instruction::new(Opcode::Eq, have_group_reg, 0, zero_reg));
    em.patch_p2(flush_check, skip_flush);
    flush_joined_group(
        em,
        reg,
        select,
        full_scope,
        dedup_star,
        total_width,
        flush_cursor,
        &snapshot_regs,
        &agg_slots,
        limit.as_ref(),
        end_label,
        order_cursors.map(|(order_sort_cursor, _)| order_sort_cursor),
        &order_exprs,
        distinct_cursor,
        sink,
    )?;
    em.place(skip_flush);
    for (&cur, &prev) in cur_key_regs.iter().zip(&prev_key_regs) {
        em.emit(Instruction::new(Opcode::Copy, cur, prev, 0));
    }
    em.emit(Instruction::new(Opcode::Integer, 1, have_group_reg, 0));
    for agg in &agg_slots {
        emit_joined_agg_step(em, reg, full_scope, &row_scope, agg, true)?;
    }
    let after_accumulate = em.new_label();
    let goto_after_accumulate = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
    em.patch_p2(goto_after_accumulate, after_accumulate);

    em.place(not_boundary_label);
    for agg in &agg_slots {
        emit_joined_agg_step(em, reg, full_scope, &row_scope, agg, false)?;
    }

    em.place(after_accumulate);
    for (idx, &dest) in snapshot_regs.iter().enumerate() {
        em.emit(Instruction::new(
            Opcode::Column,
            pseudo_cursor,
            i32::try_from(idx).unwrap_or(0),
            dest,
        ));
    }

    let sorted_next = em.emit(Instruction::new(Opcode::SorterNext, sort_cursor, 0, 0));
    em.patch_p2(sorted_next, sorted_loop);

    if implicit_group {
        em.place(empty_sorter_target);
    }
    let skip_tail_flush = em.new_label();
    if !implicit_group {
        let tail_check = em.emit(Instruction::new(Opcode::Eq, have_group_reg, 0, zero_reg));
        em.patch_p2(tail_check, skip_tail_flush);
    }
    flush_joined_group(
        em,
        reg,
        select,
        full_scope,
        dedup_star,
        total_width,
        flush_cursor,
        &snapshot_regs,
        &agg_slots,
        limit.as_ref(),
        end_label,
        order_cursors.map(|(order_sort_cursor, _)| order_sort_cursor),
        &order_exprs,
        distinct_cursor,
        sink,
    )?;
    em.place(skip_tail_flush);

    // Pass 3 (#502): every finalized group row is now buffered in
    // `order_sort_cursor`, keyed by the `ORDER BY` targets resolved
    // above. Drain it sorted, applying `LIMIT`/`OFFSET` here — not at
    // group-flush time — since the final order (and therefore which
    // rows a `LIMIT` keeps) isn't known until this sort completes.
    // Mirrors `compile_joined_sorted_scan`'s plain-`ORDER BY` pass 2.
    if let Some((order_sort_cursor, order_pseudo_cursor)) = order_cursors {
        let order_sort_addr = em.emit(Instruction::new(
            Opcode::SorterSort,
            order_sort_cursor,
            0,
            0,
        ));
        em.patch_p2(order_sort_addr, end_label);

        let order_loop = em.new_label();
        em.place(order_loop);
        let order_data_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::SorterData,
            order_sort_cursor,
            order_data_reg,
            0,
        ));
        em.emit(Instruction::new(
            Opcode::OpenPseudo,
            order_pseudo_cursor,
            order_data_reg,
            0,
        ));

        let order_row_skip = em.new_label();
        if let Some(limit) = &limit {
            emit_offset_guard(em, limit, order_row_skip);
        }
        if let Some(limit) = &limit {
            emit_limit_guard(em, limit, end_label);
        }
        let (first, count) = project_grouped_result_columns(
            em,
            reg,
            select,
            full_scope,
            dedup_star,
            &agg_slots,
            order_pseudo_cursor,
        )?;
        sink(em, reg, first, i32::try_from(count).unwrap_or(0))?;

        em.place(order_row_skip);
        let order_next = em.emit(Instruction::new(
            Opcode::SorterNext,
            order_sort_cursor,
            0,
            0,
        ));
        em.patch_p2(order_next, order_loop);
    }
    Ok(())
}

/// Resolves `expr` (after stripping any `COLLATE` wrapper) to its
/// absolute flat-row offset when it is a bare (optionally qualified)
/// column; `None` for any other expression.
fn joined_bare_column_offset(
    full_scope: &Scope,
    expr: &Expr,
) -> Option<Result<usize, CodegenError>> {
    let ExprKind::Column { table, name, .. } = &strip_collate(expr).kind else {
        return None;
    };
    Some(
        resolve_scope_column(full_scope, table.as_deref(), name).and_then(
            |(binding_idx, local_idx)| {
                Ok(joined_column_offset(full_scope, binding_idx)?.saturating_add(local_idx))
            },
        ),
    )
}

/// [`super::accum::emit_agg_step`]'s joined counterpart: evaluates
/// `agg.arg` against the flat joined row in `row_scope` (#612), so the
/// argument can be any expression over the joined columns.
fn emit_joined_agg_step(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    full_scope: &Scope,
    row_scope: &Scope,
    agg: &AggSlot,
    reset: bool,
) -> Result<(), CodegenError> {
    let (arg_reg, arity, collation) = match &agg.arg {
        Some(expr) => (
            Some(compile_value(
                em,
                reg,
                row_scope,
                &to_record_expr(expr, full_scope)?,
            )?),
            1usize,
            collation_of(expr)
                .or_else(|| expr_collation(full_scope, expr))
                .unwrap_or(Collation::Binary),
        ),
        None => (None, 0usize, Collation::Binary),
    };
    let p2 = arg_reg.unwrap_or(0);
    let mut instr = Instruction::with_p4(
        Opcode::AggStep,
        agg.slot,
        p2,
        0,
        P4::AggFunc {
            name: agg.name.clone(),
            arity,
            collation,
        },
    );
    if reset {
        instr.p5 = 1;
    }
    em.emit(instr);
    Ok(())
}

fn strip_paren(expr: &Expr) -> &Expr {
    match &expr.kind {
        ExprKind::Paren(inner) => strip_paren(inner),
        _ => expr,
    }
}

/// [`super::accum::flush_group`]'s joined counterpart: finalizes one
/// group's row — `snapshot_regs` (the group's last-seen flat joined
/// row) plus each aggregate's finalized value — into a fresh
/// `total_width + agg_slots.len()`-wide record.
///
/// When `order_sort_cursor` is `None` (no `ORDER BY`), that record is
/// reprojected into `select`'s result columns and handed to `sink`
/// directly, applying `limit`/`end_label` per group exactly as before
/// #502. When `order_sort_cursor` is `Some` (#502), `limit` is ignored
/// here — the record is inserted as-is into that second sorter
/// instead, and [`compile_joined_grouped_scan`]'s pass 3 reprojects and
/// applies `LIMIT`/`OFFSET` once the final `ORDER BY` order is known,
/// same as [`compile_joined_sorted_scan`]'s plain-`ORDER BY` pass 2.
#[allow(clippy::too_many_arguments)]
fn flush_joined_group<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    full_scope: &Scope,
    dedup_star: &[std::collections::HashSet<String>],
    total_width: usize,
    flush_cursor: i32,
    snapshot_regs: &[i32],
    agg_slots: &[AggSlot],
    limit: Option<&LimitState>,
    end_label: Label,
    order_sort_cursor: Option<i32>,
    order_exprs: &[Expr],
    distinct_cursor: Option<i32>,
    sink: &mut F,
) -> Result<(), CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, i32) -> Result<(), CodegenError>,
{
    let synthetic_count = total_width.saturating_add(agg_slots.len());
    let dests: Vec<i32> = (0..synthetic_count).map(|_| reg.alloc()).collect();
    let synthetic_first = dests.first().copied().unwrap_or_else(|| reg.alloc());
    for (&snap, &dest) in snapshot_regs.iter().zip(&dests) {
        em.emit(Instruction::new(Opcode::Copy, snap, dest, 0));
    }
    let agg_dests = dests.get(total_width..).unwrap_or(&[]);
    for (agg, &dest) in agg_slots.iter().zip(agg_dests) {
        let arity = usize::from(agg.arg.is_some());
        em.emit(Instruction::with_p4(
            Opcode::AggFinal,
            agg.slot,
            0,
            dest,
            P4::Str(format!("{}({arity})", agg.name)),
        ));
    }
    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        synthetic_first,
        i32::try_from(synthetic_count).unwrap_or(0),
        record_reg,
    ));
    em.emit(Instruction::new(
        Opcode::OpenPseudo,
        flush_cursor,
        record_reg,
        0,
    ));

    let skip_label = em.new_label();
    // db-core#219 (carried over from db-core#178): `HAVING` over a joined
    // group filters the finalized record the same way `accum::flush_group`
    // does for a single table -- aggregate calls rewritten to the
    // record's trailing `__agg<n>` fields via `substitute_aggregates`,
    // bare columns resolved by name against a synthetic single-cursor
    // schema over the flat joined row -- and, with a trailing `ORDER BY`,
    // ahead of the `SorterInsert` so a rejected group never reaches the
    // re-sort pass.
    if let Some(having) = &select.having {
        let synthetic_names = agg_field_names(agg_slots);
        let synthetic = joined_record_schema(full_scope, &synthetic_names);
        let flush_scope = record_scope(&synthetic, flush_cursor, full_scope);
        let rewritten = to_record_expr(
            &substitute_aggregates(having, agg_slots, &synthetic_names)?,
            full_scope,
        )?;
        compile_cond(
            em,
            reg,
            &flush_scope,
            &rewritten,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(skip_label)),
        )?;
    }

    // #608: `SELECT DISTINCT` drops a group whose visible columns
    // repeat an earlier group's, before it is sorted or counted against
    // LIMIT (as sqlite3: dedup feeds the ORDER BY sorter).
    let projected = match distinct_cursor {
        Some(cursor) => {
            let (first, count) = project_grouped_result_columns(
                em,
                reg,
                select,
                full_scope,
                dedup_star,
                agg_slots,
                flush_cursor,
            )?;
            let collations = joined_result_collations(select, full_scope, dedup_star);
            emit_dedup_check(em, cursor, first, collations, skip_label);
            Some((first, count))
        }
        None => None,
    };

    if let Some(order_sort_cursor) = order_sort_cursor {
        let sort_record = if order_exprs.is_empty() {
            record_reg
        } else {
            // The record plus each computed ORDER BY key (#612), copied
            // into one contiguous run after the keys are evaluated.
            let synthetic_names = agg_field_names(agg_slots);
            let synthetic = joined_record_schema(full_scope, &synthetic_names);
            let flush_scope = record_scope(&synthetic, flush_cursor, full_scope);
            let key_regs: Vec<i32> = order_exprs
                .iter()
                .map(|key| compile_value(em, reg, &flush_scope, key))
                .collect::<Result<_, _>>()?;
            let block: Vec<i32> = dests.iter().chain(&key_regs).map(|_| reg.alloc()).collect();
            for (&src, &dest) in dests.iter().chain(&key_regs).zip(&block) {
                em.emit(Instruction::new(Opcode::Copy, src, dest, 0));
            }
            let sort_record = reg.alloc();
            em.emit(Instruction::new(
                Opcode::MakeRecord,
                block.first().copied().unwrap_or(synthetic_first),
                i32::try_from(block.len()).unwrap_or(0),
                sort_record,
            ));
            sort_record
        };
        em.emit(Instruction::new(
            Opcode::SorterInsert,
            order_sort_cursor,
            sort_record,
            0,
        ));
        em.place(skip_label);
        return Ok(());
    }

    if let Some(limit) = limit {
        emit_offset_guard(em, limit, skip_label);
    }
    if let Some(limit) = limit {
        emit_limit_guard(em, limit, end_label);
    }

    let (first, count) = match projected {
        Some(projected) => projected,
        None => project_grouped_result_columns(
            em,
            reg,
            select,
            full_scope,
            dedup_star,
            agg_slots,
            flush_cursor,
        )?,
    };
    sink(em, reg, first, i32::try_from(count).unwrap_or(0))?;
    em.place(skip_label);
    Ok(())
}

/// The flat joined row as one record (#612): field `abs` is named
/// `__c<abs>` and keeps its column's declared type and collation, then
/// come the `extra` fields (each aggregate's `__agg<n>`). Unique names
/// keep a self-join's `a.s` and `b.s` apart; expressions rewritten by
/// [`to_record_expr`] compile against it through the ordinary
/// `compile_value`/`compile_cond`.
fn joined_record_schema(full_scope: &Scope, extra: &[String]) -> TableSchema {
    let mut columns = Vec::new();
    let mut column_types = Vec::new();
    let mut column_collations = Vec::new();
    for binding in &full_scope.tables {
        let schema = &binding.schema;
        for idx in 0..schema.columns.len() {
            columns.push(record_column_name(columns.len()));
            column_types.push(schema.column_types.get(idx).cloned().unwrap_or_default());
            column_collations.push(
                schema
                    .column_collations
                    .get(idx)
                    .copied()
                    .unwrap_or(Collation::Binary),
            );
        }
    }
    columns.extend(extra.iter().cloned());
    column_types.extend(extra.iter().map(|_| String::new()));
    column_collations.extend(extra.iter().map(|_| Collation::Binary));
    TableSchema {
        columns,
        column_types,
        column_collations,
        ..TableSchema::default()
    }
}

fn agg_field_names(agg_slots: &[AggSlot]) -> Vec<String> {
    (0..agg_slots.len()).map(|i| format!("__agg{i}")).collect()
}

fn record_column_name(abs: usize) -> String {
    format!("__c{abs}")
}

/// A scope reading [`joined_record_schema`]'s record from `cursor`.
fn record_scope(schema: &TableSchema, cursor: i32, full_scope: &Scope) -> Scope {
    Scope::single(schema, cursor)
        .with_catalog(full_scope.catalog.clone())
        .with_record_cursors()
}

/// `expr` with every column reference rewritten to its `__c<abs>` field
/// of the flat joined row (#612); `__agg<n>` fields already put in by
/// `substitute_aggregates` stay. Spans are cleared, so two spellings of
/// the same expression compare equal. A subquery would still read the
/// join's cursors and is refused.
fn to_record_expr(expr: &Expr, full_scope: &Scope) -> Result<Expr, CodegenError> {
    let kind = match &expr.kind {
        ExprKind::Column {
            table: None, name, ..
        } if name.starts_with("__agg") => expr.kind.clone(),
        ExprKind::Column { table, name, .. } => {
            let (binding_idx, local_idx) =
                resolve_scope_column(full_scope, table.as_deref(), name)?;
            ExprKind::Column {
                table: None,
                catalog: None,
                name: record_column_name(
                    joined_column_offset(full_scope, binding_idx)?.saturating_add(local_idx),
                ),
            }
        }
        ExprKind::Subquery(_)
        | ExprKind::Exists { .. }
        | ExprKind::InSubquery { .. }
        | ExprKind::InSubqueryMulti { .. } => {
            return Err(CodegenError::Unsupported {
                reason: "a subquery inside a GROUP BY/aggregate combined with a JOIN is not yet \
                         supported"
                    .to_string(),
            });
        }
        _ => map_subexprs(expr, &mut |sub: &Expr| to_record_expr(sub, full_scope))?.kind,
    };
    Ok(Expr {
        kind,
        span: Span::default(),
    })
}

#[allow(clippy::too_many_arguments)]
fn project_grouped_result_columns(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    select: &Select,
    full_scope: &Scope,
    dedup_star: &[std::collections::HashSet<String>],
    agg_slots: &[AggSlot],
    cursor: i32,
) -> Result<(i32, usize), CodegenError> {
    let synthetic_names = agg_field_names(agg_slots);
    let flush_schema = joined_record_schema(full_scope, &synthetic_names);
    let flush_scope = record_scope(&flush_schema, cursor, full_scope);
    let mut regs = Vec::new();
    let read_offset = |em: &mut Emitter, reg: &mut RegAlloc, abs: usize| -> i32 {
        let r = reg.alloc();
        em.emit(Instruction::new(
            Opcode::Column,
            cursor,
            i32::try_from(abs).unwrap_or(0),
            r,
        ));
        r
    };
    for col in &select.columns {
        match col {
            ResultColumn::Star => {
                for (i, binding) in full_scope.tables.iter().enumerate() {
                    let suppressed = dedup_star.get(i);
                    let base = joined_column_offset(full_scope, i)?;
                    for idx in 0..binding.schema.columns.len() {
                        let Some(name) = binding.schema.columns.get(idx) else {
                            continue;
                        };
                        if suppressed.is_some_and(|s| s.contains(&name.to_ascii_lowercase())) {
                            continue;
                        }
                        regs.push(read_offset(em, reg, base.saturating_add(idx)));
                    }
                }
            }
            ResultColumn::TableStar { table } => {
                let i = full_scope
                    .tables
                    .iter()
                    .position(|b| b.matches_qualifier(table))
                    .ok_or_else(|| CodegenError::UnknownColumn {
                        name: format!("{table}.*"),
                    })?;
                let base = joined_column_offset(full_scope, i)?;
                // `i` was just found by name in `full_scope.tables`; a miss
                // here is a planner bug, not an empty expansion (#232).
                let count = full_scope
                    .tables
                    .get(i)
                    .map(|b| b.schema.columns.len())
                    .ok_or_else(|| CodegenError::Internal {
                        reason: format!("binding {i} for {table}.* vanished from scope"),
                    })?;
                for idx in 0..count {
                    regs.push(read_offset(em, reg, base.saturating_add(idx)));
                }
            }
            ResultColumn::Expr { expr, .. } => {
                // #612: any expression over the group's columns and
                // aggregates, evaluated against its flushed record.
                regs.push(compile_value(
                    em,
                    reg,
                    &flush_scope,
                    &to_record_expr(
                        &substitute_aggregates(expr, agg_slots, &synthetic_names)?,
                        full_scope,
                    )?,
                )?);
            }
        }
    }
    let Some(&first) = regs.first() else {
        let r = reg.alloc();
        return Ok((r, 0));
    };
    let contiguous = regs
        .iter()
        .enumerate()
        .all(|(i, r)| *r == first.saturating_add(i32::try_from(i).unwrap_or(i32::MAX)));
    if contiguous {
        return Ok((first, regs.len()));
    }
    // An expression allocated temporaries between columns: copy every
    // value into a fresh contiguous run, as `compile_row_values` does.
    let dests: Vec<i32> = regs.iter().map(|_| reg.alloc()).collect();
    for (&r, &dest) in regs.iter().zip(&dests) {
        em.emit(Instruction::new(Opcode::Copy, r, dest, 0));
    }
    Ok((dests.first().copied().unwrap_or(first), regs.len()))
}

/// Structural (span-independent) match between an `ORDER BY` term and
/// one already-collected `agg_slots` entry: same function name, same
/// `DISTINCT`-ness, and the same argument once both are rewritten
/// against the flat joined row (#612, spans aside). See [`resolve_group_order_target`]'s doc for why
/// plain `Expr` equality doesn't work here.
fn matches_agg_slot(
    full_scope: &Scope,
    stripped: &Expr,
    slot: &AggSlot,
) -> Result<bool, CodegenError> {
    let ExprKind::FunctionCall {
        name,
        distinct,
        args,
        tail: None,
    } = &stripped.kind
    else {
        return Ok(false);
    };
    if !name.eq_ignore_ascii_case(&slot.name) || *distinct != slot.eph_cursor.is_some() {
        return Ok(false);
    }
    match (args, &slot.arg) {
        (FunctionArgs::Star, None) => Ok(true),
        (FunctionArgs::List(list), None) => Ok(list.is_empty()),
        (FunctionArgs::List(list), Some(slot_arg)) => {
            let [arg_expr] = list.as_slice() else {
                return Ok(false);
            };
            Ok(to_record_expr(arg_expr, full_scope)? == to_record_expr(slot_arg, full_scope)?)
        }
        (FunctionArgs::Star, Some(_)) => Ok(false),
    }
}

/// Where a joined aggregate's `ORDER BY` term sorts the finalized group
/// rows: a field of the record, or (#612) an expression over it whose
/// value is appended to the record handed to the sorter.
enum GroupOrderKey {
    Offset(usize),
    Expr(Expr),
}

/// Resolves one `ORDER BY` term over a finalized group row
/// (`total_width` raw joined columns, then `agg_slots.len()` finalized
/// aggregates), with the collation it sorts under. A whole aggregate
/// call or a bare column is a record field; a position or alias names
/// a result column and sorts under that column's collation; anything
/// else is evaluated against the record.
fn resolve_group_order_target(
    select: &Select,
    full_scope: &Scope,
    agg_slots: &[AggSlot],
    total_width: usize,
    expr: &Expr,
) -> Result<(GroupOrderKey, Collation), CodegenError> {
    let stripped = strip_paren(strip_collate(expr));
    let explicit = collation_of(expr);
    // An `ORDER BY` term is parsed apart from the result columns the
    // slots came from (different spans), so it is matched structurally.
    for (pos, slot) in agg_slots.iter().enumerate() {
        if matches_agg_slot(full_scope, stripped, slot)? {
            let key = GroupOrderKey::Offset(total_width.saturating_add(pos));
            return Ok((key, explicit.unwrap_or(Collation::Binary)));
        }
    }
    match named_result_column(stripped, select, full_scope)? {
        Some(NamedColumn::Offset(abs, collation)) => {
            return Ok((GroupOrderKey::Offset(abs), explicit.unwrap_or(collation)));
        }
        Some(NamedColumn::Expr(target)) => {
            let (key, collation) =
                resolve_group_order_target(select, full_scope, agg_slots, total_width, &target)?;
            return Ok((key, explicit.unwrap_or(collation)));
        }
        None => {}
    }
    let collation = explicit
        .or_else(|| expr_collation(full_scope, stripped))
        .unwrap_or(Collation::Binary);
    if let Some(offset) = joined_bare_column_offset(full_scope, stripped) {
        return Ok((GroupOrderKey::Offset(offset?), collation));
    }
    let key = to_record_expr(
        &substitute_matching_aggregates(stripped, full_scope, agg_slots)?,
        full_scope,
    )?;
    Ok((GroupOrderKey::Expr(key), collation))
}

/// `substitute_aggregates` for an `ORDER BY` term: an aggregate call is
/// matched to its slot structurally ([`matches_agg_slot`]), since the
/// term was parsed apart from the result columns the slots came from.
fn substitute_matching_aggregates(
    expr: &Expr,
    full_scope: &Scope,
    agg_slots: &[AggSlot],
) -> Result<Expr, CodegenError> {
    for (pos, slot) in agg_slots.iter().enumerate() {
        if matches_agg_slot(full_scope, strip_paren(expr), slot)? {
            return Ok(Expr {
                kind: ExprKind::Column {
                    table: None,
                    catalog: None,
                    name: format!("__agg{pos}"),
                },
                span: expr.span,
            });
        }
    }
    map_subexprs(expr, &mut |sub: &Expr| {
        substitute_matching_aggregates(sub, full_scope, agg_slots)
    })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#219/#235).

    use crate::codegen::row::dispatch::{compile_statement, DispatchError};
    use crate::codegen::row::TableSchema;
    use crate::value::Collation;
    use crate::vm::row::{Opcode, Program};

    fn table(name: &str, root_page: u32, cols: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            root_page,
            columns: cols.iter().map(|c| (*c).to_string()).collect(),
            column_types: cols.iter().map(|_| "INTEGER".to_string()).collect(),
            column_collations: cols.iter().map(|_| Collation::Binary).collect(),
            sql: format!("CREATE TABLE {name} ({})", cols.join(", ")),
            ..Default::default()
        }
    }

    /// `a(k, v)` at root 2 and `b(k, w)` at root 3.
    fn two_tables() -> Vec<TableSchema> {
        vec![table("a", 2, &["k", "v"]), table("b", 3, &["k", "w"])]
    }

    fn compile(sql: &str, schemas: &[TableSchema]) -> Result<Program, DispatchError> {
        compile_statement(sql, schemas, &[])
    }

    fn ok(sql: &str, schemas: &[TableSchema]) -> Program {
        match compile(sql, schemas) {
            Ok(p) => p,
            Err(e) => panic!("{sql}: expected Ok, got {e:?}"),
        }
    }

    fn err_text(sql: &str, schemas: &[TableSchema]) -> String {
        match compile(sql, schemas) {
            Ok(p) => panic!("{sql}: expected Err, got program {p:?}"),
            Err(e) => format!("{e:?}"),
        }
    }

    fn has(program: &Program, opcode: Opcode) -> bool {
        program.instructions.iter().any(|i| i.opcode == opcode)
    }

    // ---------------------------------------------------------------------
    // join_796 -- `matches_agg_slot`: an ORDER BY aggregate matches a
    // collected slot only when `name` and `DISTINCT`-ness both agree
    // (`!name_eq || distinct != slot_distinct`). A match sorts on the
    // finalized aggregate; no match falls through to bare-column
    // resolution, which rejects a function call.
    // ---------------------------------------------------------------------
    #[test]
    fn mcdc__codegen_row_select_aggregate_join_matches_agg_slot_ddfa3547__v1_same_name_and_distinctness_matches_the_slot(
    ) {
        let p = ok(
            "SELECT a.k, count(b.w) FROM a JOIN b ON a.k = b.k GROUP BY a.k ORDER BY count(b.w)",
            &two_tables(),
        );
        assert!(has(&p, Opcode::SorterOpen), "{p:?}");
    }

    #[test]
    fn mcdc__codegen_row_select_aggregate_join_matches_agg_slot_ddfa3547__v2_different_name_does_not_match(
    ) {
        let e = err_text(
            "SELECT a.k, count(b.w) FROM a JOIN b ON a.k = b.k GROUP BY a.k ORDER BY sum(b.w)",
            &two_tables(),
        );
        assert!(
            e.contains("Unsupported") || e.contains("UnknownColumn"),
            "{e}"
        );
    }

    #[test]
    fn mcdc__codegen_row_select_aggregate_join_matches_agg_slot_ddfa3547__v3_same_name_but_different_distinctness_does_not_match(
    ) {
        let e = err_text(
            "SELECT a.k, count(b.w) FROM a JOIN b ON a.k = b.k GROUP BY a.k \
             ORDER BY count(DISTINCT b.w)",
            &two_tables(),
        );
        assert!(
            e.contains("Unsupported") || e.contains("UnknownColumn"),
            "{e}"
        );
    }
}
