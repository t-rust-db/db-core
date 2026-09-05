//! Aggregation over a join -- ported from sqlite-rs's
//! `codegen/select/aggregate/join.rs`.
//!
//! Same sort-then-group shape as [`super::compile_grouped_scan`],
//! generalized from a single [`TableSchema`]/cursor to a joined
//! [`Scope`]: pass 1 buffers every `WHERE`-matching joined row -- both
//! bindings' columns, flat and concatenated -- into the sorter keyed by
//! the `GROUP BY` columns; pass 2 walks that buffer through one flat
//! pseudo cursor, detecting boundaries and accumulating by *absolute
//! column offset* rather than by a `TableSchema`-relative index.
//!
//! Bounded MVP scope, documented rather than silently wrong, and
//! mirroring the reference's own list:
//! - `HAVING` combined with a `JOIN` is rejected outright.
//! - Only `INNER`/`LEFT` joins aggregate. `FULL OUTER`'s second
//!   (right-outer) pass would have to feed the same sorter a second
//!   time; `RIGHT`/`CROSS` have no non-aggregate counterpart in
//!   [`super::super::select`] either.
//!
//! None of these apply to the single-table path.

use super::super::select::{build_join_cond, emit_limit_guard};
use super::super::{CodegenError, CondTargets, Emitter, Label, RegAlloc, Result, Scope, Target};
use super::accum::{
    agg_label, collect_aggregates, column_operand, count_operand, emit_agg_final, AggSlot,
};
use super::{emit_boundary_check, group_key_p4};
use crate::expr::{JoinKind, Query, SelectItem};
use crate::vm::row::{Collation, Instruction, Opcode, SortKeyColumn, P4};

/// [`super::compile_grouped_scan`]'s joined counterpart: `GROUP BY`, or
/// an implicit whole-table aggregate, combined with a single equi-join.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::codegen::row) fn compile_joined_grouped_scan<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Query,
    scope: &Scope,
    cursors: super::ScanCursors,
    right_cursor: i32,
    limit_reg: Option<i32>,
    end_label: Label,
    sink: &mut F,
) -> Result<()>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, usize) -> Result<()>,
{
    if query.having.is_some() {
        return Err(CodegenError::Unsupported {
            reason: "HAVING combined with a JOIN is not yet supported".to_string(),
        });
    }
    let Some(join) = query.joins.first() else {
        return Err(CodegenError::Unsupported {
            reason: "compile_joined_grouped_scan requires a JOIN".to_string(),
        });
    };
    if !matches!(join.kind, JoinKind::Inner | JoinKind::Left) {
        return Err(CodegenError::Unsupported {
            reason: "only INNER/LEFT JOIN can be aggregated over".to_string(),
        });
    }
    let Some((right_schema, _)) = &scope.right else {
        return Err(CodegenError::Unsupported {
            reason: "compile_joined_grouped_scan requires a right-hand binding".to_string(),
        });
    };
    let left_width = scope.schema.columns.len();
    let total_width = left_width.saturating_add(right_schema.columns.len());

    let group_offsets: Vec<usize> = query
        .group_by
        .iter()
        .map(|name| column_offset(scope, right_cursor, left_width, name))
        .collect::<Result<_>>()?;
    let agg_slots = collect_aggregates(query)?;
    let implicit_group = query.group_by.is_empty();

    // Pass 1: buffer every WHERE-matching joined row, sorted by the
    // GROUP BY key.
    let sort_keys: Vec<SortKeyColumn> = group_offsets
        .iter()
        .map(|&index| SortKeyColumn {
            index,
            descending: false,
            collation: Collation::Binary,
            nulls_first: true,
        })
        .collect();
    em.emit(Instruction::with_p4(
        Opcode::SorterOpen,
        cursors.sort,
        0,
        0,
        P4::SortKey(sort_keys),
    ));

    let outer_rewind = em.emit(Instruction::new(Opcode::Rewind, cursors.table, 0, 0));
    let sort_step = em.new_label();
    em.patch_p2(outer_rewind, sort_step);
    let outer_loop = em.new_label();
    em.place(outer_loop);
    let outer_skip = em.new_label();

    // `LEFT` null-extends an outer row that matched no inner row -- the
    // flag is per outer row, so it is (re-)zeroed here inside the loop,
    // matching `select::compile_join_body`.
    let matched_reg = if join.kind == JoinKind::Left {
        let r = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, r, 0));
        Some(r)
    } else {
        None
    };

    let inner_end = em.new_label();
    let inner_rewind = em.emit(Instruction::new(Opcode::Rewind, right_cursor, 0, 0));
    em.patch_p2(inner_rewind, inner_end);
    let inner_loop = em.new_label();
    em.place(inner_loop);
    let inner_skip = em.new_label();

    let join_cond = build_join_cond(scope, join);
    super::super::compile_cond(
        em,
        reg,
        scope,
        &join_cond,
        CondTargets::null_is_false(Target::Fallthrough, Target::Jump(inner_skip)),
    )?;
    if let Some(m) = matched_reg {
        em.emit(Instruction::new(Opcode::Integer, 1, m, 0));
    }
    if let Some(where_expr) = &query.where_clause {
        super::super::compile_cond(
            em,
            reg,
            scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(inner_skip)),
        )?;
    }
    buffer_joined_row(em, reg, scope, right_cursor, cursors.sort, None)?;

    em.place(inner_skip);
    let inner_next = em.emit(Instruction::new(Opcode::Next, right_cursor, 0, 0));
    em.patch_p2(inner_next, inner_loop);
    em.place(inner_end);

    if let Some(m) = matched_reg {
        let zero = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, zero, 0));
        let eq_addr = em.emit(Instruction::new(Opcode::Eq, m, 0, zero));
        let null_ext = em.new_label();
        em.patch_p2(eq_addr, null_ext);
        em.goto(outer_skip);
        em.place(null_ext);
        buffer_joined_row(
            em,
            reg,
            scope,
            right_cursor,
            cursors.sort,
            Some(right_cursor),
        )?;
    }

    em.place(outer_skip);
    let outer_next = em.emit(Instruction::new(Opcode::Next, cursors.table, 0, 0));
    em.patch_p2(outer_next, outer_loop);

    // Pass 2: walk the sorted buffer, grouping and aggregating.
    em.place(sort_step);
    let sort_addr = em.emit(Instruction::new(Opcode::SorterSort, cursors.sort, 0, 0));
    let empty_sorter_target = if implicit_group {
        em.new_label()
    } else {
        end_label
    };
    em.patch_p2(sort_addr, empty_sorter_target);

    let zero_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, 0, zero_reg, 0));
    let have_group_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Integer, 0, have_group_reg, 0));

    let prev_key_regs: Vec<i32> = group_offsets.iter().map(|_| reg.alloc()).collect();
    let snapshot_regs: Vec<i32> = (0..total_width).map(|_| reg.alloc()).collect();
    for &r in &snapshot_regs {
        em.emit(Instruction::new(Opcode::Null, 0, r, 0));
    }

    let sorter_data_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::OpenPseudo,
        cursors.pseudo,
        sorter_data_reg,
        0,
    ));
    let sorted_loop = em.new_label();
    em.place(sorted_loop);
    em.emit(Instruction::new(
        Opcode::SorterData,
        cursors.sort,
        sorter_data_reg,
        0,
    ));

    let mut cur_key_regs = Vec::with_capacity(group_offsets.len());
    for &offset in &group_offsets {
        cur_key_regs.push(read_offset(em, reg, cursors.pseudo, offset)?);
    }
    let key_p4s: Vec<P4> = query
        .group_by
        .iter()
        .map(|name| group_key_p4(scope, name))
        .collect();

    let (boundary_label, not_boundary_label) = emit_boundary_check(
        em,
        &cur_key_regs,
        &prev_key_regs,
        &key_p4s,
        have_group_reg,
        zero_reg,
    );

    em.place(boundary_label);
    let skip_flush = em.new_label();
    let flush_check = em.emit(Instruction::new(Opcode::Eq, have_group_reg, 0, zero_reg));
    em.patch_p2(flush_check, skip_flush);
    flush_joined_group(
        em,
        reg,
        query,
        scope,
        right_cursor,
        left_width,
        total_width,
        cursors.flush,
        &snapshot_regs,
        &agg_slots,
        limit_reg,
        end_label,
        sink,
    )?;
    em.place(skip_flush);
    for (&cur, &prev) in cur_key_regs.iter().zip(&prev_key_regs) {
        em.emit(Instruction::new(Opcode::Copy, cur, prev, 0));
    }
    em.emit(Instruction::new(Opcode::Integer, 1, have_group_reg, 0));
    for agg in &agg_slots {
        emit_joined_agg_step(
            em,
            reg,
            scope,
            right_cursor,
            left_width,
            cursors.pseudo,
            agg,
            true,
        )?;
    }
    for (idx, &dest) in snapshot_regs.iter().enumerate() {
        em.emit(Instruction::new(
            Opcode::Column,
            cursors.pseudo,
            column_operand(idx)?,
            dest,
        ));
    }
    let after_accumulate = em.new_label();
    let goto_after_accumulate = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
    em.patch_p2(goto_after_accumulate, after_accumulate);

    em.place(not_boundary_label);
    for agg in &agg_slots {
        emit_joined_agg_step(
            em,
            reg,
            scope,
            right_cursor,
            left_width,
            cursors.pseudo,
            agg,
            false,
        )?;
    }

    em.place(after_accumulate);
    let sorted_next = em.emit(Instruction::new(Opcode::SorterNext, cursors.sort, 0, 0));
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
        query,
        scope,
        right_cursor,
        left_width,
        total_width,
        cursors.flush,
        &snapshot_regs,
        &agg_slots,
        limit_reg,
        end_label,
        sink,
    )?;
    em.place(skip_tail_flush);
    Ok(())
}

/// One (possibly `table.`-qualified) column name's absolute offset in
/// the flat joined row: the left binding's columns, then the right's.
fn column_offset(scope: &Scope, right_cursor: i32, left_width: usize, name: &str) -> Result<usize> {
    let (cursor, idx) = scope.resolve(name)?;
    if cursor == right_cursor {
        return Ok(left_width.saturating_add(idx));
    }
    Ok(idx)
}

/// Serializes one joined row -- every left column then every right
/// column -- into the sorter. `null_cursor`, when set, NULL-fills every
/// column that would be read from it: `LEFT`'s unmatched-row
/// null-extension, where that cursor holds no current row.
fn buffer_joined_row(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    right_cursor: i32,
    sort_cursor: i32,
    null_cursor: Option<i32>,
) -> Result<()> {
    let Some((right_schema, _)) = &scope.right else {
        return Err(CodegenError::Unsupported {
            reason: "compile_joined_grouped_scan requires a right-hand binding".to_string(),
        });
    };
    let mut first = None;
    let mut count = 0usize;
    for (schema, cursor) in [(&scope.schema, scope.cursor), (right_schema, right_cursor)] {
        for idx in 0..schema.columns.len() {
            let r = reg.alloc();
            first.get_or_insert(r);
            count = count.saturating_add(1);
            if Some(cursor) == null_cursor {
                em.emit(Instruction::new(Opcode::Null, 0, r, 0));
            } else {
                super::super::value::emit_column_read(em, schema, cursor, idx, r)?;
            }
        }
    }
    let first = first.unwrap_or_else(|| reg.alloc());
    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        first,
        count_operand(count)?,
        record_reg,
    ));
    em.emit(Instruction::new(
        Opcode::SorterInsert,
        sort_cursor,
        record_reg,
        0,
    ));
    Ok(())
}

fn read_offset(em: &mut Emitter, reg: &mut RegAlloc, cursor: i32, offset: usize) -> Result<i32> {
    let r = reg.alloc();
    em.emit(Instruction::new(
        Opcode::Column,
        cursor,
        column_operand(offset)?,
        r,
    ));
    Ok(r)
}

/// [`super::accum::emit_agg_step`]'s joined counterpart: reads the
/// argument straight off the flat pseudo cursor at its resolved absolute
/// offset, instead of compiling a general expression against a `Scope`.
#[allow(clippy::too_many_arguments)]
fn emit_joined_agg_step(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    right_cursor: i32,
    left_width: usize,
    pseudo_cursor: i32,
    agg: &AggSlot,
    reset: bool,
) -> Result<()> {
    let (arg_reg, arity) = match &agg.arg {
        Some(name) => {
            let offset = column_offset(scope, right_cursor, left_width, name)?;
            (Some(read_offset(em, reg, pseudo_cursor, offset)?), 1usize)
        }
        None => (None, 0usize),
    };
    let mut instr = Instruction::with_p4(
        Opcode::AggStep,
        agg.slot,
        arg_reg.unwrap_or(0),
        0,
        P4::AggFunc {
            name: agg.func.name().to_ascii_lowercase(),
            arity,
            collation: Collation::Binary,
        },
    );
    if reset {
        instr.p5 = 1;
    }
    em.emit(instr);
    Ok(())
}

/// [`super::accum::flush_group`]'s joined counterpart: finalizes one
/// group into a `total_width + agg_slots.len()`-wide record (the group's
/// snapshot joined row followed by each aggregate's finalized value),
/// then reprojects it by absolute offset. There is no `Scope`-based
/// re-resolution here (and so no `HAVING`, rejected up front): a flat
/// joined record has two bindings' column names in it, which a
/// single-table synthetic schema could not disambiguate.
#[allow(clippy::too_many_arguments)]
fn flush_joined_group<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Query,
    scope: &Scope,
    right_cursor: i32,
    left_width: usize,
    total_width: usize,
    flush_cursor: i32,
    snapshot_regs: &[i32],
    agg_slots: &[AggSlot],
    limit_reg: Option<i32>,
    end_label: Label,
    sink: &mut F,
) -> Result<()>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, usize) -> Result<()>,
{
    let synthetic_count = total_width.saturating_add(agg_slots.len());
    let dests: Vec<i32> = (0..synthetic_count).map(|_| reg.alloc()).collect();
    let synthetic_first = dests.first().copied().unwrap_or_else(|| reg.alloc());
    for (&snap, &dest) in snapshot_regs.iter().zip(&dests) {
        em.emit(Instruction::new(Opcode::Copy, snap, dest, 0));
    }
    let agg_dests = dests.get(total_width..).unwrap_or(&[]);
    for (agg, &dest) in agg_slots.iter().zip(agg_dests) {
        emit_agg_final(em, agg, dest);
    }
    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        synthetic_first,
        count_operand(synthetic_count)?,
        record_reg,
    ));
    em.emit(Instruction::new(
        Opcode::OpenPseudo,
        flush_cursor,
        record_reg,
        0,
    ));
    if let Some(limit_reg) = limit_reg {
        emit_limit_guard(em, limit_reg, end_label);
    }

    let offsets = projected_offsets(
        query,
        scope,
        right_cursor,
        left_width,
        total_width,
        agg_slots,
    )?;
    let mut regs = Vec::with_capacity(offsets.len());
    for offset in offsets {
        regs.push(read_offset(em, reg, flush_cursor, offset)?);
    }
    let first = regs.first().copied().unwrap_or_else(|| reg.alloc());
    sink(em, reg, first, regs.len())?;
    Ok(())
}

/// Each result column's absolute offset within a finalized group record:
/// `total_width` raw joined columns followed by one finalized value per
/// aggregate, in `agg_slots` order.
fn projected_offsets(
    query: &Query,
    scope: &Scope,
    right_cursor: i32,
    left_width: usize,
    total_width: usize,
    agg_slots: &[AggSlot],
) -> Result<Vec<usize>> {
    let mut offsets = Vec::with_capacity(query.columns.len());
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => {
                offsets.push(column_offset(scope, right_cursor, left_width, name)?);
            }
            SelectItem::Star => offsets.extend(0..total_width),
            SelectItem::Agg(func, arg) => {
                let label = agg_label(*func, arg.as_deref());
                let pos = agg_slots
                    .iter()
                    .position(|a| a.label == label)
                    .ok_or_else(|| CodegenError::UnknownColumn(label.clone()))?;
                offsets.push(total_width.saturating_add(pos));
            }
            SelectItem::Window(_) => {
                return Err(CodegenError::Unsupported {
                    reason: "window functions are not supported by codegen::row".to_string(),
                })
            }
        }
    }
    Ok(offsets)
}
