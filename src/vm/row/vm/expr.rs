// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! [`super::step`]'s expr opcode family (#661, #670).

use super::*;

/// [`step`] for one family: comparisons, arithmetic, casts and scalar functions. `step` sends
/// only these opcodes here.
#[inline(always)]
pub(super) fn step_expr(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    match instr.opcode {
        Opcode::Eq => compare_jump(vm, instr, |o| o == Ordering::Equal),
        Opcode::Ge => compare_jump(vm, instr, |o| o != Ordering::Less),
        Opcode::Gt => compare_jump(vm, instr, |o| o == Ordering::Greater),
        Opcode::Le => compare_jump(vm, instr, |o| o != Ordering::Greater),
        Opcode::Lt => compare_jump(vm, instr, |o| o == Ordering::Less),
        Opcode::RealAffinity => real_affinity(vm, instr),
        Opcode::Cast => cast(vm, instr),

        Opcode::Add => binary_op(vm, instr, coerce::checked_add),
        Opcode::Subtract => binary_op_reversed(vm, instr, coerce::checked_sub),
        Opcode::Multiply => binary_op(vm, instr, coerce::checked_mul),
        Opcode::Divide => binary_op_reversed(vm, instr, coerce::checked_div),
        Opcode::Remainder => binary_op_reversed(vm, instr, coerce::checked_rem),
        Opcode::Not => arith_not(vm, instr),
        Opcode::BitAnd => binary_op(vm, instr, coerce::bit_and),
        Opcode::BitOr => binary_op(vm, instr, coerce::bit_or),
        Opcode::ShiftLeft => binary_op_reversed(vm, instr, coerce::shift_left),
        Opcode::ShiftRight => binary_op_reversed(vm, instr, coerce::shift_right),
        Opcode::BitNot => bit_not(vm, instr),
        Opcode::Concat => binary_op_reversed(vm, instr, coerce::concat),

        Opcode::Function => {
            let descriptor = match &instr.p4 {
                P4::Str(s) => s.as_str(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Function",
                        reason: format!("expected a \"name(arity)\" string P4, got {other:?}"),
                    })
                }
            };
            // `name(arity);json=0,2`: the arguments at those positions
            // carry sqlite3's JSON subtype (#651).
            let (descriptor, json) = match descriptor.split_once(";json=") {
                Some((call, positions)) => (call, Some(positions)),
                None => (descriptor, None),
            };
            let (name, arity) = parse_function_descriptor(descriptor).ok_or_else(|| {
                ExecError::MalformedInstruction {
                    opcode: "Function",
                    reason: format!("malformed function descriptor {descriptor:?}"),
                }
            })?;
            // The connection's counters live in the VM, not the pure
            // function registry (#651). `total_changes` includes this
            // statement's writes so far, as in sqlite3.
            let connection = match (name.to_ascii_lowercase().as_str(), arity) {
                ("changes", 0) => Some(vm.connection.changes),
                ("total_changes", 0) => Some(vm.connection_state(false).total_changes),
                ("last_insert_rowid", 0) => Some(vm.connection.last_insert_rowid),
                _ => None,
            };
            if let Some(n) = connection {
                vm.set_register(instr.p3, Value::Integer(n))?;
                return Ok(Step::Next);
            }
            let mut args = Vec::with_capacity(arity);
            for i in 0..arity {
                let reg = instr
                    .p2
                    .checked_add(i32::try_from(i).unwrap_or(i32::MAX))
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "Function",
                        index: instr.p2,
                    })?;
                args.push(vm.register(reg)?.clone());
            }
            let result = match json {
                Some(positions) => {
                    let positions = positions
                        .split(',')
                        .map(|p| p.parse::<usize>().ok().filter(|&p| p < arity))
                        .collect::<Option<Vec<usize>>>()
                        .ok_or_else(|| ExecError::MalformedInstruction {
                            opcode: "Function",
                            reason: format!("malformed JSON argument positions {positions:?}"),
                        })?;
                    crate::functions::call_marked(name, &args, &positions)
                }
                None => functions::call_at(name, &args, vm.statement_now_ns),
            };
            let result = result.map_err(|e| match e {
                // A limit sqlite3 reports in its own words (#659).
                functions::FunctionError::TooBig => ExecError::TooBig,
                e => ExecError::MalformedInstruction {
                    opcode: "Function",
                    reason: e.to_string(),
                },
            })?;
            vm.set_register(instr.p3, result)?;
            Ok(Step::Next)
        }

        other => Err(ExecError::MalformedInstruction {
            opcode: "step",
            reason: format!("{other:?} dispatched to step_expr"),
        }),
    }
}
