// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! [`super::step`]'s control opcode family (#661, #670).

use super::*;

/// [`step`] for one family: jumps, branches and subroutines. `step` sends
/// only these opcodes here.
#[inline(always)]
pub(super) fn step_control(vm: &mut Vm, pc: usize, instr: &Instruction) -> Result<Step, ExecError> {
    match instr.opcode {
        Opcode::Init => Ok(if instr.p2 == 0 {
            Step::Next
        } else {
            Step::Jump(to_pc(instr.p2))
        }),
        Opcode::Goto => Ok(Step::Jump(to_pc(instr.p2))),
        Opcode::Once => {
            if vm.once_fired.insert(pc) {
                Ok(Step::Next)
            } else {
                Ok(Step::Jump(to_pc(instr.p2)))
            }
        }
        Opcode::BeginSubrtn => Ok(Step::Next),
        Opcode::Gosub => {
            let return_addr = i32::try_from(pc).map_err(|_| ExecError::MalformedInstruction {
                opcode: "Gosub",
                reason: format!("instruction address {pc} does not fit in a PC"),
            })?;
            vm.set_register(instr.p1, Value::Integer(i64::from(return_addr)))?;
            Ok(Step::Jump(to_pc(instr.p2)))
        }
        Opcode::Return => match vm.register(instr.p1)? {
            Value::Integer(i) => match i32::try_from(*i) {
                Ok(target) => Ok(Step::Jump(to_pc(target).saturating_add(1))),
                Err(_) => Err(ExecError::MalformedInstruction {
                    opcode: "Return",
                    reason: format!("return address {i} does not fit in a PC"),
                }),
            },
            other => Err(ExecError::TypeMismatch {
                opcode: "Return",
                found: value_kind(other),
            }),
        },
        Opcode::Halt => {
            let message = match &instr.p4 {
                P4::Str(s) => Some(s.clone()),
                _ => None,
            };
            Ok(Step::Halt {
                code: instr.p1,
                message,
            })
        }
        Opcode::IsNull => {
            let jump = matches!(vm.register(instr.p1)?, Value::Null);
            Ok(if jump {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::NotNull => {
            let jump = !matches!(vm.register(instr.p1)?, Value::Null);
            Ok(if jump {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::IfNot => {
            let v = vm.register(instr.p1)?;
            let take_jump = match v {
                Value::Null => instr.p3 != 0,
                other => is_falsy(other),
            };
            Ok(if take_jump {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::MustBeInt => {
            let v = vm.register(instr.p1)?.clone();
            match try_to_integer(&v) {
                Some(i) => {
                    vm.set_register(instr.p1, Value::Integer(i))?;
                    Ok(Step::Next)
                }
                None if instr.p2 != 0 => Ok(Step::Jump(to_pc(instr.p2))),
                None => Err(ExecError::MustBeInt),
            }
        }
        Opcode::OffsetLimit => {
            let limit = register_as_i64(vm, instr.p1)?;
            let offset = register_as_i64(vm, instr.p3)?;
            let combined = if limit > 0 {
                limit.saturating_add(offset.max(0))
            } else {
                -1
            };
            vm.set_register(instr.p2, Value::Integer(combined))?;
            Ok(Step::Next)
        }
        Opcode::IfPos => {
            let v = register_as_i64(vm, instr.p1)?;
            if v > 0 {
                vm.set_register(
                    instr.p1,
                    Value::Integer(v.saturating_sub(i64::from(instr.p3))),
                )?;
                Ok(Step::Jump(to_pc(instr.p2)))
            } else {
                Ok(Step::Next)
            }
        }
        Opcode::IfNotZero => {
            let v = register_as_i64(vm, instr.p1)?;
            if v != 0 {
                if v > 0 {
                    vm.set_register(instr.p1, Value::Integer(v.saturating_sub(1)))?;
                }
                Ok(Step::Jump(to_pc(instr.p2)))
            } else {
                Ok(Step::Next)
            }
        }
        Opcode::DecrJumpZero => {
            let v = register_as_i64(vm, instr.p1)?.saturating_sub(1);
            vm.set_register(instr.p1, Value::Integer(v))?;
            Ok(if v == 0 {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }

        other => Err(ExecError::MalformedInstruction {
            opcode: "step",
            reason: format!("{other:?} dispatched to step_control"),
        }),
    }
}
