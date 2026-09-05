//! Hash-based `GROUP BY` codegen -- ported from sqlite-rs's
//! `codegen/select/aggregate/hash.rs`, over db-core#86's `HashAgg*`
//! opcode slice.

use super::super::{Emitter, Label, RegAlloc, Result, Scope, TableSchema};
use super::accum::{
    collect_aggregates, count_operand, flush_group, read_row_columns_into, AggSlot,
};
use super::{compile_full_row, emit_where, group_column_indices};
use crate::expr::{Expr, Query};
use crate::vm::row::{Collation, GroupKeyColumn, Instruction, Opcode, P4};

/// Compiles an explicit `GROUP BY` as a single-pass hash aggregation:
/// each `WHERE`-matching row is folded straight into its group's
/// accumulators at scan time (`HashAggFind` plus one `HashAggStep` per
/// aggregate call), then the groups are walked once
/// (`HashAggRewind`/`HashAggData`/`HashAggNext`) and flushed through the
/// very same [`flush_group`] the sort strategy uses.
///
/// The win over [`super::compile_grouped_scan`] is asymptotic, not
/// constant-factor: the sort strategy buffers all `n` rows and sorts
/// them, O(n log n), purely so a group's rows end up adjacent; folding
/// into a hash table needs no adjacency, so the build is O(n) and only
/// the `K` groups are ever ordered. Memory is strictly better too: one
/// retained row per *group* rather than per *row*.
///
/// Returns `Ok(true)` when this path was taken; `Ok(false)` leaves
/// `em`/`reg` untouched so the caller falls back to the sort strategy,
/// which stays the always-correct general path. The one narrowing (the
/// reference's own): no explicit `GROUP BY` key at all -- the implicit
/// whole-table group is a single group, so there is nothing to hash and
/// nothing to save.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_compile_hash_grouped_scan<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Query,
    schema: &TableSchema,
    cursors: super::ScanCursors,
    limit_reg: Option<i32>,
    end_label: Label,
    sink: &mut F,
) -> Result<bool>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, usize) -> Result<()>,
{
    if query.group_by.is_empty() {
        return Ok(false);
    }
    let scope = Scope::single(schema.clone(), cursors.table);
    let group_indices = group_column_indices(query, schema)?;
    let agg_slots = collect_aggregates(query)?;

    // The hash table reuses the sort cursor's number: `SorterOpen` never
    // runs on this branch (the reference's own convention).
    let hash_cursor = cursors.sort;
    // The same collation and comparison affinity the sort strategy puts
    // on its group-boundary `Eq` -- hash equality has to agree with that
    // comparison exactly, or the two strategies would group differently.
    let group_keys: Vec<GroupKeyColumn> = query
        .group_by
        .iter()
        .zip(&group_indices)
        .map(|(name, &index)| GroupKeyColumn {
            index,
            collation: Collation::Binary,
            affinity: crate::vm::row::comparison_affinity(
                super::super::value::expr_affinity(&scope, &Expr::Column(name.clone())),
                None,
            )
            .to_p4_byte(),
        })
        .collect();
    em.emit(Instruction::with_p4(
        Opcode::HashAggOpen,
        hash_cursor,
        0,
        0,
        P4::GroupKey(group_keys),
    ));

    // The one and only pass over the table: filter, project, fold.
    let scan_rewind = em.emit(Instruction::new(Opcode::Rewind, cursors.table, 0, 0));
    let scan_done = em.new_label();
    em.patch_p2(scan_rewind, scan_done);
    let scan_loop = em.new_label();
    em.place(scan_loop);

    let scan_skip = em.new_label();
    emit_where(em, reg, &scope, query, scan_skip)?;

    // Identical record layout to the sort strategy's pass 1 (every
    // schema column in declared order), which is what lets the retained
    // group row be read back through an ordinary `OpenPseudo` cursor
    // below, and what makes `P4::GroupKey`'s indices plain column
    // indices.
    let first = compile_full_row(em, reg, schema, cursors.table)?;
    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        first,
        count_operand(schema.columns.len())?,
        record_reg,
    ));
    em.emit(Instruction::new(
        Opcode::HashAggFind,
        hash_cursor,
        record_reg,
        0,
    ));
    for agg in &agg_slots {
        emit_hash_agg_step(em, reg, &scope, hash_cursor, agg)?;
    }

    em.place(scan_skip);
    let scan_next = em.emit(Instruction::new(Opcode::Next, cursors.table, 0, 0));
    em.patch_p2(scan_next, scan_loop);

    // Walk the groups.
    em.place(scan_done);
    let snapshot_regs: Vec<i32> = schema.columns.iter().map(|_| reg.alloc()).collect();
    let group_row_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::OpenPseudo,
        cursors.pseudo,
        group_row_reg,
        0,
    ));
    // An explicit `GROUP BY` over zero matching rows produces zero
    // groups, so an empty table jumps straight past every flush -- the
    // implicit whole-table group's "still emit one row" case never
    // reaches this path (declined above).
    let rewind_addr = em.emit(Instruction::new(Opcode::HashAggRewind, hash_cursor, 0, 0));
    em.patch_p2(rewind_addr, end_label);

    let group_loop = em.new_label();
    em.place(group_loop);
    em.emit(Instruction::new(
        Opcode::HashAggData,
        hash_cursor,
        group_row_reg,
        0,
    ));
    read_row_columns_into(em, cursors.pseudo, &snapshot_regs)?;
    flush_group(
        em,
        reg,
        query,
        &schema.columns,
        &snapshot_regs,
        &agg_slots,
        limit_reg,
        cursors.flush,
        end_label,
        sink,
    )?;
    let group_next = em.emit(Instruction::new(Opcode::HashAggNext, hash_cursor, 0, 0));
    em.patch_p2(group_next, group_loop);
    Ok(true)
}

/// [`super::accum::emit_agg_step`]'s hash-table counterpart: the same
/// argument expression against the same scope, the same `P4::AggFunc`
/// descriptor, the same `crate::vm::row::aggregate::step` kernel. The
/// only differences are the target (the located group's accumulators
/// rather than the VM-wide context slot) and the absence of a `reset`
/// flag -- a hash table gives each group its own accumulator by
/// construction, so there is no reused slot to discard.
fn emit_hash_agg_step(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    hash_cursor: i32,
    agg: &AggSlot,
) -> Result<()> {
    let (arg_reg, arity) = match &agg.arg {
        Some(name) => (
            Some(super::super::compile_value(
                em,
                reg,
                scope,
                &Expr::Column(name.clone()),
            )?),
            1usize,
        ),
        None => (None, 0usize),
    };
    em.emit(Instruction::with_p4(
        Opcode::HashAggStep,
        agg.slot,
        arg_reg.unwrap_or(0),
        hash_cursor,
        P4::AggFunc {
            name: agg.func.name().to_ascii_lowercase(),
            arity,
            collation: Collation::Binary,
        },
    ));
    Ok(())
}
