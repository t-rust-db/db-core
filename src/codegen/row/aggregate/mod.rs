//! `GROUP BY`/`HAVING`/aggregate codegen (db-core#93) -- ported from
//! sqlite-rs's `src/codegen/select/aggregate.rs` plus its
//! `aggregate/{accum,hash,join}.rs`, targeting [`crate::vm::row::Opcode`]
//! directly (the #18 decision).
//!
//! Two grouping strategies, both ported:
//! [`compile_grouped_scan`] is the sort-then-group one (`Sorter*`, real
//! SQLite's `select.c` shape) and [`hash::try_compile_hash_grouped_scan`]
//! the single-pass hash one over #86's `HashAgg*` opcodes. The hash
//! strategy takes any explicit `GROUP BY`; the sort strategy takes the
//! implicit whole-table group (which has one group, so there is nothing
//! to hash) and, via [`join::compile_joined_grouped_scan`], aggregation
//! over a join.
//!
//! **Scoped down from a byte-faithful port**, exactly as
//! [`super`]'s module doc records for the rest of `codegen::row`.
//! db-core's [`Query`] carries `group_by: Vec<String>` (bare column
//! names, never computed expressions) and `SelectItem::Agg(AggFunc,
//! Option<String>)` (one bare-column argument, never a nested
//! expression, never `DISTINCT`), where sqlite-rs's `Select` carries
//! full `Expr`s. So the reference's computed-`GROUP BY`-expression sort
//! keys, its `DISTINCT`-aggregate ephemeral dedup cursors, its
//! aggregate-nested-in-an-expression rewriting, and its `#506`/`#665`
//! sort-record column pruning (a projection analysis over an `Expr`
//! tree db-core has no equivalent of) all have nothing here to act on
//! and are left out rather than ported as dead machinery.
//!
//! `HAVING` references an aggregate by its `SELECT`-item label
//! (`"COUNT(*)"`) rather than by repeating the call, since [`Expr`] has
//! no function-call variant -- the same convention `ORDER BY` already
//! uses for a SELECT-list aggregate (#131). See
//! [`accum::parse_agg_label`].
//!
//! Rejected outright by [`super::select`]'s caller, matching the
//! reference's own documented simplification: aggregation combined with
//! `ORDER BY` or `DISTINCT`, and (per sqlite-rs's `aggregate/join.rs`)
//! `HAVING` combined with a `JOIN`.

pub(super) mod accum;
mod hash;
mod join;

use super::{CodegenError, Emitter, Label, RegAlloc, Result, Scope, TableSchema};
use crate::expr::{Expr, Query};
use crate::vm::row::{Collation, Instruction, Opcode, SortKeyColumn, P4};

use accum::{
    collect_aggregates, count_operand, emit_agg_step, flush_group, read_pseudo_column,
    read_row_columns_into,
};

pub(super) use accum::query_has_aggregate;
pub(super) use join::compile_joined_grouped_scan;

/// The three cursor slots a grouped scan needs beyond the caller-wired
/// table cursor(s), mirroring sqlite-rs's `ScanCursors`: `sort` is the
/// sorter (or, on the hash path, the hash table -- `SorterOpen` never
/// runs there, so the number is reused, matching the reference's own
/// convention), `pseudo` re-reads each buffered row, and `flush` re-reads
/// each finalized group record.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScanCursors {
    pub(super) table: i32,
    pub(super) sort: i32,
    pub(super) pseudo: i32,
    pub(super) flush: i32,
}

impl ScanCursors {
    /// Numbers the three scratch cursors past `highest`, the highest
    /// cursor slot the caller already wired up.
    pub(super) fn past(table: i32, highest: i32) -> Self {
        ScanCursors {
            table,
            sort: highest.saturating_add(1),
            pseudo: highest.saturating_add(2),
            flush: highest.saturating_add(3),
        }
    }
}

/// Resolves every `GROUP BY` term to its column index in `schema`.
fn group_column_indices(query: &Query, schema: &TableSchema) -> Result<Vec<usize>> {
    query
        .group_by
        .iter()
        .map(|name| {
            schema
                .column_index(name)
                .ok_or_else(|| CodegenError::UnknownColumn(name.clone()))
        })
        .collect()
}

/// The `P4` a group-boundary `Eq` compares one key column under: the
/// column's comparison affinity, and BINARY collation (db-core's
/// [`TableSchema`] carries no per-column `COLLATE`; see
/// [`accum::emit_agg_step`]'s note).
fn group_key_p4(scope: &Scope, name: &str) -> P4 {
    let affinity = crate::vm::row::comparison_affinity(
        super::value::expr_affinity(scope, &Expr::Column(name.to_string())),
        None,
    );
    super::p4_coll_seq(Collation::Binary, affinity)
}

/// Emits the group-boundary test, ported verbatim from sqlite-rs: the
/// very first row is always a boundary; afterwards a boundary is any key
/// column that differs from the previous row's, with NULL treated as
/// equal to NULL (`IsNull`/`NotNull` guards) rather than as SQL's
/// three-valued "unknown", which is what makes `GROUP BY` collect NULL
/// keys into one group instead of scattering them.
///
/// Returns `(boundary_label, not_boundary_label)`; both are unplaced --
/// the caller places them around its accumulate/flush bodies.
fn emit_boundary_check(
    em: &mut Emitter,
    cur_key_regs: &[i32],
    prev_key_regs: &[i32],
    key_p4s: &[P4],
    have_group_reg: i32,
    zero_reg: i32,
) -> (Label, Label) {
    let boundary_label = em.new_label();
    let not_boundary_label = em.new_label();
    let first_row_check = em.emit(Instruction::new(Opcode::Eq, have_group_reg, 0, zero_reg));
    em.patch_p2(first_row_check, boundary_label);
    for ((&cur, &prev), p4) in cur_key_regs.iter().zip(prev_key_regs).zip(key_p4s) {
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
    (boundary_label, not_boundary_label)
}

/// Compiles a single-table aggregate scan, choosing a strategy the way
/// sqlite-rs's own `entry.rs` dispatch does: an explicit `GROUP BY` goes
/// to the O(n) hash strategy, and anything it declines (here: the
/// implicit whole-table group, which has exactly one group and so
/// nothing to hash) falls back to the always-correct sort-then-group
/// strategy.
#[allow(clippy::too_many_arguments)]
pub(super) fn compile_aggregate_scan<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Query,
    schema: &TableSchema,
    cursors: ScanCursors,
    limit_reg: Option<i32>,
    end_label: Label,
    sink: &mut F,
) -> Result<()>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, usize) -> Result<()>,
{
    if hash::try_compile_hash_grouped_scan(
        em, reg, query, schema, cursors, limit_reg, end_label, sink,
    )? {
        return Ok(());
    }
    let implicit_group = query.group_by.is_empty();
    compile_grouped_scan(
        em,
        reg,
        query,
        schema,
        cursors,
        limit_reg,
        end_label,
        implicit_group,
        sink,
    )
}

/// `GROUP BY`/`HAVING` by sort-then-group, mirroring real SQLite's
/// `select.c` shape (and sqlite-rs's port of it): pass 1 buffers every
/// `WHERE`-matching row into a sorter keyed by the `GROUP BY` columns,
/// pass 2 walks the sorted stream detecting key changes as group
/// boundaries, folding one aggregate-context slot per aggregate call and
/// flushing a finalized row at each boundary (and once more after the
/// loop, for the final group).
///
/// `implicit_group` selects #287's whole-table behavior for a `SELECT`
/// with aggregates but no `GROUP BY` key: every row belongs to one
/// synthetic group, and -- unlike an explicit `GROUP BY`, which
/// correctly produces zero groups over zero matching rows -- exactly one
/// row is still emitted when nothing matched (`COUNT(*)` finalizing to
/// 0, every other aggregate to NULL).
#[allow(clippy::too_many_arguments)]
pub(super) fn compile_grouped_scan<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    query: &Query,
    schema: &TableSchema,
    cursors: ScanCursors,
    limit_reg: Option<i32>,
    end_label: Label,
    implicit_group: bool,
    sink: &mut F,
) -> Result<()>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, i32, usize) -> Result<()>,
{
    let scope = Scope::single(schema.clone(), cursors.table);
    let group_indices = group_column_indices(query, schema)?;
    let agg_slots = collect_aggregates(query)?;

    // Pass 1: buffer every WHERE-matching row, sorted by the GROUP BY
    // key. The record is every schema column in declared order -- db-core
    // has no computed `GROUP BY` expression to append a trailing key
    // register for, so a key's record index is just its column index.
    let sort_keys: Vec<SortKeyColumn> = group_indices
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

    let scan_rewind = em.emit(Instruction::new(Opcode::Rewind, cursors.table, 0, 0));
    let sort_step = em.new_label();
    em.patch_p2(scan_rewind, sort_step);
    let scan_loop = em.new_label();
    em.place(scan_loop);

    let scan_skip = em.new_label();
    emit_where(em, reg, &scope, query, scan_skip)?;

    let first = compile_full_row(em, reg, schema, cursors.table)?;
    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        first,
        count_operand(schema.columns.len())?,
        record_reg,
    ));
    em.emit(Instruction::new(
        Opcode::SorterInsert,
        cursors.sort,
        record_reg,
        0,
    ));

    em.place(scan_skip);
    let scan_next = em.emit(Instruction::new(Opcode::Next, cursors.table, 0, 0));
    em.patch_p2(scan_next, scan_loop);

    // Pass 2: walk the sorted buffer, grouping and aggregating.
    em.place(sort_step);
    let sort_addr = em.emit(Instruction::new(Opcode::SorterSort, cursors.sort, 0, 0));
    // `SorterSort` jumps past pass 2 entirely when nothing matched. An
    // explicit `GROUP BY` then has zero groups, so `end_label` is
    // correct as is; the implicit whole-table group still owes one row,
    // so it lands on the same unconditional tail flush the normal
    // end-of-loop path falls through to.
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

    let prev_key_regs: Vec<i32> = group_indices.iter().map(|_| reg.alloc()).collect();
    let snapshot_regs: Vec<i32> = schema.columns.iter().map(|_| reg.alloc()).collect();
    // NULL-initialized so the implicit whole-table group's tail flush
    // over a zero-row table (where nothing ever overwrites these) reads
    // NULL for a plain column, SQLite's "arbitrary row" semantics
    // degrading to NULL when there is no row. Harmless for an explicit
    // `GROUP BY`: every real group overwrites them before its flush.
    for &r in &snapshot_regs {
        em.emit(Instruction::new(Opcode::Null, 0, r, 0));
    }

    // `OpenPseudo` records only `cursors.pseudo -> sorter_data_reg` (the
    // register index, not its value), so it runs once, outside the loop;
    // `SorterData` refreshes the register's contents per row.
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

    let mut cur_key_regs = Vec::with_capacity(group_indices.len());
    for &idx in &group_indices {
        let r = reg.alloc();
        read_pseudo_column(em, cursors.pseudo, idx, r)?;
        cur_key_regs.push(r);
    }
    let key_p4s: Vec<P4> = query
        .group_by
        .iter()
        .map(|name| group_key_p4(&scope, name))
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
    em.place(skip_flush);
    for (&cur, &prev) in cur_key_regs.iter().zip(&prev_key_regs) {
        em.emit(Instruction::new(Opcode::Copy, cur, prev, 0));
    }
    em.emit(Instruction::new(Opcode::Integer, 1, have_group_reg, 0));
    for agg in &agg_slots {
        emit_agg_step(em, reg, &pseudo_scope(schema, cursors.pseudo), agg, true)?;
    }
    // A plain (non-aggregate) column has no aggregate to fold, so it
    // takes an "arbitrary row" from the group instead -- and SQLite's own
    // sort-then-group strategy observably picks the group's *first* row,
    // so the snapshot happens once, here, on the boundary row.
    read_row_columns_into(em, cursors.pseudo, &snapshot_regs)?;
    let after_accumulate = em.new_label();
    let goto_after_accumulate = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
    em.patch_p2(goto_after_accumulate, after_accumulate);

    em.place(not_boundary_label);
    for agg in &agg_slots {
        emit_agg_step(em, reg, &pseudo_scope(schema, cursors.pseudo), agg, false)?;
    }

    em.place(after_accumulate);
    let sorted_next = em.emit(Instruction::new(Opcode::SorterNext, cursors.sort, 0, 0));
    em.patch_p2(sorted_next, sorted_loop);

    // Tail flush: the last group never sees another row to trigger the
    // mid-loop boundary flush.
    if implicit_group {
        em.place(empty_sorter_target);
    }
    let skip_tail_flush = em.new_label();
    if !implicit_group {
        let tail_check = em.emit(Instruction::new(Opcode::Eq, have_group_reg, 0, zero_reg));
        em.patch_p2(tail_check, skip_tail_flush);
    }
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
    em.place(skip_tail_flush);
    Ok(())
}

/// The scope pass 2 compiles aggregate arguments against: the same
/// schema, but bound to the pseudo cursor re-reading the buffered row
/// rather than to the real table cursor.
fn pseudo_scope(schema: &TableSchema, pseudo_cursor: i32) -> Scope {
    let mut schema = schema.clone();
    // The buffered record is a materialized row, so its rowid-alias
    // column is an ordinary field within it -- `Opcode::Rowid` against a
    // pseudo cursor would read the wrong thing.
    schema.rowid_alias = None;
    Scope::single(schema, pseudo_cursor)
}

fn emit_where(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    query: &Query,
    skip: Label,
) -> Result<()> {
    if let Some(where_expr) = &query.where_clause {
        super::compile_cond(
            em,
            reg,
            scope,
            where_expr,
            super::CondTargets::null_is_false(
                super::Target::Fallthrough,
                super::Target::Jump(skip),
            ),
        )?;
    }
    Ok(())
}

/// Reads every one of `schema`'s columns off `cursor` into a contiguous
/// freshly-allocated register run, returning its first register --
/// pass 1's `MakeRecord` source.
fn compile_full_row(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    cursor: i32,
) -> Result<i32> {
    let mut first = None;
    for idx in 0..schema.columns.len() {
        let r = reg.alloc();
        first.get_or_insert(r);
        super::value::emit_column_read(em, schema, cursor, idx, r)?;
    }
    Ok(first.unwrap_or_else(|| reg.alloc()))
}
