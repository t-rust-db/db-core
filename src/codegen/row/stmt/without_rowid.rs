// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! What `INSERT`, `UPDATE` and `DELETE` share for a `WITHOUT ROWID` table
//! (#722). Such a table is an index b-tree keyed by its `PRIMARY KEY`:
//! there is no rowid to allocate, seek or compare, and a row is found by
//! its key columns instead. The table cursor the VM opens for it answers
//! `Column` in declared order and takes whole rows through `Insert`/
//! `Update`/`Delete` (`engine::row::adapter::WithoutRowidCursorAdapter`);
//! `NoConflict` on it probes the key.
//!
//! The index b-tree compares whole records byte for byte, ascending, so a
//! key with a `DESC` term or a non-`BINARY` collation would be stored out
//! of the order sqlite3 expects. Such a table is refused here, as an index
//! with those terms is in `index_maintenance`. A table with a secondary
//! index is refused too until its entries (which end in the key columns,
//! not a rowid) are maintained.

use crate::codegen::row::expr::{column_index, compile_cond, emit_column_read};
use crate::codegen::row::select::CodegenError;
use crate::codegen::row::stmt::insert::cached_create_table;
use crate::codegen::row::{CondTargets, Emitter, Label, RegAlloc, Scope, TableSchema, Target};
use crate::parser::ast::{ConflictAction, Expr};
use crate::value::Collation;
use crate::vm::row::{Instruction, Opcode, OPFLAG_NCHANGE, P4};

/// The declared index of each `PRIMARY KEY` column of `schema`, in key
/// order.
pub(crate) fn key_columns(schema: &TableSchema) -> Result<Vec<usize>, CodegenError> {
    let create = cached_create_table(schema)?;
    let key = crate::schema::autoindexes(&create)
        .into_iter()
        .find(|index| index.is_table)
        .ok_or_else(|| CodegenError::Internal {
            reason: format!("WITHOUT ROWID table {} has no primary key", schema.name),
        })?;
    let mut columns = Vec::with_capacity(key.columns.len());
    for term in &key.columns {
        if term.desc {
            return Err(CodegenError::Unsupported {
                reason: format!(
                    "WITHOUT ROWID table {}: a descending PRIMARY KEY column ({}) isn't \
                     supported yet",
                    schema.name, term.name
                ),
            });
        }
        if term.collation != Collation::Binary {
            return Err(CodegenError::Unsupported {
                reason: format!(
                    "WITHOUT ROWID table {}: PRIMARY KEY column {} compares by {:?}; \
                     non-BINARY keys aren't supported yet",
                    schema.name, term.name, term.collation
                ),
            });
        }
        columns.push(
            column_index(schema, &term.name).ok_or_else(|| CodegenError::Internal {
                reason: format!("primary key column {} is not in {}", term.name, schema.name),
            })?,
        );
    }
    Ok(columns)
}

/// [`key_columns`] for a statement that is about to write `schema`,
/// refusing a table with secondary indexes (`statement` names what was
/// asked for, in the error).
pub(crate) fn writable_key(
    schema: &TableSchema,
    statement: &str,
) -> Result<Vec<usize>, CodegenError> {
    if !schema.indexes.is_empty() {
        return Err(CodegenError::Unsupported {
            reason: format!(
                "{statement} on WITHOUT ROWID table {} with a secondary index isn't supported yet",
                schema.name
            ),
        });
    }
    key_columns(schema)
}

/// sqlite3's message for a row that repeats a primary key: every key
/// column, in key order, qualified by the table.
pub(crate) fn key_violation_message(schema: &TableSchema, key: &[usize]) -> String {
    let columns: Vec<String> = key
        .iter()
        .map(|&column| {
            format!(
                "{}.{}",
                schema.name,
                schema.columns.get(column).map_or("?", String::as_str)
            )
        })
        .collect();
    format!("UNIQUE constraint failed: {}", columns.join(", "))
}

/// The key columns of the row in `record_reg` -- a record as `MakeRecord`
/// built it, column affinities applied -- one register each, in key
/// order. Reading them back out of the record (rather than using the
/// values that went in) is what makes a probe see `5`, not `'5'`, for an
/// `INTEGER` key.
pub(crate) fn emit_key_from_record(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    record_reg: i32,
    key: &[usize],
) -> Result<Vec<i32>, CodegenError> {
    let pseudo = reg.alloc_cursor();
    em.emit(Instruction::new(Opcode::OpenPseudo, pseudo, record_reg, 0));
    let mut regs = Vec::with_capacity(key.len());
    for &column in key {
        let dest = reg.alloc();
        emit_column_read(em, schema, pseudo, column, dest)?;
        regs.push(dest);
    }
    Ok(regs)
}

/// Probes `cursor` for the row whose key values are in `key` (one register
/// each, key order): falls through, with the cursor on that row, when it
/// exists, and jumps to `not_found` otherwise (leaving the cursor on no
/// row). The key is copied into a run of its own first, because
/// `NoConflict` reads a contiguous run and writes a rowid to the register
/// after it when it hits -- a table without a rowid leaves that alone, but
/// the register is reserved all the same.
pub(crate) fn emit_seek_key(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    cursor: i32,
    key: &[i32],
    not_found: Label,
) {
    let mut first = None;
    for &source in key {
        let dest = reg.alloc();
        first.get_or_insert(dest);
        em.emit(Instruction::new(Opcode::Copy, source, dest, 0));
    }
    let _rowid_slot = reg.alloc();
    let addr = em.emit(Instruction::with_p4(
        Opcode::NoConflict,
        cursor,
        0,
        first.unwrap_or(0),
        P4::Int(i64::try_from(key.len()).unwrap_or(0)),
    ));
    em.patch_p2(addr, not_found);
}

/// The write of an `UPDATE` of one `WITHOUT ROWID` row, the cursor on
/// that row and `record_reg` holding the new row (#722). `old_key` is the
/// row's key as it was, read before the new values were worked out, or
/// empty when the `SET` leaves every key column alone.
///
/// A row whose key stays is rewritten in place. One whose key changes
/// moves in the b-tree: unless the new key is the old one spelled the same
/// way, it must not be another row's (`OR IGNORE` skips the row, `OR
/// REPLACE` removes the other row, anything else fails), and then the old
/// entry goes and the new one is added. The probe moves the cursor off the
/// row, so the old entry is found again by its key.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_row_rewrite(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    cursor: i32,
    key: &[usize],
    old_key: &[i32],
    record_reg: i32,
    dummy_rowid: i32,
    action: ConflictAction,
    primary_key_code: i32,
    row_skip: Label,
) -> Result<(), CodegenError> {
    let in_place = |em: &mut Emitter| {
        em.emit(
            Instruction::new(Opcode::Update, cursor, dummy_rowid, record_reg)
                .with_p5(OPFLAG_NCHANGE),
        );
    };
    if old_key.is_empty() {
        in_place(em);
        return Ok(());
    }
    let new_key = emit_key_from_record(em, reg, schema, record_reg, key)?;
    let done = em.new_label();
    let key_changes = em.new_label();
    // There is no `Ne`: a column that is equal jumps over the jump to
    // `key_changes`.
    for (new, old) in new_key.iter().zip(old_key) {
        let same = em.new_label();
        let equal = em.emit(Instruction::new(Opcode::Eq, *old, 0, *new));
        em.patch_p2(equal, same);
        em.goto(key_changes);
        em.place(same);
    }
    in_place(em);
    em.goto(done);

    em.place(key_changes);
    let key_is_free = em.new_label();
    emit_seek_key(em, reg, cursor, &new_key, key_is_free);
    match action {
        ConflictAction::Ignore => em.goto(row_skip),
        ConflictAction::Replace => {
            em.emit(Instruction::new(Opcode::Delete, cursor, 0, 0));
        }
        ConflictAction::Abort | ConflictAction::Fail | ConflictAction::Rollback => {
            em.emit(Instruction::with_p4(
                Opcode::Halt,
                primary_key_code,
                0,
                0,
                P4::Str(key_violation_message(schema, key)),
            ));
        }
    }
    em.place(key_is_free);
    emit_seek_key(em, reg, cursor, old_key, row_skip);
    em.emit(Instruction::new(Opcode::Delete, cursor, 0, 0));
    em.emit(
        Instruction::new(Opcode::Insert, cursor, dummy_rowid, record_reg).with_p5(OPFLAG_NCHANGE),
    );
    em.place(done);
    Ok(())
}

/// The cursor `DELETE` and `UPDATE` open a `WITHOUT ROWID` table on.
pub(crate) const TABLE_CURSOR: i32 = 0;

/// The scan every `DELETE` and `UPDATE` of a `WITHOUT ROWID` table makes
/// (#722), in two passes. The first walks the table and collects the key of
/// every row `where_clause` keeps into an ephemeral table; the second
/// seeks each key and runs `per_row` with the cursor on that row (`per_row`
/// gets the label to jump to to leave the row alone).
///
/// Two passes because the table's b-tree is an index b-tree, which
/// rebalances as rows are removed and moves a row whose key changes: a
/// cursor part-way through a scan has no safe place to resume after
/// either, and an updated row could be met a second time further on.
pub(crate) fn emit_two_pass<F>(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    scope: &Scope,
    where_clause: Option<&Expr>,
    key: &[usize],
    mut per_row: F,
) -> Result<(), CodegenError>
where
    F: FnMut(&mut Emitter, &mut RegAlloc, Label) -> Result<(), CodegenError>,
{
    let table_cursor = TABLE_CURSOR;
    let keys_cursor = reg.alloc_cursor();
    em.emit(Instruction {
        opcode: Opcode::OpenEphemeral,
        p1: keys_cursor,
        p2: 0,
        p3: 0,
        p4: P4::None,
        p5: 1,
        comment: None,
    });
    let done = em.new_label();

    // Pass 1: the keys of the rows to change.
    let second_pass = em.new_label();
    let rewind = em.emit(Instruction::new(Opcode::Rewind, table_cursor, 0, 0));
    em.patch_p2(rewind, second_pass);
    let scan = em.new_label();
    em.place(scan);
    let not_matched = em.new_label();
    if let Some(where_clause) = where_clause {
        compile_cond(
            em,
            reg,
            scope,
            where_clause,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(not_matched)),
        )?;
    }
    let mut first = None;
    for &column in key {
        let dest = reg.alloc();
        first.get_or_insert(dest);
        emit_column_read(em, schema, table_cursor, column, dest)?;
    }
    let first = first.ok_or_else(|| CodegenError::Internal {
        reason: format!(
            "WITHOUT ROWID table {} has an empty primary key",
            schema.name
        ),
    })?;
    let key_record = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        first,
        i32::try_from(key.len()).unwrap_or(0),
        key_record,
    ));
    let sequence = reg.alloc();
    em.emit(Instruction::new(Opcode::Sequence, keys_cursor, sequence, 0));
    em.emit(Instruction::new(
        Opcode::Insert,
        keys_cursor,
        sequence,
        key_record,
    ));
    em.place(not_matched);
    let next = em.emit(Instruction::new(Opcode::Next, table_cursor, 0, 0));
    em.patch_p2(next, scan);

    // Pass 2: each collected key, found again, then the caller's work.
    em.place(second_pass);
    let rewind_keys = em.emit(Instruction::new(Opcode::Rewind, keys_cursor, 0, 0));
    em.patch_p2(rewind_keys, done);
    let replay = em.new_label();
    em.place(replay);
    let mut collected = Vec::with_capacity(key.len());
    for position in 0..key.len() {
        let dest = reg.alloc();
        em.emit(Instruction::new(
            Opcode::Column,
            keys_cursor,
            i32::try_from(position).unwrap_or(0),
            dest,
        ));
        collected.push(dest);
    }
    let skip = em.new_label();
    emit_seek_key(em, reg, table_cursor, &collected, skip);
    per_row(em, reg, skip)?;
    em.place(skip);
    let next_key = em.emit(Instruction::new(Opcode::Next, keys_cursor, 0, 0));
    em.patch_p2(next_key, replay);
    em.place(done);
    Ok(())
}
