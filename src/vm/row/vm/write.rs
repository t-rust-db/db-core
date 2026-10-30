// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! [`super::step`]'s write opcode family (#661, #670).

use super::*;

/// [`step`] for one family: row and index writes. `step` sends
/// only these opcodes here.
#[inline(always)]
pub(super) fn step_write(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    match instr.opcode {
        Opcode::Insert => {
            let rowid = match vm.register(instr.p2)? {
                Value::Integer(i) => *i,
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "Insert",
                        found: value_kind(other),
                    })
                }
            };
            let payload = match vm.register(instr.p3)? {
                Value::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "Insert",
                        found: value_kind(other),
                    })
                }
            };
            let encoding = vm.text_encoding;
            let cursor = vm.cursor_mut(instr.p1)?;
            // A storage-backed cursor takes the record bytes as they are
            // (byte-exact with MakeRecord); in-memory ones decode them.
            let inserted = match cursor.insert_payload(rowid, &payload)? {
                Some(ok) => ok,
                None => {
                    let values = decode_record(&payload, encoding).map_err(|e| {
                        ExecError::MalformedInstruction {
                            opcode: "Insert",
                            reason: e.to_string(),
                        }
                    })?;
                    cursor.insert(rowid, values)?
                }
            };
            if !inserted {
                return Err(ExecError::MalformedInstruction {
                    opcode: "Insert",
                    reason: "cursor slot does not support insertion".to_string(),
                });
            }
            vm.count_change(instr.p5);
            if instr.p5 & super::super::program::OPFLAG_LASTROWID != 0 {
                vm.connection.last_insert_rowid = rowid;
            }
            Ok(Step::Next)
        }

        Opcode::Update => {
            let rowid = match vm.register(instr.p2)? {
                Value::Integer(i) => *i,
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "Update",
                        found: value_kind(other),
                    })
                }
            };
            let payload = match vm.register(instr.p3)? {
                Value::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "Update",
                        found: value_kind(other),
                    })
                }
            };
            let encoding = vm.text_encoding;
            let cursor = vm.cursor_mut(instr.p1)?;
            let updated = match cursor.update_payload(rowid, &payload)? {
                Some(ok) => ok,
                None => {
                    // Cursor kinds without a combined rewrite (in-memory
                    // fixtures, ephemeral tables) fall back to the
                    // Delete+Insert pair a storage-backed cursor avoids.
                    if !cursor.delete()? {
                        return Err(ExecError::MalformedInstruction {
                            opcode: "Update",
                            reason: "cursor slot does not support deletion".to_string(),
                        });
                    }
                    match cursor.insert_payload(rowid, &payload)? {
                        Some(ok) => ok,
                        None => {
                            let values = decode_record(&payload, encoding).map_err(|e| {
                                ExecError::MalformedInstruction {
                                    opcode: "Update",
                                    reason: e.to_string(),
                                }
                            })?;
                            cursor.insert(rowid, values)?
                        }
                    }
                }
            };
            if !updated {
                return Err(ExecError::MalformedInstruction {
                    opcode: "Update",
                    reason: "cursor slot does not support update".to_string(),
                });
            }
            vm.count_change(instr.p5);
            Ok(Step::Next)
        }

        Opcode::Delete => {
            let deleted = vm.cursor_mut(instr.p1)?.delete()?;
            if !deleted {
                return Err(ExecError::MalformedInstruction {
                    opcode: "Delete",
                    reason: "cursor slot does not support deletion".to_string(),
                });
            }
            vm.count_change(instr.p5);
            Ok(Step::Next)
        }
        Opcode::NewRowid => {
            let next = vm.cursor(instr.p1)?.next_rowid()?;
            // sqlite-rs: p5 != 0 with P4::Str(table) is AUTOINCREMENT —
            // the schema hook consults/updates sqlite_sequence so a rowid
            // is never reused after a delete.
            let rowid = if instr.p5 != 0 {
                let table = match &instr.p4 {
                P4::Str(name) => name.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "NewRowid",
                        reason: format!(
                            "AUTOINCREMENT requested (P5 nonzero) but P4 is not a table-name string, got {other:?}"
                        ),
                    })
                }
            };
                let max_from_table = next.saturating_sub(1);
                vm.schema_storage
                    .as_deref_mut()
                    .ok_or(ExecError::SchemaStorageMissing { opcode: "NewRowid" })?
                    .autoincrement_rowid(&table, max_from_table)
                    .map_err(ExecError::SchemaStorageFailed)?
            } else {
                next
            };
            vm.set_register(instr.p2, Value::Integer(rowid))?;
            Ok(Step::Next)
        }
        Opcode::IdxInsert | Opcode::IdxDelete => {
            let opcode = if instr.opcode == Opcode::IdxInsert {
                "IdxInsert"
            } else {
                "IdxDelete"
            };
            let (key, collations) = key_from_registers(vm, opcode, instr.p2, &instr.p4)?;
            let ok = if instr.opcode == Opcode::IdxInsert {
                // sqlite-rs: on an ephemeral index the stored row is the key
                // columns plus `p5` trailing payload columns.
                let total = key.len().checked_add(usize::from(instr.p5)).ok_or(
                    ExecError::RegisterRangeTooLarge {
                        opcode,
                        count: i32::from(instr.p5),
                    },
                )?;
                let stored = register_range(vm, opcode, instr.p2, total)?;
                let cursor = vm.cursor_mut(instr.p1)?;
                match cursor.ephemeral_idx_insert(&key, &collations, stored) {
                    Some(ok) => ok,
                    None => cursor.idx_insert(key)?,
                }
            } else {
                vm.cursor_mut(instr.p1)?.idx_delete(&key)?
            };
            if !ok {
                return Err(ExecError::MalformedInstruction {
                    opcode,
                    reason:
                        "cursor slot does not support this index operation, or no matching entry"
                            .to_string(),
                });
            }
            Ok(Step::Next)
        }

        other => Err(ExecError::MalformedInstruction {
            opcode: "step",
            reason: format!("{other:?} dispatched to step_write"),
        }),
    }
}
