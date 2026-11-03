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
//! `DESC` key columns (#945) need no special key building: the key holds
//! the raw values, and the cursor (opened with a `P4::SortKey`) orders its
//! b-tree with those columns reversed. An *expression* key column is
//! evaluated per row ([`emit_index_key_columns`]) from the table cursor or
//! from the staged row's registers.

use crate::codegen::row::expr::{column_index, compile_value, emit_column_read};
use crate::codegen::row::select::CodegenError;
use crate::codegen::row::Scope;
use crate::codegen::row::{Emitter, RegAlloc};
use crate::codegen::row::{IndexSchema, TableSchema};
use crate::vm::row::{Instruction, Opcode, SortKeyColumn, P4};

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

/// Refuses to maintain or probe an index key column that is not
/// `BINARY`-collated. The index b-tree orders and searches its keys
/// byte for byte (#298), so a `NOCASE`/`RTRIM` key would be written out
/// of order and a uniqueness probe would miss case variants -- a file
/// sqlite3 reports as corrupt or a duplicate let through. Such an index
/// is read through (the planner never seeks it), but not written (#694).
pub(crate) fn reject_non_binary_key(
    index: &IndexSchema,
    col: &crate::codegen::row::IndexedColumn,
) -> Result<(), CodegenError> {
    if col.collation == crate::value::Collation::Binary {
        return Ok(());
    }
    Err(CodegenError::Unsupported {
        reason: format!(
            "index {} keys {} by {:?}; non-BINARY index keys aren't supported yet",
            index.name, col.name, col.collation
        ),
    })
}

/// sqlite3's message for a row that duplicates a key of the UNIQUE
/// `index`: the indexed columns, each qualified by the table --
/// `UNIQUE constraint failed: t.a, t.b`.
pub(crate) fn unique_violation_message(schema: &TableSchema, index: &IndexSchema) -> String {
    let columns: Vec<String> = index
        .columns
        .iter()
        .map(|col| format!("{}.{}", schema.name, col.name))
        .collect();
    format!("UNIQUE constraint failed: {}", columns.join(", "))
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

/// The key descriptor an index cursor needs when any key column is `DESC`
/// (#945), `None` for the all-ascending common case (whose opcodes then
/// stay exactly as before). Only the `descending` flags are read by the
/// cursor factory.
pub(crate) fn index_sort_key(index: &IndexSchema) -> Option<Vec<SortKeyColumn>> {
    index.columns.iter().any(|c| c.desc).then(|| {
        index
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| SortKeyColumn {
                index: i,
                descending: c.desc,
                collation: c.collation,
                nulls_first: false,
            })
            .collect()
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
        let mut instr =
            Instruction::new(Opcode::OpenWrite, cursor, root_page, schema.database_p3());
        instr.p5 = 1;
        // A `DESC` key column reorders the b-tree: tell the cursor (#945).
        if let Some(key) = index_sort_key(index) {
            instr.p4 = P4::SortKey(key);
        }
        em.emit(instr);
    }
    Ok(())
}

/// Where an index's key columns are read from.
pub(crate) enum KeySource {
    /// A cursor's current row: the table b-tree, or (`record`) a pseudo
    /// cursor re-reading a staged row's record. `schema` is that row's.
    Cursor {
        schema: TableSchema,
        cursor: i32,
        record: bool,
    },
    /// A staged row: one register per table column in `col_regs` (the
    /// rowid-alias column's own register holds NULL, so its live value is
    /// `rowid_reg`), the rowid in `rowid_reg`.
    Regs { col_regs: Vec<i32>, rowid_reg: i32 },
}

/// `index`'s key column `col` as an expression: `None` for a plain column
/// of `schema`; otherwise `col.name` is the expression's raw text
/// (`sqlite_master`'s `CREATE INDEX`, #945), parsed here.
pub(crate) fn key_column_expr(
    schema: &TableSchema,
    index: &IndexSchema,
    col: &crate::codegen::row::IndexedColumn,
) -> Result<Option<crate::parser::ast::Expr>, CodegenError> {
    use crate::parser::row::error::ParseOutcome;
    if column_index(schema, &col.name).is_some() {
        return Ok(None);
    }
    let unresolved = || CodegenError::Unsupported {
        reason: format!(
            "index {} references a column or expression this codegen can't resolve: {}",
            index.name, col.name
        ),
    };
    match crate::parser::row::parse_select(&format!("SELECT {}", col.name)) {
        ParseOutcome::Accepted(select) if select.columns.len() == 1 => {
            match select.columns.into_iter().next() {
                Some(crate::parser::ast::ResultColumn::Expr { expr, .. }) => Ok(Some(expr)),
                _ => Err(unresolved()),
            }
        }
        _ => Err(unresolved()),
    }
}

/// Emits `index`'s key columns from `source` into `index.columns.len() +
/// extra` fresh *contiguous* registers (the `extra` trailing ones are for
/// the caller: the rowid, a probe's conflict register) and returns the
/// first. A plain column is read (`Column`) or copied; an expression
/// column is compiled against the row and copied into its slot.
pub(crate) fn emit_index_key_columns(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    schema: &TableSchema,
    index: &IndexSchema,
    extra: usize,
    source: &KeySource,
) -> Result<i32, CodegenError> {
    let mut exprs = Vec::with_capacity(index.columns.len());
    for col in &index.columns {
        reject_non_binary_key(index, col)?;
        exprs.push(key_column_expr(schema, index, col)?);
    }
    // Every slot first, so the key is one contiguous run whatever the
    // expressions allocate while compiling.
    let slots: Vec<i32> = (0..index.columns.len().saturating_add(extra))
        .map(|_| reg.alloc())
        .collect();
    let start = slots
        .first()
        .copied()
        .ok_or_else(|| CodegenError::Internal {
            reason: format!("index {} has no key columns", index.name),
        })?;

    let expr_scope = if exprs.iter().any(Option::is_some) {
        Some(match source {
            KeySource::Cursor {
                schema,
                cursor,
                record,
            } => {
                let scope = Scope::single(schema, *cursor);
                if *record {
                    scope.with_record_cursors()
                } else {
                    scope
                }
            }
            KeySource::Regs {
                col_regs,
                rowid_reg,
            } => {
                // The staged row as a record a pseudo cursor re-reads; the
                // rowid-alias column carries the rowid there.
                let block: Vec<i32> = (0..col_regs.len()).map(|_| reg.alloc()).collect();
                for (i, (&src, &dest)) in col_regs.iter().zip(&block).enumerate() {
                    let src = if Some(i) == schema.rowid_alias {
                        *rowid_reg
                    } else {
                        src
                    };
                    em.emit(Instruction::new(Opcode::Copy, src, dest, 0));
                }
                let record_reg = reg.alloc();
                // A cursor number no other cursor of the statement uses
                // (#945): the allocator's own, above every fixed constant.
                let pseudo_cursor = reg.alloc_cursor();
                em.emit(Instruction::new(
                    Opcode::MakeRecord,
                    block.first().copied().unwrap_or(0),
                    i32::try_from(block.len()).unwrap_or(0),
                    record_reg,
                ));
                em.emit(Instruction::new(
                    Opcode::OpenPseudo,
                    pseudo_cursor,
                    record_reg,
                    0,
                ));
                Scope::single(schema, pseudo_cursor).with_record_cursors()
            }
        })
    } else {
        None
    };

    for (i, (col, expr)) in index.columns.iter().zip(&exprs).enumerate() {
        let dest = slots.get(i).copied().unwrap_or(start);
        if let (Some(expr), Some(scope)) = (expr, &expr_scope) {
            let value = compile_value(em, reg, scope, expr)?;
            em.emit(Instruction::new(Opcode::Copy, value, dest, 0));
            continue;
        }
        let col_idx = column_index(schema, &col.name).ok_or_else(|| CodegenError::Internal {
            reason: format!("index {} column {} lost its position", index.name, col.name),
        })?;
        match source {
            KeySource::Cursor { schema, cursor, .. } => {
                emit_column_read(em, schema, *cursor, col_idx, dest)?
            }
            KeySource::Regs {
                col_regs,
                rowid_reg,
            } => {
                let src = if Some(col_idx) == schema.rowid_alias {
                    *rowid_reg
                } else {
                    col_regs
                        .get(col_idx)
                        .copied()
                        .ok_or_else(|| CodegenError::Unsupported {
                            reason: format!(
                                "index {} references column {} outside the row's register run",
                                index.name, col.name
                            ),
                        })?
                };
                em.emit(Instruction::new(Opcode::Copy, src, dest, 0));
            }
        }
    }
    Ok(start)
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
        let start = emit_index_key_columns(
            em,
            reg,
            schema,
            index,
            1,
            &KeySource::Cursor {
                schema: schema.clone(),
                cursor: table_cursor,
                record: false,
            },
        )?;
        let rowid_reg = start.saturating_add(i32::try_from(index.columns.len()).unwrap_or(0));
        em.emit(Instruction::new(Opcode::Rowid, table_cursor, rowid_reg, 0));

        let count = i32::try_from(index.columns.len().saturating_add(1)).unwrap_or(0);
        em.emit(Instruction::with_p4(
            opcode,
            index_cursor,
            start,
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
        let start = emit_index_key_columns(
            em,
            reg,
            schema,
            index,
            1,
            &KeySource::Regs {
                col_regs: col_regs.to_vec(),
                rowid_reg,
            },
        )?;
        let key_rowid_reg = start.saturating_add(i32::try_from(index.columns.len()).unwrap_or(0));
        em.emit(Instruction::new(Opcode::Copy, rowid_reg, key_rowid_reg, 0));

        let count = i32::try_from(index.columns.len().saturating_add(1)).unwrap_or(0);
        em.emit(Instruction::with_p4(
            Opcode::IdxInsert,
            index_cursor,
            start,
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
    fn a_desc_index_column_is_maintained_and_its_cursor_carries_the_direction() {
        // #945: the key is built as for an ascending column; the write
        // cursor's `OpenWrite` carries the per-column direction so the
        // b-tree is searched in the order it is stored in.
        let schema = table("t", 2, &["a", "b"]);
        let index = IndexSchema {
            name: "idx_desc".to_string(),
            root_page: 3,
            columns: vec![
                IndexedColumn {
                    name: "a".to_string(),
                    desc: true,
                    collation: Collation::Binary,
                },
                IndexedColumn {
                    name: "b".to_string(),
                    desc: false,
                    collation: Collation::Binary,
                },
            ],
            ..Default::default()
        };
        let schema_with_index = TableSchema {
            indexes: vec![index],
            ..schema
        };
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        open_index_cursors(&mut em, &schema_with_index, 1).unwrap();
        emit_index_key_ops(
            &mut em,
            &mut reg,
            &schema_with_index,
            0,
            1,
            Opcode::IdxInsert,
            None,
        )
        .unwrap();
        let program = em.finish();
        let open = &program.instructions[0];
        assert_eq!(open.opcode, Opcode::OpenWrite);
        match &open.p4 {
            P4::SortKey(key) => {
                assert_eq!(
                    key.iter().map(|k| k.descending).collect::<Vec<_>>(),
                    [true, false]
                );
            }
            other => panic!("expected a SortKey, got {other:?}"),
        }
        assert!(program
            .instructions
            .iter()
            .any(|i| i.opcode == Opcode::IdxInsert));
    }
}
