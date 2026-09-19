// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Secondary-index maintenance shared by `INSERT`/`DELETE`/`UPDATE`
//! codegen (#196): open a write cursor per index alongside the table
//! cursor, and emit the `IdxInsert`/`IdxDelete` pair for a row's index
//! entries.
//!
//! For a row whose values are only available from disk (the *old* row
//! of an `UPDATE`'s rebuild, or a row displaced by an `INSERT OR
//! REPLACE` conflict), index keys are read back from the table cursor's
//! *current* row via ordinary `Opcode::Column`/`Opcode::Rowid` (rowid
//! last, matching the on-disk index key convention
//! `btree::index::insert`/`index::delete` use) — see
//! [`emit_index_key_ops`]. For a row whose values are already sitting in
//! registers (a freshly-inserted/updated row, before it's written),
//! [`emit_index_key_ops_from_regs`] builds the same key layout via
//! `Opcode::Copy` from those registers instead, with no cursor re-seek
//! or re-read.
//!
//! `DESC` index columns are rejected (`CodegenError::Unsupported`)
//! rather than silently mis-keyed: no index b-tree comparator in this
//! codebase (#171) is aware of per-column sort direction, so a `DESC`
//! column would otherwise get built into the key as if it were
//! ascending — a plausible-looking but semantically backwards key.

use crate::codegen::row::expr::{column_index, emit_column_read};
use crate::codegen::row::select::CodegenError;
use crate::codegen::row::{Emitter, RegAlloc};
use crate::codegen::row::{IndexSchema, TableSchema};
use crate::vm::row::{Instruction, Opcode, P4};

/// Validates a table's `sqlite_master.rootpage` before it's used as an
/// `OpenRead`/`OpenWrite` operand.
///
/// `rootpage` is untrusted on-disk data — a corrupt or adversarial file
/// could carry a zero, negative, or out-of-`i32`-range value for it.
/// Rather than defaulting a bad value to page 0 (the reserved header
/// page) and silently pointing the cursor at it, this rejects the
/// schema outright.
pub(crate) fn valid_table_root_page(schema: &TableSchema) -> Result<i32, CodegenError> {
    i32::try_from(schema.root_page)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| CodegenError::Unsupported {
            reason: format!(
                "table {} has an invalid root page ({})",
                schema.name, schema.root_page
            ),
        })
}

/// Same validation as [`valid_table_root_page`], for an index's root page.
pub(crate) fn valid_index_root_page(index: &IndexSchema) -> Result<i32, CodegenError> {
    i32::try_from(index.root_page)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| CodegenError::Unsupported {
            reason: format!(
                "index {} has an invalid root page ({})",
                index.name, index.root_page
            ),
        })
}

/// `OpenWrite`s one write cursor per index on `schema`, starting at
/// `first_cursor`, with `P5 = 1` selecting `CursorSlot::IndexWrite`
/// (#194's `OpenWrite` doc).
pub(crate) fn open_index_cursors(
    em: &mut Emitter,
    schema: &TableSchema,
    first_cursor: i32,
) -> Result<(), CodegenError> {
    for (i, index) in schema.indexes.iter().enumerate() {
        let cursor = first_cursor.saturating_add(i32::try_from(i).unwrap_or(0));
        let root_page = valid_index_root_page(index)?;
        let mut instr = Instruction::new(Opcode::OpenWrite, cursor, root_page, 0);
        instr.p5 = 1;
        em.emit(instr);
    }
    Ok(())
}

/// For every index on `schema`, reads the current row at `table_cursor`
/// into a fresh contiguous register block (index columns in declared
/// order, then rowid) and emits `opcode` (`IdxInsert` or `IdxDelete`)
/// against the matching cursor in `[first_index_cursor, ...)`.
///
/// The table cursor must already be positioned on the row whose index
/// entries are being built — callers use this both pre-`Delete` (cursor
/// already there) and post-`Insert` (after a `SeekRowid` back onto the
/// just-written row).
///
/// `touched` gates which indexes get maintenance emitted at all:
/// `None` (the `INSERT`/`DELETE` callers) always maintains every index —
/// a fresh row or a fully-removed row touches every index by definition.
/// `Some(flags)` (`UPDATE`, #524) skips index `i` entirely when
/// `flags[i]` is false, i.e. the `SET` clause doesn't touch that index's
/// key — no reason to pay a b-tree delete+insert on an index whose
/// on-disk key is provably unchanged.
pub(crate) fn emit_index_key_ops(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    table_cursor: i32,
    first_index_cursor: i32,
    opcode: Opcode,
    touched: Option<&[bool]>,
) -> Result<(), CodegenError> {
    for (i, index) in schema.indexes.iter().enumerate() {
        if touched.is_some_and(|flags| flags.get(i) == Some(&false)) {
            continue;
        }
        let index_cursor = first_index_cursor.saturating_add(i32::try_from(i).unwrap_or(0));
        let mut start = None;
        for col in &index.columns {
            if col.desc {
                // No index b-tree comparator anywhere in this codebase
                // (#171) is aware of per-column sort direction — it
                // always compares the encoded key ascending. Silently
                // building the key as if `col` were ascending would
                // write an index stock `sqlite3` (which does honor
                // `DESC`) reads back in the wrong order. Reject loudly
                // instead of writing a key that's byte-for-byte
                // plausible but semantically backwards.
                return Err(CodegenError::Unsupported {
                    reason: format!(
                        "index {} has a DESC column ({}); descending index keys aren't supported yet",
                        index.name, col.name
                    ),
                });
            }
            let col_idx =
                column_index(schema, &col.name).ok_or_else(|| CodegenError::Unsupported {
                    reason: format!(
                        "index {} references a column or expression this codegen can't resolve: {}",
                        index.name, col.name
                    ),
                })?;
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

        let count = i32::try_from(index.columns.len().saturating_add(1)).unwrap_or(0);
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
/// entry, in order — the same layout `INSERT`/`UPDATE` codegen builds
/// for `MakeRecord`) and whose rowid is already in `rowid_reg`. Builds
/// each index's key via `Opcode::Copy` from those registers into a
/// fresh contiguous run instead of `Opcode::Column`/`Opcode::Rowid`
/// against a cursor — so callers don't need to `SeekRowid` back onto
/// the row first. Always emits `IdxInsert`: the only caller that needs
/// `IdxDelete` (removing a *different*, already-on-disk row's stale
/// entries) has no such register run to reuse and stays on
/// [`emit_index_key_ops`].
/// `touched` has the same meaning as [`emit_index_key_ops`]'s parameter.
pub(crate) fn emit_index_key_ops_from_regs(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    col_regs: &[i32],
    rowid_reg: i32,
    first_index_cursor: i32,
    touched: Option<&[bool]>,
) -> Result<(), CodegenError> {
    for (i, index) in schema.indexes.iter().enumerate() {
        if touched.is_some_and(|flags| flags.get(i) == Some(&false)) {
            continue;
        }
        let index_cursor = first_index_cursor.saturating_add(i32::try_from(i).unwrap_or(0));
        let mut start = None;
        for col in &index.columns {
            if col.desc {
                return Err(CodegenError::Unsupported {
                    reason: format!(
                        "index {} has a DESC column ({}); descending index keys aren't supported yet",
                        index.name, col.name
                    ),
                });
            }
            let col_idx =
                column_index(schema, &col.name).ok_or_else(|| CodegenError::Unsupported {
                    reason: format!(
                        "index {} references a column or expression this codegen can't resolve: {}",
                        index.name, col.name
                    ),
                })?;
            // The rowid-alias column's own register holds NULL (readers
            // substitute the cursor's actual rowid instead — see
            // `emit_column_read`), so its live value is `rowid_reg`, not
            // `col_regs[col_idx]`.
            let src = if Some(col_idx) == schema.rowid_alias {
                rowid_reg
            } else {
                *col_regs
                    .get(col_idx)
                    .ok_or_else(|| CodegenError::Unsupported {
                        reason: format!(
                            "index {} references column {} outside the row's register run",
                            index.name, col.name
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::row::IndexedColumn;
    use crate::value::Collation;

    fn table(name: &str, root_page: u32, cols: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            root_page,
            columns: cols.iter().map(|c| (*c).to_string()).collect(),
            column_types: cols.iter().map(|_| "INTEGER".to_string()).collect(),
            column_collations: cols.iter().map(|_| Collation::Binary).collect(),
            ..Default::default()
        }
    }

    // Both root-page validators reject a `0` root page (the DDL-layer
    // sentinel for "not yet allocated") the same way -- untested until
    // now, since every SQL-level test naturally has a real root page by
    // the time codegen runs.

    #[test]
    fn valid_table_root_page_rejects_page_zero() {
        let schema = table("t", 0, &["a"]);
        let e = valid_table_root_page(&schema).unwrap_err();
        assert!(matches!(e, CodegenError::Unsupported { .. }), "{e:?}");
    }

    #[test]
    fn valid_index_root_page_rejects_page_zero() {
        let index = IndexSchema {
            name: "idx".to_string(),
            root_page: 0,
            ..Default::default()
        };
        let e = valid_index_root_page(&index).unwrap_err();
        assert!(matches!(e, CodegenError::Unsupported { .. }), "{e:?}");
    }

    #[test]
    fn emit_index_key_ops_rejects_a_desc_index_column() {
        // #171: no index b-tree comparator understands per-column sort
        // direction, so a `DESC` index column is rejected outright
        // rather than silently built as if it were ascending. Not
        // reachable through SQL today (the DDL parser doesn't accept a
        // descending index key yet), so this builds the schema by hand.
        let schema = table("t", 2, &["a", "b"]);
        let index = IndexSchema {
            name: "idx_desc".to_string(),
            root_page: 3,
            columns: vec![IndexedColumn {
                name: "a".to_string(),
                desc: true,
                collation: Collation::Binary,
            }],
            ..Default::default()
        };
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let schema_with_index = TableSchema {
            indexes: vec![index],
            ..schema
        };
        let e = emit_index_key_ops(
            &mut em,
            &mut reg,
            &schema_with_index,
            0,
            1,
            Opcode::IdxInsert,
            None,
        )
        .unwrap_err();
        assert!(matches!(e, CodegenError::Unsupported { .. }), "{e:?}");
    }
}
