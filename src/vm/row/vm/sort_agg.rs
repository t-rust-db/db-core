// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! [`super::step`]'s sort agg opcode family (#661, #670).

use super::*;

/// [`step`] for one family: sorters, hash aggregation and aggregate accumulators. `step` sends
/// only these opcodes here.
#[inline(always)]
pub(super) fn step_sort_agg(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    match instr.opcode {
        Opcode::SorterOpen => {
            let keys = match &instr.p4 {
                P4::SortKey(keys) => keys.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "SorterOpen",
                        reason: format!("expected a SortKey P4, got {other:?}"),
                    })
                }
            };
            let bound = if instr.p5 == 0 {
                None
            } else {
                // `LIMIT -1` (any negative) means "no limit" in SQLite, and a
                // non-integer limit register is unbounded too.
                match vm.register(instr.p2)? {
                    Value::Integer(n) if *n >= 0 => usize::try_from(*n).ok(),
                    _ => None,
                }
            };
            vm.open_cursor(instr.p1, Box::new(SorterCursor::new(keys, bound)))?;
            Ok(Step::Next)
        }
        Opcode::SorterInsert => {
            let blob = match vm.register(instr.p2)? {
                Value::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "SorterInsert",
                        found: value_kind(other),
                    })
                }
            };
            let inserted = vm.cursor_mut(instr.p1)?.sorter_insert(blob);
            if !inserted {
                return Err(ExecError::MalformedInstruction {
                    opcode: "SorterInsert",
                    reason: "cursor slot is not a sorter, or the record blob does not decode"
                        .to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::SorterSort | Opcode::Sort | Opcode::HashAggRewind => {
            let has_row = vm.cursor_mut(instr.p1)?.rewind()?;
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::SorterNext | Opcode::HashAggNext => {
            let has_row = vm.cursor_mut(instr.p1)?.next()?;
            Ok(if has_row {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::SorterData => {
            let blob =
                vm.cursor(instr.p1)?
                    .current_blob()
                    .ok_or(ExecError::MalformedInstruction {
                        opcode: "SorterData",
                        reason: "sorter has not been sorted yet, or has no current row".to_string(),
                    })?;
            vm.set_register(instr.p2, blob)?;
            Ok(Step::Next)
        }
        Opcode::HashAggOpen => {
            let keys = match &instr.p4 {
                P4::GroupKey(keys) => keys.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "HashAggOpen",
                        reason: format!("expected a GroupKey P4, got {other:?}"),
                    })
                }
            };
            vm.open_cursor(instr.p1, Box::new(HashAggCursor::new(keys)))?;
            Ok(Step::Next)
        }
        Opcode::HashAggFind => {
            let blob = match vm.register(instr.p2)? {
                Value::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::TypeMismatch {
                        opcode: "HashAggFind",
                        found: value_kind(other),
                    })
                }
            };
            let found = vm.cursor_mut(instr.p1)?.hash_agg_find(blob);
            if !found {
                return Err(ExecError::MalformedInstruction {
                opcode: "HashAggFind",
                reason:
                    "cursor slot is not a hash-aggregation cursor, or the record blob does not decode"
                        .to_string(),
            });
            }
            Ok(Step::Next)
        }
        Opcode::HashAggStep => {
            let (name, arity, collation) = match &instr.p4 {
                P4::AggFunc {
                    name,
                    arity,
                    collation,
                } => (name.as_str(), *arity, *collation),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "HashAggStep",
                        reason: format!("expected an AggFunc P4, got {other:?}"),
                    })
                }
            };
            let start = Vm::index("HashAggStep", instr.p2)?;
            let end = start
                .checked_add(arity)
                .filter(|&end| end <= MAX_REGISTERS)
                .ok_or(ExecError::RegisterOutOfRange {
                    opcode: "HashAggStep",
                    index: instr.p2,
                })?;
            if vm.registers.len() < end {
                vm.registers.resize(end, Value::Null);
            }
            #[allow(clippy::cast_sign_loss)]
            let slot = instr.p1.max(0) as usize;
            // Borrow the argument registers in place (no per-row `Vec` +
            // `Value` clones, #486): `registers` and `cursors` are
            // disjoint fields, so both borrows coexist.
            let cursor_idx = Vm::index("cursor slot write", instr.p3)?;
            let cursor = vm
                .cursors
                .get_mut(cursor_idx)
                .and_then(Option::as_mut)
                .ok_or(ExecError::CursorNotOpen { slot: instr.p3 })?;
            let args = vm.registers.get(start..end).unwrap_or(&[]);
            let handled = cursor
                .hash_agg_step(slot, name, args, collation)
                .map_err(|e| ExecError::MalformedInstruction {
                    opcode: "HashAggStep",
                    reason: e.to_string(),
                })?;
            if !handled {
                return Err(ExecError::MalformedInstruction {
                    opcode: "HashAggStep",
                    reason: "cursor slot has no current group".to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::HashAggData => {
            let blob =
                vm.cursor(instr.p1)?
                    .current_blob()
                    .ok_or(ExecError::MalformedInstruction {
                    opcode: "HashAggData",
                    reason:
                        "hash-aggregation cursor has not been rewound yet, or has no current group"
                            .to_string(),
                })?;
            let accumulators: Vec<Option<AggState>> = vm
                .cursor(instr.p1)?
                .hash_agg_group_accumulators()
                .map(<[_]>::to_vec)
                .ok_or_else(|| ExecError::MalformedInstruction {
                    opcode: "HashAggData",
                    reason: "cursor slot is not a hash-aggregation cursor".to_string(),
                })?;
            for (slot, state) in accumulators.into_iter().enumerate() {
                // Saturation is unreachable (slots are bounded by
                // MAX_REGISTERS) and would be caught by set_agg_context's
                // own range check anyway.
                let slot = i32::try_from(slot).unwrap_or(i32::MAX);
                match state {
                    Some(state) => vm.set_agg_context(slot, state)?,
                    None => vm.clear_agg_context(slot)?,
                }
            }
            vm.set_register(instr.p2, blob)?;
            Ok(Step::Next)
        }
        Opcode::AggStep => {
            let (name, arity, collation) = match &instr.p4 {
                P4::AggFunc {
                    name,
                    arity,
                    collation,
                } => (name.as_str(), *arity, *collation),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "AggStep",
                        reason: format!("expected an AggFunc P4, got {other:?}"),
                    })
                }
            };
            let mut args = Vec::with_capacity(arity);
            for i in 0..arity {
                let reg = instr
                    .p2
                    .checked_add(i32::try_from(i).unwrap_or(i32::MAX))
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "AggStep",
                        index: instr.p2,
                    })?;
                args.push(vm.register(reg)?.clone());
            }
            let current = if instr.p5 == 0 {
                vm.take_agg_context(instr.p1)?
            } else {
                None
            };
            let updated = aggregate::step(name, current, &args, collation).map_err(|e| {
                ExecError::MalformedInstruction {
                    opcode: "AggStep",
                    reason: e.to_string(),
                }
            })?;
            vm.set_agg_context(instr.p1, updated)?;
            Ok(Step::Next)
        }
        Opcode::AggFinal => {
            let descriptor = match &instr.p4 {
                P4::Str(s) => s.as_str(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "AggFinal",
                        reason: format!("expected a \"name(arity)\" string P4, got {other:?}"),
                    })
                }
            };
            let (name, _arity) = parse_function_descriptor(descriptor).ok_or_else(|| {
                ExecError::MalformedInstruction {
                    opcode: "AggFinal",
                    reason: format!("malformed aggregate descriptor {descriptor:?}"),
                }
            })?;
            let state = vm.agg_context(instr.p1)?;
            let result =
                aggregate::finalize(name, state).map_err(|e| ExecError::MalformedInstruction {
                    opcode: "AggFinal",
                    reason: e.to_string(),
                })?;
            vm.set_register(instr.p3, result)?;
            vm.clear_agg_context(instr.p1)?;
            Ok(Step::Next)
        }
        Opcode::AggReset => {
            vm.clear_agg_context(instr.p1)?;
            Ok(Step::Next)
        }

        other => Err(ExecError::MalformedInstruction {
            opcode: "step",
            reason: format!("{other:?} dispatched to step_sort_agg"),
        }),
    }
}
