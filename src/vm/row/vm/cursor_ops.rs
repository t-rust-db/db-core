// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! [`super::step`]'s cursor ops opcode family (#661, #670).

use super::*;

/// [`step`] for one family: opening, moving and reading table and index cursors. `step` sends
/// only these opcodes here.
#[inline(always)]
pub(super) fn step_cursor_ops(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    match instr.opcode {
        Opcode::Rewind => {
            vm.clear_null_row(instr.p1)?;
            let has_row = vm.cursor_mut(instr.p1)?.rewind();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::Last => {
            vm.clear_null_row(instr.p1)?;
            let has_row = vm.cursor_mut(instr.p1)?.last();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::Rowid => {
            // sqlite-rs: a NullRow'd cursor reads NULL for its rowid too.
            let value = if vm.is_null_row(instr.p1)? {
                Value::Null
            } else {
                Value::Integer(
                    vm.cursor(instr.p1)?
                        .rowid()
                        .ok_or(ExecError::NoCurrentRow {
                            opcode: "Rowid",
                            slot: instr.p1,
                        })?,
                )
            };
            vm.set_register(instr.p2, value)?;
            Ok(Step::Next)
        }
        Opcode::SeekRowid => {
            vm.clear_null_row(instr.p1)?;
            let rowid = register_as_i64(vm, instr.p3)?;
            let found = vm.cursor_mut(instr.p1)?.seek(rowid);
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::NullRow => {
            vm.set_null_row(instr.p1)?;
            Ok(Step::Next)
        }
        Opcode::Sequence => {
            let value = vm.next_sequence(instr.p1)?;
            vm.set_register(instr.p2, Value::Integer(value))?;
            Ok(Step::Next)
        }
        Opcode::Count => {
            // sqlite-rs: p1 is the *root page* of the table to count (the
            // `SELECT count(*)` fast path opens its own cursor); with no
            // cursor factory (in-memory programs) p1 names an open slot.
            let count = if let Some(factory) = vm.cursor_factory.as_deref_mut() {
                #[allow(clippy::cast_sign_loss)]
                let root = instr.p1 as u32;
                let mut cursor = factory
                    .open_read(root)
                    .map_err(ExecError::CursorFactoryFailed)?;
                count_rows(cursor.as_mut())
            } else {
                count_rows(vm.cursor_mut(instr.p1)?.as_mut())
            };
            vm.set_register(instr.p2, Value::Integer(count))?;
            Ok(Step::Next)
        }
        Opcode::AutoIndexInsert => {
            // sqlite-rs: p2 = first key register (count/collations from P4),
            // p3 = rowid register.
            let (key, collations) = key_from_registers(vm, "AutoIndexInsert", instr.p2, &instr.p4)?;
            let rowid = register_as_i64(vm, instr.p3)?;
            if vm.cursor(instr.p1).is_err() {
                vm.open_cursor(
                    instr.p1,
                    Box::new(super::super::cursor::AutoIndexCursor::new()),
                )?;
            }
            let ok = vm
                .cursor_mut(instr.p1)?
                .auto_index_insert(key, &collations, rowid);
            if !ok {
                return Err(ExecError::MalformedInstruction {
                    opcode: "AutoIndexInsert",
                    reason: "cursor slot is not an automatic-index cursor".to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::AutoIndexSeek => {
            let (key, collations) = key_from_registers(vm, "AutoIndexSeek", instr.p3, &instr.p4)?;
            let found = vm.cursor_mut(instr.p1)?.auto_index_seek(&key, &collations);
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::AutoIndexRowid => {
            let rowid = vm
                .cursor(instr.p1)?
                .rowid()
                .ok_or(ExecError::NoCurrentRow {
                    opcode: "AutoIndexRowid",
                    slot: instr.p1,
                })?;
            vm.set_register(instr.p2, Value::Integer(rowid))?;
            Ok(Step::Next)
        }
        Opcode::AutoIndexNext => {
            let has_next = vm.cursor_mut(instr.p1)?.auto_index_next();
            Ok(if has_next {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::OpenRead | Opcode::OpenWrite => {
            if instr.p3 != 0 {
                return Err(ExecError::AttachedDatabasesUnsupported);
            }
            let Some(factory) = vm.cursor_factory.as_deref_mut() else {
                // No factory installed -- fall back to the pre-wired
                // path: assert the caller already wired this slot via
                // `open_cursor` before running the program.
                vm.cursor(instr.p1)?;
                return Ok(Step::Next);
            };
            #[allow(clippy::cast_sign_loss)]
            let root = instr.p2 as u32;
            // sqlite-rs flags an index cursor with `p5 != 0` (no key
            // descriptor); a `P4::SortKey` carries one when the planner has it.
            let cursor = match (&instr.p4, instr.p5, instr.opcode) {
                (P4::SortKey(key), _, _) => factory.open_index(root, key),
                (_, p5, _) if p5 != 0 => factory.open_index(root, &[]),
                (_, _, Opcode::OpenWrite) => factory.open_write(root),
                _ => factory.open_read(root),
            }
            .map_err(ExecError::CursorFactoryFailed)?;
            vm.open_cursor(instr.p1, cursor)?;
            vm.set_cursor_root(instr.p1, root)?;
            Ok(Step::Next)
        }
        Opcode::OpenDup => {
            // sqlite-rs: a second cursor sharing an ephemeral table's rows.
            if let Some(dup) = vm.cursor(instr.p2)?.dup() {
                vm.open_cursor(instr.p1, dup)?;
                return Ok(Step::Next);
            }
            let root = vm.cursor_root(instr.p2)?;
            let factory =
                vm.cursor_factory
                    .as_deref_mut()
                    .ok_or(ExecError::CursorFactoryFailed(CursorFactoryError(
                        "OpenDup requires a cursor factory".to_string(),
                    )))?;
            let cursor = factory
                .open_read(root)
                .map_err(ExecError::CursorFactoryFailed)?;
            vm.open_cursor(instr.p1, cursor)?;
            vm.set_cursor_root(instr.p1, root)?;
            Ok(Step::Next)
        }
        Opcode::OpenPseudo => {
            // sqlite-rs: the cursor reads register p2 *lazily* — codegen
            // opens it before the register holds a row (`SorterData` /
            // `MakeRecord` fill it later, and rewrite it between rows).
            // Placeholder only: the row is read live from register `p2`
            // (`pseudo_column`), never from this cursor's own values.
            vm.open_cursor(instr.p1, Box::new(PseudoCursor::default()))?;
            vm.set_pseudo_reg(instr.p1, Some(instr.p2))?;
            Ok(Step::Next)
        }
        Opcode::OpenEphemeral => {
            // sqlite-rs `cursor::open_ephemeral`: p5 = 0 -> ephemeral index
            // (DISTINCT / IN-subquery), 1 -> ephemeral table (materialized
            // rows by rowid), 2 -> automatic join index.
            let cursor: Box<dyn Cursor> = match instr.p5 {
                0 => Box::new(super::super::cursor::EphemeralIndexCursor::new()),
                2 => Box::new(super::super::cursor::AutoIndexCursor::new()),
                _ => Box::new(EphemeralTableCursor::new()),
            };
            vm.open_cursor(instr.p1, cursor)?;
            Ok(Step::Next)
        }
        Opcode::IdxRewind => {
            let has_row = vm.cursor_mut(instr.p1)?.rewind();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::IdxLast => {
            let has_row = vm.cursor_mut(instr.p1)?.last();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::IdxNext => {
            let has_row = vm.cursor_mut(instr.p1)?.next();
            Ok(if has_row {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::IdxPrev => {
            let has_row = vm.cursor_mut(instr.p1)?.prev();
            Ok(if has_row {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::IdxRowid => {
            let rowid =
                vm.cursor(instr.p1)?
                    .idx_rowid()
                    .ok_or(ExecError::MalformedInstruction {
                        opcode: "IdxRowid",
                        reason: "cursor slot is not an index cursor, or has no current entry"
                            .to_string(),
                    })?;
            vm.set_register(instr.p2, Value::Integer(rowid))?;
            Ok(Step::Next)
        }
        Opcode::SeekIndexEq => {
            let (key, collations) = key_from_registers(vm, "SeekIndexEq", instr.p3, &instr.p4)?;
            let found = vm.cursor_mut(instr.p1)?.seek_index_eq(&key, &collations);
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::SeekIndexGE => {
            let (key, collations) = key_from_registers(vm, "SeekIndexGE", instr.p3, &instr.p4)?;
            let found = vm.cursor_mut(instr.p1)?.seek_index_ge(&key, &collations);
            Ok(if found {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::IdxCompareGT => {
            let (key, collations) = key_from_registers(vm, "IdxCompareGT", instr.p3, &instr.p4)?;
            let cmp = vm.cursor(instr.p1)?.idx_compare(&key, &collations).ok_or(
                ExecError::MalformedInstruction {
                    opcode: "IdxCompareGT",
                    reason: "cursor slot is not an index cursor, or has no current entry"
                        .to_string(),
                },
            )?;
            Ok(if cmp == Ordering::Greater {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::IdxCompareGE => {
            let (key, collations) = key_from_registers(vm, "IdxCompareGE", instr.p3, &instr.p4)?;
            let cmp = vm.cursor(instr.p1)?.idx_compare(&key, &collations).ok_or(
                ExecError::MalformedInstruction {
                    opcode: "IdxCompareGE",
                    reason: "cursor slot is not an index cursor, or has no current entry"
                        .to_string(),
                },
            )?;
            Ok(if cmp == Ordering::Less {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::IdxLE => {
            let (key, collations) = key_from_registers(vm, "IdxLE", instr.p3, &instr.p4)?;
            let cmp = vm.cursor(instr.p1)?.idx_compare(&key, &collations).ok_or(
                ExecError::MalformedInstruction {
                    opcode: "IdxLE",
                    reason: "cursor slot is not an index cursor, or has no current entry"
                        .to_string(),
                },
            )?;
            Ok(if cmp != Ordering::Greater {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::Found => {
            let (key, collations) = key_from_registers(vm, "Found", instr.p3, &instr.p4)?;
            let cursor = vm.cursor_mut(instr.p1)?;
            // Ephemeral index (DISTINCT / IN-subquery): membership of the
            // normalized key; real index: a seek.
            let found = match cursor.found(&key, &collations) {
                Some(present) => present,
                None => cursor.seek_index_eq(&key, &collations),
            };
            Ok(if found {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::Filter => {
            let (key, collations) = key_from_registers(vm, "Filter", instr.p3, &instr.p4)?;
            let maybe_present = vm.cursor(instr.p1)?.filter_maybe_present(&key, &collations);
            Ok(if maybe_present {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::FilterAdd => {
            let (key, collations) = key_from_registers(vm, "FilterAdd", instr.p3, &instr.p4)?;
            vm.cursor_mut(instr.p1)?.filter_add(&key, &collations);
            Ok(Step::Next)
        }
        Opcode::NoConflict => {
            let (key, collations) = key_from_registers(vm, "NoConflict", instr.p3, &instr.p4)?;
            let count = i32::try_from(key.len()).unwrap_or(i32::MAX);
            let cursor = vm.cursor_mut(instr.p1)?;
            // Ephemeral index: membership of the normalized key; real
            // index: a seek. sqlite-rs (`cursor::no_conflict`): on a
            // conflict the conflicting entry's rowid lands in the register
            // right after the probe (`p3 + count`) so OR REPLACE / OR
            // IGNORE can SeekRowid straight to the displaced row.
            let found = match cursor.found(&key, &collations) {
                Some(present) => present,
                None => cursor.seek_index_eq(&key, &collations),
            };
            if found {
                let rowid = cursor.idx_rowid();
                if let Some(rowid) = rowid {
                    let dest =
                        instr
                            .p3
                            .checked_add(count)
                            .ok_or(ExecError::RegisterRangeTooLarge {
                                opcode: "NoConflict",
                                count,
                            })?;
                    vm.set_register(dest, Value::Integer(rowid))?;
                }
                Ok(Step::Next)
            } else {
                Ok(Step::Jump(to_pc(instr.p2)))
            }
        }
        other => Err(ExecError::MalformedInstruction {
            opcode: "step",
            reason: format!("{other:?} dispatched to step_cursor_ops"),
        }),
    }
}
