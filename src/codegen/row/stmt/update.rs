// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `Update` AST -> `Program` compilation (#210, index maintenance #196,
//! constraint re-validation #218). Mirrors `delete.rs`'s scan shape,
//! but per matched row builds the new record from a mix of assigned
//! expressions and the row's own unassigned columns (read via
//! `emit_column_read` before the row is touched), re-validates NOT
//! NULL/CHECK against the new row the same way `insert.rs` does
//! (reusing its `column_plans`/`ColumnPlan`/`emit_constraint_violation`
//! machinery), then emits per-index `IdxDelete` + `Delete` + `Insert` +
//! per-index `IdxInsert` rather than `Delete`+`Insert` alone — SQLite's
//! own "no in-place update opcode" convention for a b-tree keyed by
//! rowid, since a `SET`-assignment to the rowid-alias column can change
//! the row's key.
//!
//! Known simplification (deferred to a follow-up ticket, not chased
//! here): `DEFAULT` is not substituted for an assigned `NULL` (`SET col
//! = DEFAULT` isn't a thing this parser accepts yet, and an explicit
//! `SET col = NULL` on a NOT NULL column is correctly a violation, not
//! a default substitution — unlike `INSERT ... OR REPLACE`, `UPDATE`
//! has no "explicit NULL means take the default" convention in stock
//! SQLite either). Correctness-neutral.
//!
//! #524: an index is only rebuilt (delete old key, insert new key) on a
//! matched row when the `SET` clause actually assigns one of its
//! columns (or the rowid, which is part of every index's key) —
//! `index_touched` in [`compile_update_with_catalog`], threaded into
//! [`emit_update_row_body`]'s `emit_index_key_ops`/
//! `emit_index_key_ops_from_regs` calls.
//!
//! #336: `WHERE rowid = <int literal|param>` (or the table's `INTEGER
//! PRIMARY KEY` rowid-alias column) compiles to `SeekRowid` instead of
//! the `Rewind`/`Next` scan — same narrow recognition as `delete.rs`'s
//! own #336 fast path (a single top-level equality, nothing compound),
//! reusing the exact same per-row body (`col_regs` construction,
//! constraint checks, index maintenance) either way.
//!
//! Constraint recovery reuses `insert.rs`'s [`cached_create_table`]
//! (#643) instead of calling `parse_create_table` directly, so the DDL
//! text is only tokenized/parsed once across however many
//! INSERT/UPDATE compiles reuse the same schema.

use crate::codegen::row::expr::{column_index, compile_cond, compile_value, emit_column_read};
use crate::codegen::row::first_reg;
use crate::codegen::row::index_maintenance::{
    emit_index_key_ops, emit_index_key_ops_from_regs, open_index_cursors, valid_table_root_page,
};
use crate::codegen::row::planner::Stats;
use crate::codegen::row::select::{
    is_rowid_reference, range_seek_index_position, top_level_equality_operands,
    try_compile_range_row_seek, CodegenError,
};
use crate::codegen::row::stmt::insert::{
    cached_create_table, column_plans, emit_constraint_violation, table_check_constraints,
    ColumnPlan, SQLITE_CONSTRAINT_CHECK, SQLITE_CONSTRAINT_NOTNULL,
};
use crate::codegen::row::TableSchema;
use crate::codegen::row::{CondTargets, Emitter, Label, NullTarget, RegAlloc, Scope, Target};
use crate::parser::ast::{ConflictAction, Expr, ExprKind, Literal, ParamKind, Update};
use crate::vm::row::{affinity_of, Instruction, Opcode, Program, P4};

const TABLE_CURSOR: i32 = 0;
const CHECK_CURSOR: i32 = 1;
const FIRST_INDEX_CURSOR: i32 = 2;

/// Compiles `update` against `schema` (the resolved target table) into
/// a `Program`. `catalog = [schema]` — no cross-table subquery support
/// in `SET`/`WHERE` expressions; use [`compile_update_with_catalog`] for
/// that (#251).
pub fn compile_update(update: &Update, schema: &TableSchema) -> Result<Program, CodegenError> {
    compile_update_with_catalog(update, schema, std::slice::from_ref(schema))
}

/// [`compile_update`], plus `catalog` — the full table catalog, used to
/// resolve a scalar/`IN`/`EXISTS` subquery expression in a `SET` value
/// or `WHERE` clause when it names a table other than `schema` itself
/// (#251).
pub fn compile_update_with_catalog(
    update: &Update,
    schema: &TableSchema,
    catalog: &[TableSchema],
) -> Result<Program, CodegenError> {
    if schema.without_rowid {
        return Err(CodegenError::Unsupported {
            reason: "WITHOUT ROWID tables are not supported by UPDATE codegen yet".to_string(),
        });
    }

    let create = cached_create_table(schema)?;

    let rowid_alias = schema.rowid_alias;
    let plans = column_plans(schema, &create, rowid_alias);
    let table_checks: Vec<(Expr, String)> = table_check_constraints(schema, &create);
    let action = update.or_action.unwrap_or(ConflictAction::Abort);

    // Same rationale as `insert.rs`'s `check_schema`: `CHECK` column
    // references must read via ordinary `Opcode::Column` against the
    // pseudo-cursor built from the new row's record, not `Opcode::Rowid`
    // (which `rowid_alias`-driven codegen would otherwise emit
    // for the rowid-alias column — cleared here alongside `sql`, and which the pseudo-cursor can't
    // answer).
    let check_schema = TableSchema {
        sql: String::new(),
        rowid_alias: None,
        ..schema.clone()
    };

    let mut assigned: Vec<Option<&Expr>> = vec![None; schema.columns.len()];
    for assignment in &update.assignments {
        for name in &assignment.columns {
            let idx = column_index(schema, name)
                .ok_or_else(|| CodegenError::UnknownColumn { name: name.clone() })?;
            if let Some(slot) = assigned.get_mut(idx) {
                *slot = Some(&assignment.value);
            }
        }
    }

    // #524: an index whose key is provably unchanged by this statement
    // doesn't need its b-tree entry rebuilt per matched row. A
    // reassigned rowid changes every index's key (rowid is the last key
    // component), so it forces every index touched; otherwise an index
    // is touched only if the `SET` clause assigns one of its columns. An
    // `IndexedColumn` whose name isn't a plain column (an expression
    // index) can't be proven untouched, so it's conservatively treated
    // as touched — same reasoning `range_seek_touches_scanned_index`
    // below already applies to the single index a range-seek scans.
    let rowid_reassigned = rowid_alias.and_then(|idx| assigned.get(idx).copied().flatten());
    let index_touched: Vec<bool> = schema
        .indexes
        .iter()
        .map(|index| {
            rowid_reassigned.is_some()
                || index.columns.iter().any(|c| {
                    column_index(schema, &c.name)
                        .is_none_or(|idx| assigned.get(idx).is_some_and(Option::is_some))
                })
        })
        .collect();

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    let root_page = valid_table_root_page(schema)?;
    em.emit(Instruction::new(
        Opcode::OpenWrite,
        TABLE_CURSOR,
        root_page,
        0,
    ));
    open_index_cursors(&mut em, schema, FIRST_INDEX_CURSOR)?;

    let scope = crate::codegen::row::Scope::single(schema, TABLE_CURSOR).with_catalog(catalog);
    let end_label = em.new_label();

    let rowid_seek_operand = update
        .where_clause
        .as_ref()
        .and_then(|where_expr| top_level_equality_operands(where_expr))
        .and_then(|(lhs, rhs)| {
            if is_rowid_reference(schema, lhs) {
                Some(rhs)
            } else if is_rowid_reference(schema, rhs) {
                Some(lhs)
            } else {
                None
            }
        })
        .filter(|operand| {
            matches!(
                &operand.kind,
                ExprKind::Literal(Literal::Integer(_))
                    | ExprKind::Param(ParamKind::Anonymous | ParamKind::Numbered(_))
            )
        });

    // #336: on a seek, `row_skip` and `end_label` are the same target —
    // there's exactly one candidate row, so "skip this row" (a
    // constraint violation under `OR IGNORE`) and "no more rows" both
    // mean "we're done". On the ordinary/range-seek scans, they differ
    // as usual: `row_skip` continues the loop, `end_label` exits it.
    if let Some(operand) = rowid_seek_operand {
        let value_reg = compile_value(&mut em, &mut reg, &scope, operand)?;
        let seek_addr = em.emit(Instruction::new(
            Opcode::SeekRowid,
            TABLE_CURSOR,
            0,
            value_reg,
        ));
        em.patch_p2(seek_addr, end_label);
        emit_update_row_body(
            &mut em,
            &mut reg,
            schema,
            &scope,
            &plans,
            &table_checks,
            &check_schema,
            action,
            rowid_alias,
            rowid_reassigned.is_some(),
            &assigned,
            &index_touched,
            end_label,
        )?;
        em.place(end_label);
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        return Ok(em.finish());
    }

    // #666/#675: an index-seek range scan (`WHERE col >/>=/</<= lit` or
    // `BETWEEN`, against a leading-indexed column) in place of the
    // ordinary `Rewind`/`Next` scan + per-row `compile_cond` filter,
    // mirroring `select.rs`'s own range-seek fast paths (#606). The
    // `IdxNext` walk mutates a b-tree it's scanning only when a `SET`
    // column is actually part of *that* index's key — unlike
    // [`TableCursor`]'s snapshotted traversal frames, the index cursor
    // doing the `IdxNext` walk has no protection against a mid-scan
    // mutation of its own b-tree. When no assigned column intersects the
    // scanned index (the common case), the update is safe to apply
    // directly inside the walk (`range_seek_touches_scanned_index ==
    // false`, single pass below). Otherwise (#666's original shape) pass
    // 1 (the `IdxNext` walk, read-only) records each matched rowid into
    // an in-memory ephemeral table, and pass 2 replays those rowids
    // against `TABLE_CURSOR` to do the actual update once the index scan
    // is safely finished — like `delete.rs`'s own #666 fast path.
    //
    // [`TableCursor`]: db_storage::row::btree::TableCursor
    let range_index_cursor =
        FIRST_INDEX_CURSOR.saturating_add(i32::try_from(schema.indexes.len()).unwrap_or(0));
    let eph_cursor = range_index_cursor.saturating_add(1);

    // Only the index the range-seek itself walks matters here — other
    // indexes get rebuilt per matched row regardless of pass count (a
    // separate, documented simplification, see this module's top-level
    // doc comment), and rebuilding *them* doesn't perturb the cursor
    // doing the `IdxNext` walk on `range_index_cursor`. An `IndexedColumn`
    // whose name isn't a plain column (an expression index) can't be
    // proven not to reference an assigned column, so it's conservatively
    // treated as touched.
    let range_seek_touches_scanned_index = update
        .where_clause
        .as_ref()
        // #498: UPDATE's row seek is not stats-gated (no stats reach
        // `compile_update`), so the ungated eligibility is the right one.
        .and_then(|where_expr| {
            range_seek_index_position(where_expr, schema, catalog, &Stats::default())
        })
        .and_then(|position| schema.indexes.get(position))
        .is_some_and(|index| {
            index.columns.iter().any(|c| {
                column_index(schema, &c.name)
                    .is_none_or(|idx| assigned.get(idx).is_some_and(Option::is_some))
            })
        });

    let used_range_seek = if let Some(where_expr) = &update.where_clause {
        if range_seek_touches_scanned_index {
            em.emit(Instruction {
                opcode: Opcode::OpenEphemeral,
                p1: eph_cursor,
                p2: 0,
                p3: 0,
                p4: P4::None,
                p5: 1,
                comment: None,
            });
            let pass1_done = em.new_label();
            let matched = try_compile_range_row_seek(
                &mut em,
                &mut reg,
                where_expr,
                schema,
                &scope,
                range_index_cursor,
                pass1_done,
                &Stats::default(),
                false,
                &mut |em, reg, index_cursor, _row_skip| {
                    let rowid_reg = reg.alloc();
                    em.emit(Instruction::new(
                        Opcode::IdxRowid,
                        index_cursor,
                        rowid_reg,
                        0,
                    ));
                    let seq_reg = reg.alloc();
                    em.emit(Instruction::new(Opcode::Sequence, eph_cursor, seq_reg, 0));
                    let record_reg = reg.alloc();
                    em.emit(Instruction::new(
                        Opcode::MakeRecord,
                        rowid_reg,
                        1,
                        record_reg,
                    ));
                    em.emit(Instruction::new(
                        Opcode::Insert,
                        eph_cursor,
                        seq_reg,
                        record_reg,
                    ));
                    Ok(())
                },
            )?;
            // Pass 1's own "no more rows"/"past the upper bound" exit
            // (both routed to `pass1_done`, not `end_label`) must still
            // fall into pass 2's replay loop below — an empty
            // `eph_cursor` there is a correct, cheap no-op, but skipping
            // straight to `end_label` would skip pass 2 entirely even
            // when rows *were* collected before the bound was hit.
            em.place(pass1_done);
            matched
        } else {
            // #675: no assigned column intersects the scanned index, so
            // it's safe to apply the update directly inside the same
            // `IdxNext` walk instead of deferring it to a second pass —
            // `row_skip` here is the exact label `try_compile_range_row_seek`
            // already places right before its own `IdxNext`, so reusing
            // it for both a failed `SeekRowid` and a constraint-violation
            // skip continues the walk exactly like the two-pass replay
            // loop below does for its own `Next`.
            try_compile_range_row_seek(
                &mut em,
                &mut reg,
                where_expr,
                schema,
                &scope,
                range_index_cursor,
                end_label,
                &Stats::default(),
                false,
                &mut |em, reg, index_cursor, row_skip| {
                    let rowid_reg = reg.alloc();
                    em.emit(Instruction::new(
                        Opcode::IdxRowid,
                        index_cursor,
                        rowid_reg,
                        0,
                    ));
                    let seek_addr = em.emit(Instruction::new(
                        Opcode::SeekRowid,
                        TABLE_CURSOR,
                        0,
                        rowid_reg,
                    ));
                    em.patch_p2(seek_addr, row_skip);
                    emit_update_row_body(
                        em,
                        reg,
                        schema,
                        &scope,
                        &plans,
                        &table_checks,
                        &check_schema,
                        action,
                        rowid_alias,
                        rowid_reassigned.is_some(),
                        &assigned,
                        &index_touched,
                        row_skip,
                    )
                },
            )?
        }
    } else {
        false
    };

    if used_range_seek && range_seek_touches_scanned_index {
        let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, eph_cursor, 0, 0));
        em.patch_p2(rewind_addr, end_label);
        let loop_start = em.new_label();
        em.place(loop_start);

        let rowid_reg = reg.alloc();
        em.emit(Instruction::new(Opcode::Column, eph_cursor, 0, rowid_reg));
        let row_skip = em.new_label();
        let seek_addr = em.emit(Instruction::new(
            Opcode::SeekRowid,
            TABLE_CURSOR,
            0,
            rowid_reg,
        ));
        em.patch_p2(seek_addr, row_skip);

        emit_update_row_body(
            &mut em,
            &mut reg,
            schema,
            &scope,
            &plans,
            &table_checks,
            &check_schema,
            action,
            rowid_alias,
            rowid_reassigned.is_some(),
            &assigned,
            &index_touched,
            row_skip,
        )?;

        em.place(row_skip);
        let next_addr = em.emit(Instruction::new(Opcode::Next, eph_cursor, 0, 0));
        em.patch_p2(next_addr, loop_start);
    } else if !used_range_seek {
        let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, TABLE_CURSOR, 0, 0));
        em.patch_p2(rewind_addr, end_label);
        let loop_start = em.new_label();
        em.place(loop_start);

        let row_skip = em.new_label();
        if let Some(where_expr) = &update.where_clause {
            compile_cond(
                &mut em,
                &mut reg,
                &scope,
                where_expr,
                CondTargets::null_is_false(Target::Fallthrough, Target::Jump(row_skip)),
            )?;
        }

        emit_update_row_body(
            &mut em,
            &mut reg,
            schema,
            &scope,
            &plans,
            &table_checks,
            &check_schema,
            action,
            rowid_alias,
            rowid_reassigned.is_some(),
            &assigned,
            &index_touched,
            row_skip,
        )?;

        em.place(row_skip);
        let next_addr = em.emit(Instruction::new(Opcode::Next, TABLE_CURSOR, 0, 0));
        em.patch_p2(next_addr, loop_start);
    }

    em.place(end_label);
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// Shared per-matched-row body for [`compile_update_with_catalog`]'s
/// three positioning strategies (`SeekRowid` #336 fast path, #666's
/// range-seek fast path, and the ordinary `Rewind`/`Next` scan): reads
/// the new row's values (a mix of `SET`-assigned expressions and the
/// row's own unassigned columns) from `TABLE_CURSOR`'s current row,
/// re-validates NOT NULL/CHECK, then rebuilds the row (`Delete` +
/// `Insert`) and its index entries. `row_skip` is where a constraint
/// violation under `OR IGNORE` jumps — the caller places it and wires
/// whatever "next row" mechanism (or none, for the single-row seek
/// cases) follows.
#[allow(clippy::too_many_arguments)]
fn emit_update_row_body(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    scope: &Scope,
    plans: &[ColumnPlan],
    table_checks: &[(Expr, String)],
    check_schema: &TableSchema,
    action: ConflictAction,
    rowid_alias: Option<usize>,
    rowid_reassigned: bool,
    assigned: &[Option<&Expr>],
    index_touched: &[bool],
    row_skip: Label,
) -> Result<(), CodegenError> {
    // Every value the new row needs — including a possibly-reassigned
    // rowid — is read from the cursor's *current* row before `Delete`
    // below clears it (`cursor::delete` sets `state.current = None`).
    let rowid_reg = match rowid_alias.and_then(|idx| assigned.get(idx).copied().flatten()) {
        Some(expr) => compile_value(em, reg, scope, expr)?,
        None => {
            let r = reg.alloc();
            em.emit(Instruction::new(Opcode::Rowid, TABLE_CURSOR, r, 0));
            r
        }
    };

    let mut col_regs = Vec::with_capacity(schema.columns.len());
    for (idx, expr) in assigned.iter().enumerate() {
        if Some(idx) == rowid_alias {
            let r = reg.alloc();
            em.emit(Instruction::new(Opcode::Null, 0, r, 0));
            col_regs.push(r);
            continue;
        }
        let r = match expr {
            Some(expr) => compile_value(em, reg, scope, expr)?,
            None => {
                let r = reg.alloc();
                emit_column_read(em, schema, TABLE_CURSOR, idx, r)?;
                r
            }
        };
        col_regs.push(r);
    }

    // `compile_value` returns whatever register its expression's *last*
    // sub-computation happened to land in — for anything beyond a bare
    // literal/column reference (e.g. `SET val = val + 1`), that's a
    // scratch register consumed while evaluating the expression's own
    // operands, so `col_regs` collected above can land anywhere,
    // interleaved with other columns' scratch registers, not the
    // contiguous run `MakeRecord` below requires. A second pass
    // `Copy`'s each value into a freshly bump-allocated register, back
    // to back with nothing else allocated in between (mirroring
    // `insert.rs`'s own `compile_column_source`, #141/#261's fix for the
    // same requirement), so the *copies* — not the original scattered
    // registers — form the contiguous run.
    for r in &mut col_regs {
        let dest = reg.alloc();
        em.emit(Instruction::new(Opcode::Copy, *r, dest, 0));
        *r = dest;
    }

    // Re-validate NOT NULL against the new row's values — an unassigned
    // column keeps a value that already passed this check when the row
    // was written, but an assigned one might not have (`insert.rs`
    // documents the same per-column `IsNull` pattern this mirrors).
    for (idx, plan) in plans.iter().enumerate() {
        if !plan.not_null {
            continue;
        }
        let Some(&r) = col_regs.get(idx) else {
            continue;
        };
        let violation = em.new_label();
        let ok = em.new_label();
        let addr = em.emit(Instruction::new(Opcode::IsNull, r, 0, 0));
        em.patch_p2(addr, violation);
        em.goto(ok);
        em.place(violation);
        emit_constraint_violation(
            em,
            action,
            SQLITE_CONSTRAINT_NOTNULL,
            format!(
                "NOT NULL constraint failed: {}.{}",
                schema.name,
                schema.columns.get(idx).map_or("?", String::as_str)
            ),
            row_skip,
        );
        em.place(ok);
    }

    // Re-validate CHECK against the new row, the same way `insert.rs`
    // does: build a plain (pre-affinity) record from `col_regs` and
    // evaluate each CHECK expression against a pseudo-cursor over it.
    // Built separately from `record_reg` below (which applies column
    // affinities) because affinity coercion can change what a CHECK
    // expression sees — e.g. `CHECK (col = 5)` against a TEXT '5'
    // reads differently before vs. after INTEGER-affinity coercion.
    let has_checks = !table_checks.is_empty() || plans.iter().any(|p| !p.checks.is_empty());
    if has_checks {
        let base_reg = first_reg(&col_regs)?;
        let count = i32::try_from(col_regs.len()).unwrap_or(0);
        let check_record_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::MakeRecord,
            base_reg,
            count,
            check_record_reg,
        ));
        em.emit(Instruction::new(
            Opcode::OpenPseudo,
            CHECK_CURSOR,
            check_record_reg,
            0,
        ));

        let mut check_exprs: Vec<&(Expr, String)> =
            plans.iter().flat_map(|p| p.checks.iter()).collect();
        check_exprs.extend(table_checks.iter());
        for (expr, label) in check_exprs {
            let violation = em.new_label();
            let ok = em.new_label();
            compile_cond(
                em,
                reg,
                &crate::codegen::row::Scope::single(check_schema, CHECK_CURSOR),
                expr,
                CondTargets {
                    on_true: Target::Fallthrough,
                    on_false: Target::Jump(violation),
                    on_null: NullTarget::True,
                },
            )?;
            em.goto(ok);
            em.place(violation);
            emit_constraint_violation(
                em,
                action,
                SQLITE_CONSTRAINT_CHECK,
                format!("CHECK constraint failed: {label}"),
                row_skip,
            );
            em.place(ok);
        }
    }

    let base_reg = first_reg(&col_regs)?;
    let count = i32::try_from(col_regs.len()).unwrap_or(0);
    let record_reg = reg.alloc();
    let affinities: Vec<u8> = schema
        .column_types
        .iter()
        .map(|t| affinity_of(t).to_p4_byte())
        .collect();
    em.emit(Instruction::with_p4(
        Opcode::MakeRecord,
        base_reg,
        count,
        record_reg,
        P4::Affinity(affinities),
    ));

    // Old index entries are read from the cursor's still-current
    // (pre-`Delete`) row — must happen before `Delete` clears it.
    emit_index_key_ops(
        em,
        reg,
        schema,
        TABLE_CURSOR,
        FIRST_INDEX_CURSOR,
        Opcode::IdxDelete,
        Some(index_touched),
    )?;
    if rowid_reassigned {
        // The row moves to a different rowid -- `Update` requires the
        // rowid to stay put (it rewrites the existing b-tree cell in
        // place), so this keeps the old two-op rebuild.
        em.emit(Instruction::new(Opcode::Delete, TABLE_CURSOR, 0, 0));
        em.emit(Instruction::new(
            Opcode::Insert,
            TABLE_CURSOR,
            rowid_reg,
            record_reg,
        ));
    } else {
        // #524: same rowid -- one combined leaf-page rewrite instead of
        // `Delete`'s and `Insert`'s independent root-to-leaf descents.
        em.emit(Instruction::new(
            Opcode::Update,
            TABLE_CURSOR,
            rowid_reg,
            record_reg,
        ));
    }

    if !schema.indexes.is_empty() {
        // The new row's values are already sitting in `col_regs`/
        // `rowid_reg` — build index keys from those directly instead of
        // seeking `TABLE_CURSOR` back onto the just-written row.
        emit_index_key_ops_from_regs(
            em,
            reg,
            schema,
            &col_regs,
            rowid_reg,
            FIRST_INDEX_CURSOR,
            Some(index_touched),
        )?;
    }

    Ok(())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#219/#235).

    use crate::codegen::row::{
        compile_update_with_catalog, IndexSchema, IndexedColumn, TableSchema,
    };
    use crate::parser::row::{parse_update, ParseOutcome};
    use crate::vm::row::{Opcode, Program};

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

    fn has(program: &Program, opcode: Opcode) -> bool {
        program.instructions.iter().any(|i| i.opcode == opcode)
    }

    // codegen_row_stmt_update_compile_update_with_catalog_8d819a73: `used_range_seek && range_seek_touches_scanned_index`.
    // Observable: the two-pass plan opens an ephemeral rowid table.
    fn update_program(sql: &str) -> Program {
        let update = match parse_update(sql) {
            ParseOutcome::Accepted(update) => *update,
            other => panic!("{sql:?} must parse, got {other:?}"),
        };
        let schema = with_index(table("t", 2, &["a", "b"]), "ia", 5, "a");
        compile_update_with_catalog(&update, &schema, std::slice::from_ref(&schema)).unwrap()
    }

    #[test]
    fn mcdc__codegen_row_stmt_update_compile_update_with_catalog_8d819a73__v1_range_seek_over_an_index_the_set_touches_uses_two_passes(
    ) {
        let p = update_program("UPDATE t SET a = 9 WHERE a BETWEEN 1 AND 5");
        assert!(has(&p, Opcode::OpenEphemeral), "{p:?}");
    }

    #[test]
    fn mcdc__codegen_row_stmt_update_compile_update_with_catalog_8d819a73__v2_range_seek_over_an_untouched_index_is_single_pass(
    ) {
        let p = update_program("UPDATE t SET b = 9 WHERE a BETWEEN 1 AND 5");
        assert!(
            has(&p, Opcode::IdxRowid) && !has(&p, Opcode::OpenEphemeral),
            "{p:?}"
        );
    }

    #[test]
    fn mcdc__codegen_row_stmt_update_compile_update_with_catalog_8d819a73__v3_no_range_seek_is_a_plain_scan(
    ) {
        let p = update_program("UPDATE t SET a = 9 WHERE b = 1");
        assert!(
            !has(&p, Opcode::IdxRowid) && !has(&p, Opcode::OpenEphemeral),
            "{p:?}"
        );
    }
}

#[cfg(test)]
mod index_skip_tests {
    //! #524: `UPDATE` shouldn't rebuild an index whose columns the `SET`
    //! clause never touches.

    use super::compile_update_with_catalog;
    use crate::codegen::row::{IndexSchema, IndexedColumn, TableSchema};
    use crate::parser::row::{parse_update, ParseOutcome};
    use crate::vm::row::{Opcode, Program};

    fn table_with_index(root_page: u32, columns: &[&str], index_column: &str) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            root_page,
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| "INTEGER".to_string()).collect(),
            sql: format!("CREATE TABLE t ({})", columns.join(", ")),
            indexes: vec![IndexSchema {
                name: "ix".to_string(),
                root_page: root_page + 1,
                unique: false,
                columns: vec![IndexedColumn {
                    name: index_column.to_string(),
                    desc: false,
                    collation: Default::default(),
                }],
            }],
            ..Default::default()
        }
    }

    fn compile(sql: &str, schema: &TableSchema) -> Program {
        let update = match parse_update(sql) {
            ParseOutcome::Accepted(update) => *update,
            other => panic!("{sql:?} must parse, got {other:?}"),
        };
        compile_update_with_catalog(&update, schema, std::slice::from_ref(schema)).unwrap()
    }

    fn count(program: &Program, opcode: Opcode) -> usize {
        program
            .instructions
            .iter()
            .filter(|i| i.opcode == opcode)
            .count()
    }

    #[test]
    fn update_skips_index_maintenance_when_set_does_not_touch_index_column() {
        // `x` (the index's column) is never assigned — no IdxDelete/IdxInsert.
        let schema = table_with_index(2, &["x", "n"], "x");
        let p = compile("UPDATE t SET n = n + 1 WHERE x > 5", &schema);
        assert_eq!(count(&p, Opcode::IdxDelete), 0, "{p:?}");
        assert_eq!(count(&p, Opcode::IdxInsert), 0, "{p:?}");
    }

    #[test]
    fn update_maintains_index_when_set_touches_index_column() {
        let schema = table_with_index(2, &["x", "n"], "x");
        let p = compile("UPDATE t SET x = x + 1 WHERE x > 5", &schema);
        assert_eq!(count(&p, Opcode::IdxDelete), 1, "{p:?}");
        assert_eq!(count(&p, Opcode::IdxInsert), 1, "{p:?}");
    }

    #[test]
    fn update_maintains_every_index_when_rowid_is_reassigned() {
        // Reassigning the rowid-alias column changes every index's key,
        // even one over an otherwise-untouched column.
        let mut schema = table_with_index(2, &["id", "n"], "n");
        schema.rowid_alias = Some(0);
        let p = compile("UPDATE t SET id = id + 1 WHERE id > 5", &schema);
        assert_eq!(count(&p, Opcode::IdxDelete), 1, "{p:?}");
        assert_eq!(count(&p, Opcode::IdxInsert), 1, "{p:?}");
    }

    #[test]
    fn update_emits_a_single_update_op_when_the_rowid_is_unchanged() {
        // db-core#524: the common case (SET doesn't touch the rowid
        // alias) rewrites the row with one `Update` op instead of a
        // `Delete`+`Insert` pair, avoiding two root-to-leaf descents.
        let schema = table_with_index(2, &["x", "n"], "x");
        let p = compile("UPDATE t SET n = n + 1 WHERE x > 5", &schema);
        assert_eq!(count(&p, Opcode::Update), 1, "{p:?}");
        assert_eq!(count(&p, Opcode::Delete), 0, "{p:?}");
        assert_eq!(count(&p, Opcode::Insert), 0, "{p:?}");
    }

    #[test]
    fn update_falls_back_to_delete_and_insert_when_the_rowid_is_reassigned() {
        // `Update` requires the rowid to stay put; a reassignment keeps
        // the two-op rebuild since it moves the row to a different cell.
        let mut schema = table_with_index(2, &["id", "n"], "n");
        schema.rowid_alias = Some(0);
        let p = compile("UPDATE t SET id = id + 1 WHERE id > 5", &schema);
        assert_eq!(count(&p, Opcode::Update), 0, "{p:?}");
        assert_eq!(count(&p, Opcode::Delete), 1, "{p:?}");
        assert_eq!(count(&p, Opcode::Insert), 1, "{p:?}");
    }
}
