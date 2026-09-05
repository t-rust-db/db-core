//! Secondary-index maintenance shared by `INSERT`/`UPDATE`/`DELETE`
//! codegen (db-core#96, mirroring sqlite-rs's `codegen/
//! index_maintenance.rs`): open a write cursor per index alongside the
//! table cursor, and emit the `IdxInsert`/`IdxDelete` pair for a row's
//! index entries.
//!
//! **Scoped down** (see [`super`]'s module doc for the general
//! rationale): no real root pages -- index cursors, like the table
//! cursor, are pre-wired storage-agnostic [`crate::vm::row::Cursor`]
//! slots (see `super::select`'s same convention), so `open_index_cursors`
//! just emits `OpenWrite` against the caller-assigned slot. No `DESC`
//! index columns (rejected loudly, same reasoning sqlite-rs's own
//! version gives: no index comparator here is aware of sort direction).
//!
//! For a row whose values are only available from disk (the row a
//! `DELETE`/`UPDATE` is currently positioned on), index keys are read
//! back from the table cursor's *current* row via ordinary
//! `Opcode::Column`/`Opcode::Rowid` -- see [`emit_index_key_ops`]. For a
//! row whose values are already sitting in registers (a
//! freshly-inserted/updated row, before or instead of being re-read),
//! [`emit_index_key_ops_from_regs`] builds the same key layout via
//! `Opcode::Copy` from those registers instead.

use super::value::emit_column_read;
use super::{CodegenError, Emitter, IndexSchema, RegAlloc, Result, TableSchema};
use crate::vm::row::{Instruction, Opcode, P4};

/// `OpenWrite`s one write cursor per index on `schema`, starting at
/// `first_cursor` -- the caller must pre-wire (`Vm::open_cursor`) the
/// same slots before running the resulting `Program`.
pub(crate) fn open_index_cursors(em: &mut Emitter, schema: &TableSchema, first_cursor: i32) {
    for (i, _index) in schema.indexes.iter().enumerate() {
        let cursor = first_cursor.saturating_add(i32::try_from(i).unwrap_or(i32::MAX));
        em.emit(Instruction::new(Opcode::OpenWrite, cursor, 0, 0));
    }
}

fn resolve_index_columns(schema: &TableSchema, index: &IndexSchema) -> Result<Vec<usize>> {
    index
        .columns
        .iter()
        .map(|name| {
            schema
                .column_index(name)
                .ok_or_else(|| CodegenError::Unsupported {
                    reason: format!("index {} references unknown column {name}", index.name),
                })
        })
        .collect()
}

/// For every index on `schema`, reads the current row at `table_cursor`
/// into a fresh contiguous register block (index columns in declared
/// order, then rowid) and emits `opcode` (`IdxInsert` or `IdxDelete`)
/// against the matching cursor in `[first_index_cursor, ...)`.
///
/// The table cursor must already be positioned on the row whose index
/// entries are being built.
pub(crate) fn emit_index_key_ops(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    table_cursor: i32,
    first_index_cursor: i32,
    opcode: Opcode,
) -> Result<()> {
    for (i, index) in schema.indexes.iter().enumerate() {
        let index_cursor = first_index_cursor.saturating_add(i32::try_from(i).unwrap_or(i32::MAX));
        let col_indices = resolve_index_columns(schema, index)?;
        let mut start = None;
        for &col_idx in &col_indices {
            let r = reg.alloc();
            if start.is_none() {
                start = Some(r);
            }
            emit_column_read(em, schema, table_cursor, col_idx, r)?;
        }
        let rowid_reg = reg.alloc();
        if start.is_none() {
            start = Some(rowid_reg);
        }
        em.emit(Instruction::new(Opcode::Rowid, table_cursor, rowid_reg, 0));

        let count = i32::try_from(col_indices.len().saturating_add(1)).unwrap_or(0);
        em.emit(Instruction::with_p4(
            opcode,
            index_cursor,
            start.unwrap_or(rowid_reg),
            0,
            P4::Int(i64::from(count)),
        ));
    }
    Ok(())
}

/// Like [`emit_index_key_ops`], but for a row whose column values are
/// already sitting in `col_regs` (one register per `schema.columns`
/// entry, in order) and whose rowid is already in `rowid_reg`. Builds
/// each index's key via `Opcode::Copy` from those registers instead of
/// `Opcode::Column`/`Opcode::Rowid` against a cursor. Always emits
/// `IdxInsert`.
pub(crate) fn emit_index_key_ops_from_regs(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    col_regs: &[i32],
    rowid_reg: i32,
    first_index_cursor: i32,
) -> Result<()> {
    for (i, index) in schema.indexes.iter().enumerate() {
        let index_cursor = first_index_cursor.saturating_add(i32::try_from(i).unwrap_or(i32::MAX));
        let col_indices = resolve_index_columns(schema, index)?;
        let mut start = None;
        for col_idx in col_indices {
            let src = if Some(col_idx) == schema.rowid_alias {
                rowid_reg
            } else {
                *col_regs
                    .get(col_idx)
                    .ok_or_else(|| CodegenError::Unsupported {
                        reason: format!(
                            "index {} references column outside the row's register run",
                            index.name
                        ),
                    })?
            };
            let r = reg.alloc();
            if start.is_none() {
                start = Some(r);
            }
            em.emit(Instruction::new(Opcode::Copy, src, r, 0));
        }
        let key_rowid_reg = reg.alloc();
        if start.is_none() {
            start = Some(key_rowid_reg);
        }
        em.emit(Instruction::new(Opcode::Copy, rowid_reg, key_rowid_reg, 0));

        let count = i32::try_from(index.columns.len().saturating_add(1)).unwrap_or(0);
        em.emit(Instruction::with_p4(
            Opcode::IdxInsert,
            index_cursor,
            start.unwrap_or(key_rowid_reg),
            0,
            P4::Int(i64::from(count)),
        ));
    }
    Ok(())
}
