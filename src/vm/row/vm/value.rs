// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! [`super::step`]'s value opcode family (#661, #670).

use super::*;

/// [`step`] for one family: constants, register copies, records and result rows. `step` sends
/// only these opcodes here.
#[inline(always)]
pub(super) fn step_value(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    match instr.opcode {
        Opcode::Integer => {
            vm.set_register(instr.p2, Value::Integer(i64::from(instr.p1)))?;
            Ok(Step::Next)
        }
        Opcode::Int64 => {
            let i = match &instr.p4 {
                P4::Int(i) => *i,
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Int64",
                        reason: format!("expected an integer P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Integer(i))?;
            Ok(Step::Next)
        }
        Opcode::Real => {
            let r = match &instr.p4 {
                P4::Real(r) => *r,
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Real",
                        reason: format!("expected a real P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Real(r))?;
            Ok(Step::Next)
        }
        Opcode::Blob => {
            // One allocation: straight from the P4 slice into the `Rc` (#259).
            let bytes: &[u8] = match &instr.p4 {
                P4::Blob(bytes) => bytes.as_slice(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Blob",
                        reason: format!("expected a blob P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Blob(Rc::from(bytes)))?;
            Ok(Step::Next)
        }
        Opcode::Null => {
            let last = instr.p3.max(instr.p2);
            for reg in instr.p2..=last {
                vm.set_register(reg, Value::Null)?;
            }
            Ok(Step::Next)
        }
        Opcode::String8 => {
            let s: &str = match &instr.p4 {
                P4::Str(s) => s.as_str(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "String8",
                        reason: format!("expected a string P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Text(Rc::from(s)))?;
            Ok(Step::Next)
        }
        Opcode::Variable => {
            // An unbound host parameter reads as NULL (SQLite semantics).
            let value = vm.param(instr.p1)?.cloned().unwrap_or(Value::Null);
            vm.set_register(instr.p2, value)?;
            Ok(Step::Next)
        }
        Opcode::Copy => {
            // `p3` extra registers beyond the first (SQLite `OP_Copy`, #260).
            let extra = usize::try_from(instr.p3).map_err(|_| ExecError::MalformedInstruction {
                opcode: "Copy",
                reason: format!("register count {} is negative", instr.p3),
            })?;
            for i in 0..=extra {
                let offset = i32::try_from(i).unwrap_or(i32::MAX);
                let (src, dst) = match (instr.p1.checked_add(offset), instr.p2.checked_add(offset))
                {
                    (Some(src), Some(dst)) => (src, dst),
                    _ => {
                        return Err(ExecError::RegisterOutOfRange {
                            opcode: "Copy",
                            index: instr.p1,
                        })
                    }
                };
                let value = vm.register(src)?.clone();
                vm.set_register(dst, value)?;
            }
            Ok(Step::Next)
        }
        Opcode::MakeRecord => {
            let count = Vm::bounded_count("MakeRecord", instr.p2)?;
            let affinities: &[u8] = match &instr.p4 {
                P4::Affinity(bytes) => bytes,
                _ => &[],
            };
            let mut values = Vec::with_capacity(count);
            for i in 0..count {
                let reg = i32::try_from(i)
                    .ok()
                    .and_then(|i| instr.p1.checked_add(i))
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "MakeRecord",
                        index: instr.p1,
                    })?;
                let mut value = vm.register(reg)?.clone();
                if let Some(byte) = affinities.get(i) {
                    apply_affinity(&mut value, Affinity::from_p4_byte(*byte));
                }
                values.push(value);
            }
            let payload = encode_record(&values, TextEncoding::Utf8);
            vm.set_register(instr.p3, Value::Blob(payload.into()))?;
            Ok(Step::Next)
        }
        Opcode::ResultRow => {
            let count = Vm::bounded_count("ResultRow", instr.p2)?;
            let mut row = Vec::with_capacity(count);
            for i in 0..count {
                let reg = i32::try_from(i)
                    .ok()
                    .and_then(|i| instr.p1.checked_add(i))
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "ResultRow",
                        index: instr.p1,
                    })?;
                row.push(vm.take_register(reg)?);
            }
            vm.emit_row(row);
            Ok(Step::Next)
        }

        other => Err(ExecError::MalformedInstruction {
            opcode: "step",
            reason: format!("{other:?} dispatched to step_value"),
        }),
    }
}
