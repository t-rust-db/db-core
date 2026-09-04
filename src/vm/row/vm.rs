//! The register file, cursor-slot table, aggregate-context slot table,
//! and fetch-decode-execute loop, ported from sqlite-rs's `vdbe::exec`
//! (db-core#51/#56/#59/#62/#64/#68/#69). Storage-agnostic: cursors are
//! [`super::cursor::Cursor`] trait objects, not real b-tree/pager
//! access -- see `super::cursor`'s doc and ADR 0008.
//!
//! **Scope of this phase.** Dispatched: control flow (`Init`/`Goto`/
//! `Once`/`BeginSubrtn`/`Return`/`Halt`/`IfNot`/`IfNotZero`/`IfPos`/
//! `DecrJumpZero`/`IsNull`/`NotNull`/`MustBeInt`/`OffsetLimit`), the
//! fused compare-jump opcodes (`Eq`/`Ge`/`Gt`/`Le`/`Lt`), `RealAffinity`/
//! `Cast`, arithmetic (`Add`/`Subtract`/`Multiply`/`Divide`/`Remainder`/
//! `Not`/`BitAnd`/`BitOr`/`ShiftLeft`/`ShiftRight`/`BitNot`/`Concat`),
//! result-row loads (`Integer`/`Int64`/`Real`/`Blob`/`Null`/`String8`/
//! `Variable`/`Copy`/`ResultRow`/`MakeRecord`, via [`super::record`]),
//! `Rewind`/`Next`/`Column`/`Rowid` over a [`super::cursor::Cursor`]
//! opened via [`Vm::open_cursor`], `OpenEphemeral`/`Insert` over an
//! in-memory [`super::cursor::EphemeralTableCursor`] (`Insert` decodes
//! `MakeRecord`'s blob straight back into `Value`s -- sqlite-rs's own
//! "decode-once-at-insert" design), `AggStep`/`AggFinal` over
//! [`super::aggregate::AggState`] (single-group only), `Function` over
//! [`super::functions::call`], and `SorterOpen`/`SorterInsert`/
//! `SorterSort`/`Sort`/`SorterNext`/`SorterData` over a
//! [`super::cursor::SorterCursor`] (single-key, no LIMIT/bound).
//! Everything else in [`super::program::Opcode`] returns
//! `ExecError::Unimplemented`.

use std::cmp::Ordering;
use std::collections::HashSet;

use super::affinity::{apply_affinity, Affinity};
use super::aggregate::{self, AggState};
use super::cast::cast_to;
use super::coerce;
use super::compare::compare;
use super::cursor::{Cursor, EphemeralTableCursor, SorterCursor};
use super::functions;
use super::program::{Instruction, Opcode, Program, P4};
use super::record::{decode_record, encode_record};
use super::value::{Collation, TextEncoding, Value};

/// Caps a single register index or range count -- a backstop against an
/// adversarial/corrupt instruction driving an oversized allocation.
pub(crate) const MAX_REGISTERS: usize = 1 << 20;

/// A backstop against a runaway/looping program with no `Halt`.
pub const MAX_STEPS: u64 = 1 << 24;

/// The ways the fetch-decode-execute loop can fail to run a [`Program`]
/// to completion.
#[derive(Debug)]
pub enum ExecError {
    /// `opcode` addressed register `index`, which lies outside the
    /// register file.
    RegisterOutOfRange { opcode: &'static str, index: i32 },
    /// `opcode` requested a register range of `count` registers, more
    /// than [`MAX_REGISTERS`] allows.
    RegisterRangeTooLarge { opcode: &'static str, count: i32 },
    /// `opcode` required a register to hold a particular [`Value`]
    /// variant but found `found` instead.
    TypeMismatch {
        opcode: &'static str,
        found: &'static str,
    },
    /// `MustBeInt`'s coercion failed.
    MustBeInt,
    /// `opcode`'s operands are structurally invalid for `reason`.
    MalformedInstruction {
        opcode: &'static str,
        reason: String,
    },
    /// `opcode` is a recognized opcode with no dispatch arm yet.
    Unimplemented { opcode: Opcode },
    /// `slot` was referenced but has no cursor open in it.
    CursorNotOpen { slot: i32 },
    /// A jump or fall-through moved the program counter past the end of
    /// the program's instructions.
    ProgramCounterOutOfRange { pc: usize },
    /// The program executed [`MAX_STEPS`] instructions without halting.
    StepLimitExceeded,
    /// The program executed `Halt` with a non-success result `code`.
    Halted { code: i32, message: Option<String> },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::RegisterOutOfRange { opcode, index } => {
                write!(f, "{opcode}: register index {index} is out of range")
            }
            ExecError::RegisterRangeTooLarge { opcode, count } => write!(
                f,
                "{opcode}: register range count {count} exceeds the maximum ({MAX_REGISTERS})"
            ),
            ExecError::TypeMismatch { opcode, found } => {
                write!(
                    f,
                    "{opcode}: expected a different value type, found {found}"
                )
            }
            ExecError::MustBeInt => write!(
                f,
                "MustBeInt: value cannot be converted to an integer without data loss"
            ),
            ExecError::MalformedInstruction { opcode, reason } => {
                write!(f, "{opcode}: malformed instruction ({reason})")
            }
            ExecError::Unimplemented { opcode } => {
                write!(f, "opcode {opcode:?} is not yet implemented by this VM")
            }
            ExecError::CursorNotOpen { slot } => write!(f, "cursor slot {slot} is not open"),
            ExecError::ProgramCounterOutOfRange { pc } => {
                write!(f, "program counter {pc} is out of range")
            }
            ExecError::StepLimitExceeded => write!(
                f,
                "program exceeded the maximum step count ({MAX_STEPS}) without halting"
            ),
            ExecError::Halted { code, message } => write!(
                f,
                "statement halted with result code {code}{}",
                message
                    .as_deref()
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            ),
        }
    }
}

impl std::error::Error for ExecError {}

/// The outcome of executing one instruction: fall through to PC+1, jump
/// to an explicit target, or halt the program.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Next,
    Jump(usize),
    Halt { code: i32, message: Option<String> },
}

#[allow(clippy::cast_sign_loss)]
fn to_pc(p2: i32) -> usize {
    p2.max(0) as usize
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Real(_) => "REAL",
        Value::Text(_) => "TEXT",
        Value::Blob(_) => "BLOB",
    }
}

/// Truthiness for the boolean-consuming opcodes: numeric-coerced zero
/// is false, everything else is true. NULL answers `false` here (it is
/// neither true nor false; callers decide what NULL means for them).
pub(crate) fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Integer(i) => *i == 0,
        Value::Real(r) => *r == 0.0,
        Value::Null => false,
        Value::Text(s) => match coerce::coerce_text_to_numeric(s) {
            Value::Integer(i) => i == 0,
            Value::Real(r) => r == 0.0,
            _ => true,
        },
        Value::Blob(_) => true,
    }
}

/// The VM's mutable execution state: a register file of `Value` cells,
/// a disjoint cursor-slot table, a disjoint aggregate-context slot
/// table (`AggStep`/`AggFinal`'s `p1`, same shape as `cursors`), plus
/// accumulated output rows and `Once`'s one-shot-guard bookkeeping.
#[derive(Default)]
pub struct Vm {
    registers: Vec<Value>,
    cursors: Vec<Option<Box<dyn Cursor>>>,
    agg_contexts: Vec<Option<AggState>>,
    rows: Vec<Vec<Value>>,
    once_fired: HashSet<usize>,
    params: Vec<Value>,
}

impl Vm {
    pub fn new() -> Self {
        Vm::default()
    }

    /// Binds parameter values for `Opcode::Variable`, 1-based.
    pub fn bind_params(&mut self, values: Vec<Value>) {
        self.params = values;
    }

    fn param(&self, index: i32) -> Option<&Value> {
        let idx = usize::try_from(index).ok()?.checked_sub(1)?;
        self.params.get(idx)
    }

    #[allow(clippy::cast_sign_loss)]
    fn index(opcode: &'static str, reg: i32) -> Result<usize, ExecError> {
        if reg < 0 || reg as usize > MAX_REGISTERS {
            return Err(ExecError::RegisterOutOfRange { opcode, index: reg });
        }
        Ok(reg as usize)
    }

    pub(crate) fn bounded_count(opcode: &'static str, count: i32) -> Result<usize, ExecError> {
        if !(0..=MAX_REGISTERS as i32).contains(&count) {
            return Err(ExecError::RegisterRangeTooLarge { opcode, count });
        }
        #[allow(clippy::cast_sign_loss)]
        Ok(count as usize)
    }

    /// Reads register `reg`. An unwritten register reads as NULL.
    pub fn register(&self, reg: i32) -> Result<&Value, ExecError> {
        let idx = Self::index("register read", reg)?;
        Ok(self.registers.get(idx).unwrap_or(&Value::Null))
    }

    /// Writes register `reg`, growing the register file with NULL
    /// filler as needed.
    pub fn set_register(&mut self, reg: i32, value: Value) -> Result<(), ExecError> {
        let idx = Self::index("register write", reg)?;
        if idx >= self.registers.len() {
            self.registers.resize(idx.saturating_add(1), Value::Null);
        }
        if let Some(slot) = self.registers.get_mut(idx) {
            *slot = value;
        }
        Ok(())
    }

    /// Takes register `reg`'s value, leaving NULL behind, without
    /// cloning (`ResultRow`'s hand-off).
    fn take_register(&mut self, reg: i32) -> Result<Value, ExecError> {
        let idx = Self::index("register read", reg)?;
        Ok(match self.registers.get_mut(idx) {
            Some(slot) => std::mem::replace(slot, Value::Null),
            None => Value::Null,
        })
    }

    /// Opens cursor slot `slot` with an arbitrary [`Cursor`]
    /// implementation -- the storage-agnostic wiring point ADR 0008
    /// calls for. Not an opcode: `OpenRead`'s real root-page/pager
    /// semantics are future work (`cursor.rs`'s db-storage wiring); a
    /// caller sets a program's cursors up via this method before
    /// running it.
    pub fn open_cursor(&mut self, slot: i32, cursor: Box<dyn Cursor>) -> Result<(), ExecError> {
        let idx = Self::index("cursor slot write", slot)?;
        if idx >= self.cursors.len() {
            self.cursors.resize_with(idx.saturating_add(1), || None);
        }
        if let Some(cell) = self.cursors.get_mut(idx) {
            *cell = Some(cursor);
        }
        Ok(())
    }

    fn cursor(&self, slot: i32) -> Result<&dyn Cursor, ExecError> {
        let idx = Self::index("cursor slot read", slot)?;
        self.cursors
            .get(idx)
            .and_then(Option::as_ref)
            .map(std::convert::AsRef::as_ref)
            .ok_or(ExecError::CursorNotOpen { slot })
    }

    fn cursor_mut(&mut self, slot: i32) -> Result<&mut Box<dyn Cursor>, ExecError> {
        let idx = Self::index("cursor slot write", slot)?;
        self.cursors
            .get_mut(idx)
            .and_then(Option::as_mut)
            .ok_or(ExecError::CursorNotOpen { slot })
    }

    /// Reads aggregate-context slot `slot`. `None` if no `AggStep` has
    /// run for this slot yet (or the group is empty) -- distinct from
    /// `cursor`'s error-on-unopened behavior, since an unaggregated
    /// slot is a legitimate zero-row state, not a malformed program.
    fn agg_context(&self, slot: i32) -> Result<Option<&AggState>, ExecError> {
        let idx = Self::index("agg context read", slot)?;
        Ok(self.agg_contexts.get(idx).and_then(Option::as_ref))
    }

    /// Writes aggregate-context slot `slot`, growing the table with
    /// empty filler as needed -- mirrors `open_cursor`'s growth policy,
    /// into the disjoint `agg_contexts` storage.
    fn set_agg_context(&mut self, slot: i32, value: AggState) -> Result<(), ExecError> {
        let idx = Self::index("agg context write", slot)?;
        if idx >= self.agg_contexts.len() {
            self.agg_contexts
                .resize_with(idx.saturating_add(1), || None);
        }
        if let Some(cell) = self.agg_contexts.get_mut(idx) {
            *cell = Some(value);
        }
        Ok(())
    }

    /// Clears aggregate-context slot `slot` back to `None` -- used by
    /// `AggFinal` so a slot's leftover accumulator from one invocation
    /// can't leak into a later invocation that skips `AggStep` entirely
    /// (a zero-row group, or a correlated aggregate subquery re-run
    /// once per outer row).
    fn clear_agg_context(&mut self, slot: i32) -> Result<(), ExecError> {
        let idx = Self::index("agg context clear", slot)?;
        if let Some(cell) = self.agg_contexts.get_mut(idx) {
            *cell = None;
        }
        Ok(())
    }

    /// Appends `row` to the set of rows produced so far.
    pub fn emit_row(&mut self, row: Vec<Value>) {
        self.rows.push(row);
    }

    /// The rows emitted by the program so far, in emission order.
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }
}

/// Compare opcodes (`Eq`/`Ge`/`Gt`/`Le`/`Lt`): jump to `p2` if `r[p1]
/// <op> r[p3]` holds. Either operand NULL means unknown, so no jump is
/// taken. `p4`, if [`P4::CollSeq`], selects collation/affinity for the
/// comparison; absent `p4` defaults to BINARY collation, BLOB affinity
/// (no coercion).
fn compare_jump(
    vm: &Vm,
    instr: &Instruction,
    holds: fn(Ordering) -> bool,
) -> Result<Step, ExecError> {
    let a = vm.register(instr.p1)?;
    let b = vm.register(instr.p3)?;
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Step::Next);
    }
    let (collation, affinity) = match &instr.p4 {
        P4::CollSeq {
            collation,
            affinity,
        } => (*collation, Affinity::from_p4_byte(*affinity)),
        _ => (Collation::Binary, Affinity::Blob),
    };
    let ord = if matches!(affinity, Affinity::Blob) {
        compare(a, b, collation)
    } else {
        let mut a = a.clone();
        let mut b = b.clone();
        apply_affinity(&mut a, affinity);
        apply_affinity(&mut b, affinity);
        compare(&a, &b, collation)
    };
    Ok(if holds(ord) {
        Step::Jump(to_pc(instr.p2))
    } else {
        Step::Next
    })
}

fn real_affinity(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    let mut v = vm.take_register(instr.p1)?;
    apply_affinity(&mut v, Affinity::Real);
    vm.set_register(instr.p1, v)?;
    Ok(Step::Next)
}

fn cast(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let affinity = Affinity::from_p4_byte(instr.p2 as u8);
    let v = vm.take_register(instr.p1)?;
    vm.set_register(instr.p1, cast_to(&v, affinity))?;
    Ok(Step::Next)
}

fn binary_op(
    vm: &mut Vm,
    instr: &Instruction,
    op: fn(&Value, &Value) -> Value,
) -> Result<Step, ExecError> {
    let result = {
        let a = vm.register(instr.p1)?;
        let b = vm.register(instr.p2)?;
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            Value::Null
        } else {
            op(a, b)
        }
    };
    vm.set_register(instr.p3, result)?;
    Ok(Step::Next)
}

/// `p2`-op-`p1` binary opcodes (sqlite-rs's operand order for
/// `Subtract`/`Divide`/`Remainder`/`ShiftLeft`/`ShiftRight`/`Concat`).
fn binary_op_reversed(
    vm: &mut Vm,
    instr: &Instruction,
    op: fn(&Value, &Value) -> Value,
) -> Result<Step, ExecError> {
    let result = {
        let a = vm.register(instr.p1)?;
        let b = vm.register(instr.p2)?;
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            Value::Null
        } else {
            op(b, a)
        }
    };
    vm.set_register(instr.p3, result)?;
    Ok(Step::Next)
}

fn arith_not(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    let result = match vm.register(instr.p1)? {
        Value::Null => Value::Null,
        other => Value::Integer(i64::from(is_falsy(other))),
    };
    vm.set_register(instr.p2, result)?;
    Ok(Step::Next)
}

fn bit_not(vm: &mut Vm, instr: &Instruction) -> Result<Step, ExecError> {
    let result = match vm.register(instr.p1)? {
        Value::Null => Value::Null,
        other => coerce::bit_not(other),
    };
    vm.set_register(instr.p2, result)?;
    Ok(Step::Next)
}

fn register_as_i64(vm: &Vm, reg: i32) -> Result<i64, ExecError> {
    match vm.register(reg)? {
        Value::Integer(i) => Ok(*i),
        other => Err(ExecError::TypeMismatch {
            opcode: "control counter",
            found: value_kind(other),
        }),
    }
}

fn in_i64_range(r: f64) -> bool {
    r >= i64::MIN as f64 && r < i64::MAX as f64
}

fn try_to_integer(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        #[allow(clippy::cast_possible_truncation)]
        Value::Real(r) if r.fract() == 0.0 && r.is_finite() && in_i64_range(*r) => Some(*r as i64),
        Value::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Parses a `"name(arity)"` descriptor (e.g. `"count(0)"`,
/// `"sum(1)"`) into its parts, used by `AggFinal`'s `p4`.
fn parse_function_descriptor(descriptor: &str) -> Option<(&str, usize)> {
    let open = descriptor.find('(')?;
    if !descriptor.ends_with(')') {
        return None;
    }
    let name = descriptor.get(..open)?;
    let inner_start = open.checked_add(1)?;
    let inner_end = descriptor.len().checked_sub(1)?;
    let arity: usize = descriptor.get(inner_start..inner_end)?.parse().ok()?;
    Some((name, arity))
}

/// Executes one instruction against `vm`, returning where control flow
/// goes next. `pc` is this instruction's own address (needed by
/// `Once`'s per-address guard).
#[allow(clippy::too_many_lines)]
fn step(vm: &mut Vm, pc: usize, instr: &Instruction) -> Result<Step, ExecError> {
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
        Opcode::Return => match vm.register(instr.p1)? {
            Value::Integer(i) => match i32::try_from(*i) {
                Ok(target) => Ok(Step::Jump(to_pc(target))),
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
            let bytes = match &instr.p4 {
                P4::Blob(bytes) => bytes.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "Blob",
                        reason: format!("expected a blob P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Blob(bytes.into()))?;
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
            let s = match &instr.p4 {
                P4::Str(s) => s.clone(),
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "String8",
                        reason: format!("expected a string P4, got {other:?}"),
                    })
                }
            };
            vm.set_register(instr.p2, Value::Text(s.into()))?;
            Ok(Step::Next)
        }
        Opcode::Variable => {
            let value = vm.param(instr.p1).cloned().unwrap_or(Value::Null);
            vm.set_register(instr.p2, value)?;
            Ok(Step::Next)
        }
        Opcode::Copy => {
            let value = vm.register(instr.p1)?.clone();
            vm.set_register(instr.p2, value)?;
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
                let reg = instr
                    .p1
                    .checked_add(i as i32)
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
                let reg = instr
                    .p1
                    .checked_add(i as i32)
                    .ok_or(ExecError::RegisterOutOfRange {
                        opcode: "ResultRow",
                        index: instr.p1,
                    })?;
                row.push(vm.take_register(reg)?);
            }
            vm.emit_row(row);
            Ok(Step::Next)
        }

        Opcode::Rewind => {
            let has_row = vm.cursor_mut(instr.p1)?.rewind();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::Next => {
            let has_row = vm.cursor_mut(instr.p1)?.next();
            Ok(if has_row {
                Step::Jump(to_pc(instr.p2))
            } else {
                Step::Next
            })
        }
        Opcode::Column => {
            #[allow(clippy::cast_sign_loss)]
            let col = instr.p2 as usize;
            let value = vm.cursor(instr.p1)?.column(col);
            vm.set_register(instr.p3, value)?;
            Ok(Step::Next)
        }
        Opcode::Rowid => {
            let rowid = vm.cursor(instr.p1)?.rowid();
            vm.set_register(instr.p2, Value::Integer(rowid))?;
            Ok(Step::Next)
        }
        Opcode::OpenEphemeral => {
            vm.open_cursor(instr.p1, Box::new(EphemeralTableCursor::new()))?;
            Ok(Step::Next)
        }
        Opcode::SorterOpen => {
            let key = match &instr.p4 {
                P4::SortKey(key) => *key,
                other => {
                    return Err(ExecError::MalformedInstruction {
                        opcode: "SorterOpen",
                        reason: format!("expected a SortKey P4, got {other:?}"),
                    })
                }
            };
            vm.open_cursor(instr.p1, Box::new(SorterCursor::new(key)))?;
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
                    reason: "cursor slot is not a sorter".to_string(),
                });
            }
            Ok(Step::Next)
        }
        Opcode::SorterSort | Opcode::Sort => {
            let has_row = vm.cursor_mut(instr.p1)?.rewind();
            Ok(if has_row {
                Step::Next
            } else {
                Step::Jump(to_pc(instr.p2))
            })
        }
        Opcode::SorterNext => {
            let has_row = vm.cursor_mut(instr.p1)?.next();
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
            let values = decode_record(&payload, TextEncoding::Utf8).map_err(|e| {
                ExecError::MalformedInstruction {
                    opcode: "Insert",
                    reason: e.to_string(),
                }
            })?;
            let inserted = vm.cursor_mut(instr.p1)?.insert(rowid, values);
            if !inserted {
                return Err(ExecError::MalformedInstruction {
                    opcode: "Insert",
                    reason: "cursor slot does not support insertion".to_string(),
                });
            }
            Ok(Step::Next)
        }

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
            let (name, arity) = parse_function_descriptor(descriptor).ok_or_else(|| {
                ExecError::MalformedInstruction {
                    opcode: "Function",
                    reason: format!("malformed function descriptor {descriptor:?}"),
                }
            })?;
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
            let result =
                functions::call(name, &args).map_err(|e| ExecError::MalformedInstruction {
                    opcode: "Function",
                    reason: e.to_string(),
                })?;
            vm.set_register(instr.p3, result)?;
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
                vm.agg_context(instr.p1)?.cloned()
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

        other => Err(ExecError::Unimplemented { opcode: other }),
    }
}

/// Runs `program` to completion (or the first error/step-limit),
/// returning the rows [`Opcode::ResultRow`] emitted.
pub fn execute(vm: &mut Vm, program: &Program) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut pc = 0usize;
    let mut steps = 0u64;
    loop {
        if pc >= program.instructions.len() {
            return Err(ExecError::ProgramCounterOutOfRange { pc });
        }
        steps = steps.saturating_add(1);
        if steps > MAX_STEPS {
            return Err(ExecError::StepLimitExceeded);
        }
        let instr = &program.instructions[pc];
        match step(vm, pc, instr)? {
            Step::Next => pc = pc.saturating_add(1),
            Step::Jump(target) => pc = target,
            Step::Halt { code: 0, .. } => return Ok(vm.rows().to_vec()),
            Step::Halt { code, message } => return Err(ExecError::Halted { code, message }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::cursor::InMemoryCursor;
    use super::super::program::{Instruction, Opcode, Program, P4};
    use super::*;

    fn run(instructions: Vec<Instruction>) -> Vec<Vec<Value>> {
        let mut vm = Vm::new();
        execute(&mut vm, &Program::new(instructions)).unwrap()
    }

    #[test]
    fn goto_jumps_unconditionally() {
        let rows = run(vec![
            Instruction::new(Opcode::Goto, 0, 2, 0),
            Instruction::new(Opcode::Halt, 1, 0, 0), // skipped
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert!(rows.is_empty());
    }

    #[test]
    fn once_falls_through_first_time_then_jumps_on_repeat_entry() {
        // A loop that visits the same `Once` instruction (pc 1) three
        // times: only the first pass runs the guarded `Integer` at pc 2
        // and appends a row; the next two passes jump straight to Halt.
        let mut vm = Vm::new();
        vm.set_register(0, Value::Integer(3)).unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::DecrJumpZero, 0, 5, 0), // pc 0: loop counter
            Instruction::new(Opcode::Once, 0, 4, 0),         // pc 1
            Instruction::new(Opcode::Integer, 9, 1, 0),      // pc 2: guarded body
            Instruction::new(Opcode::ResultRow, 1, 1, 0),    // pc 3
            Instruction::new(Opcode::Goto, 0, 0, 0),         // pc 4: back to loop top
            Instruction::new(Opcode::Halt, 0, 0, 0),         // pc 5
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(9)]]);
    }

    #[test]
    fn integer_and_result_row_emit_a_row() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 42, 0, 0),
            Instruction::new(Opcode::Integer, 7, 1, 0),
            Instruction::new(Opcode::ResultRow, 0, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(42), Value::Integer(7)]]);
    }

    #[test]
    fn eq_jumps_when_registers_are_equal_skipping_the_fall_through_write() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 5, 0, 0),
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::new(Opcode::Eq, 0, 4, 1), // jump to pc4 (skip pc3) when equal
            Instruction::new(Opcode::Integer, 999, 2, 0), // must be skipped
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]], "reg2 was never written");
    }

    #[test]
    fn eq_falls_through_when_registers_differ() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 5, 0, 0),
            Instruction::new(Opcode::Integer, 6, 1, 0),
            Instruction::new(Opcode::Eq, 0, 4, 1),
            Instruction::new(Opcode::Integer, 999, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(999)]]);
    }

    #[test]
    fn eq_does_not_jump_on_null_operand() {
        let rows = run(vec![
            Instruction::new(Opcode::Null, 0, 0, 0),
            Instruction::new(Opcode::Integer, 5, 1, 0),
            Instruction::new(Opcode::Eq, 0, 5, 1),
            Instruction::new(Opcode::Integer, 1, 2, 0),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn subtract_uses_sqlite_operand_order() {
        // r[p3] = r[p2] - r[p1]
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 3, 0, 0),  // p1
            Instruction::new(Opcode::Integer, 10, 1, 0), // p2
            Instruction::new(Opcode::Subtract, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(7)]]);
    }

    #[test]
    fn null_propagates_through_arithmetic() {
        let rows = run(vec![
            Instruction::new(Opcode::Null, 0, 0, 0),
            Instruction::new(Opcode::Integer, 2, 1, 0),
            Instruction::new(Opcode::Add, 0, 1, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn not_complements_and_propagates_null() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::Not, 0, 1, 0),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn if_not_jumps_on_falsy_register() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 0, 0, 0),
            Instruction::new(Opcode::IfNot, 0, 4, 0),
            Instruction::new(Opcode::Integer, 1, 1, 0),
            Instruction::new(Opcode::Goto, 0, 5, 0),
            Instruction::new(Opcode::Integer, 2, 1, 0),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn decr_jump_zero_terminates_at_zero() {
        let rows = run(vec![
            Instruction::new(Opcode::Integer, 1, 0, 0),
            Instruction::new(Opcode::DecrJumpZero, 0, 3, 0),
            Instruction::new(Opcode::Halt, 1, 0, 0),
            Instruction::new(Opcode::ResultRow, 0, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(0)]]);
    }

    #[test]
    fn cast_forces_target_affinity() {
        let rows = run(vec![
            Instruction::with_p4(Opcode::String8, 0, 0, 0, P4::Str("42".to_string())),
            Instruction::new(
                Opcode::Cast,
                0,
                i32::from(Affinity::Integer.to_p4_byte()),
                0,
            ),
            Instruction::new(Opcode::ResultRow, 0, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        assert_eq!(rows, vec![vec![Value::Integer(42)]]);
    }

    #[test]
    fn jump_past_the_end_of_the_program_is_an_error() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::new(Opcode::Goto, 0, 99, 0)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::ProgramCounterOutOfRange { pc: 99 })
        ));
    }

    #[test]
    fn halt_with_nonzero_code_is_an_error() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::new(Opcode::Halt, 1, 0, 0)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::Halted { code: 1, .. })
        ));
    }

    #[test]
    fn cursor_scan_reads_every_row_via_rewind_next_column_rowid() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![
                vec![Value::Integer(10)],
                vec![Value::Integer(20)],
            ])),
        )
        .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::Rewind, 0, 6, 0),
            Instruction::new(Opcode::Column, 0, 0, 1),
            Instruction::new(Opcode::Rowid, 0, 2, 0),
            Instruction::new(Opcode::ResultRow, 1, 2, 0),
            Instruction::new(Opcode::Next, 0, 1, 0),
            Instruction::new(Opcode::Goto, 0, 6, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(10), Value::Integer(1)],
                vec![Value::Integer(20), Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn unimplemented_opcode_errors_by_name() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::new(Opcode::HashAggOpen, 0, 0, 0)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::Unimplemented {
                opcode: Opcode::HashAggOpen
            })
        ));
    }

    #[test]
    fn make_record_output_matches_expected_encoding() {
        let mut vm = Vm::new();
        vm.set_register(0, Value::Integer(42)).unwrap();
        vm.set_register(1, Value::Text("abc".to_string().into()))
            .unwrap();
        let program = Program::new(vec![
            Instruction::new(Opcode::MakeRecord, 0, 2, 2),
            Instruction::new(Opcode::ResultRow, 2, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        let Value::Blob(payload) = &rows[0][0] else {
            panic!("expected a Blob");
        };
        assert_eq!(&payload[..], &[3, 1, 19, 42, b'a', b'b', b'c']);
    }

    #[test]
    fn make_record_applies_p4_affinity_before_encoding() {
        let mut vm = Vm::new();
        vm.set_register(0, Value::Text("42".to_string().into()))
            .unwrap();
        let program = Program::new(vec![
            Instruction::with_p4(
                Opcode::MakeRecord,
                0,
                1,
                1,
                P4::Affinity(vec![Affinity::Integer.to_p4_byte()]),
            ),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        let Value::Blob(payload) = &rows[0][0] else {
            panic!("expected a Blob");
        };
        // serial type 9 (constant 1) never appears for "42"; expect
        // integer serial type 1 (i8), body byte 42 -- proving affinity
        // coerced the text register before encoding, not after.
        assert_eq!(&payload[..], &[2, 1, 42]);
        // The source register is untouched -- affinity applies to a
        // copy, not the live register.
        assert_eq!(
            *vm.register(0).unwrap(),
            Value::Text("42".to_string().into())
        );
    }

    #[test]
    fn ephemeral_table_round_trips_makerecord_insert_scan() {
        // MakeRecord -> Insert -> Rewind/Next -> Column -> ResultRow:
        // the first genuinely complete end-to-end micro-query, entirely
        // storage-agnostic.
        let mut vm = Vm::new();
        let program = Program::new(vec![
            /* 0 */ Instruction::new(Opcode::OpenEphemeral, 0, 0, 0), // cursor 0
            // Row 1: (rowid=1, "a")
            /* 1 */
            Instruction::with_p4(Opcode::String8, 0, 1, 0, P4::Str("a".to_string())),
            /* 2 */
            Instruction::new(Opcode::MakeRecord, 1, 1, 2), // reg2 = record([reg1])
            /* 3 */ Instruction::new(Opcode::Integer, 1, 3, 0), // reg3 = rowid 1
            /* 4 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Row 2: (rowid=2, "b")
            /* 5 */
            Instruction::with_p4(Opcode::String8, 0, 1, 0, P4::Str("b".to_string())),
            /* 6 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 7 */ Instruction::new(Opcode::Integer, 2, 3, 0),
            /* 8 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Scan cursor 0, emitting (col0, rowid) per row.
            /* 9 */
            Instruction::new(Opcode::Rewind, 0, 14, 0), // jump to Halt(14) if empty
            /* 10 */ Instruction::new(Opcode::Column, 0, 0, 4),
            /* 11 */ Instruction::new(Opcode::Rowid, 0, 5, 0),
            /* 12 */ Instruction::new(Opcode::ResultRow, 4, 2, 0),
            /* 13 */
            Instruction::new(Opcode::Next, 0, 10, 0), // jump to Column(10) if a next row exists
            /* 14 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("a".to_string().into()), Value::Integer(1)],
                vec![Value::Text("b".to_string().into()), Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn insert_into_a_non_ephemeral_cursor_slot_errors() {
        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(super::super::cursor::InMemoryCursor::new(vec![])),
        )
        .unwrap();
        vm.set_register(1, Value::Integer(1)).unwrap();
        vm.set_register(
            2,
            Value::Blob(encode_record(&[], TextEncoding::Utf8).into()),
        )
        .unwrap();
        let program = Program::new(vec![Instruction::new(Opcode::Insert, 0, 1, 2)]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::MalformedInstruction {
                opcode: "Insert",
                ..
            })
        ));
    }

    #[test]
    fn count_and_sum_over_an_ephemeral_table_scan() {
        // MakeRecord/Insert three rows (10, 20, 30), then AggStep
        // COUNT(*)/SUM(x) over a Rewind/Next scan, AggFinal, ResultRow.
        let mut vm = Vm::new();
        let count_p4 = || P4::AggFunc {
            name: "count".to_string(),
            arity: 0,
            collation: Collation::Binary,
        };
        let sum_p4 = || P4::AggFunc {
            name: "sum".to_string(),
            arity: 1,
            collation: Collation::Binary,
        };
        let program = Program::new(vec![
            /* 0 */ Instruction::new(Opcode::OpenEphemeral, 0, 0, 0),
            // Row 1: (rowid=1, 10)
            /* 1 */
            Instruction::new(Opcode::Integer, 10, 1, 0),
            /* 2 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 3 */ Instruction::new(Opcode::Integer, 1, 3, 0),
            /* 4 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Row 2: (rowid=2, 20)
            /* 5 */
            Instruction::new(Opcode::Integer, 20, 1, 0),
            /* 6 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 7 */ Instruction::new(Opcode::Integer, 2, 3, 0),
            /* 8 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Row 3: (rowid=3, 30)
            /* 9 */
            Instruction::new(Opcode::Integer, 30, 1, 0),
            /* 10 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 11 */ Instruction::new(Opcode::Integer, 3, 3, 0),
            /* 12 */ Instruction::new(Opcode::Insert, 0, 3, 2),
            // Scan, accumulating COUNT(*) (slot 0) and SUM(col0) (slot 1).
            /* 13 */
            Instruction::new(Opcode::Rewind, 0, 21, 0), // jump to Halt(21) if empty
            /* 14 */ Instruction::new(Opcode::Column, 0, 0, 4),
            /* 15 */ Instruction::with_p4(Opcode::AggStep, 0, 4, 0, count_p4()),
            /* 16 */ Instruction::with_p4(Opcode::AggStep, 1, 4, 0, sum_p4()),
            /* 17 */
            Instruction::new(Opcode::Next, 0, 14, 0), // jump to Column(14) if a next row exists
            /* 18 */
            Instruction::with_p4(Opcode::AggFinal, 0, 0, 5, P4::Str("count(0)".to_string())),
            /* 19 */
            Instruction::with_p4(Opcode::AggFinal, 1, 0, 6, P4::Str("sum(1)".to_string())),
            /* 20 */ Instruction::new(Opcode::ResultRow, 5, 2, 0),
            /* 21 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(3), Value::Integer(60)]]);
    }

    #[test]
    fn agg_final_with_no_agg_step_finalizes_to_the_zero_row_result() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::with_p4(Opcode::AggFinal, 0, 0, 0, P4::Str("count(0)".to_string())),
            Instruction::with_p4(Opcode::AggFinal, 1, 0, 1, P4::Str("sum(1)".to_string())),
            Instruction::new(Opcode::ResultRow, 0, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(0), Value::Null]]);
    }

    #[test]
    fn function_calls_abs_and_upper_end_to_end() {
        let mut vm = Vm::new();
        let program = Program::new(vec![
            Instruction::new(Opcode::Integer, -5, 0, 0), // reg0 = -5
            Instruction::with_p4(Opcode::Function, 0, 0, 1, P4::Str("abs(1)".to_string())), // reg1 = abs(reg0)
            Instruction::with_p4(Opcode::String8, 0, 3, 0, P4::Str("hi".to_string())), // reg3 = "hi"
            Instruction::with_p4(Opcode::Function, 0, 3, 2, P4::Str("upper(1)".to_string())), // reg2 = upper(reg3)
            Instruction::new(Opcode::ResultRow, 1, 2, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(5),
                Value::Text("HI".to_string().into())
            ]]
        );
    }

    #[test]
    fn function_with_unknown_name_errors() {
        let mut vm = Vm::new();
        let program = Program::new(vec![Instruction::with_p4(
            Opcode::Function,
            0,
            0,
            0,
            P4::Str("median(0)".to_string()),
        )]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::MalformedInstruction {
                opcode: "Function",
                ..
            })
        ));
    }

    #[test]
    fn sorter_scans_makerecord_rows_back_in_sort_order() {
        // MakeRecord/SorterInsert three out-of-order rows, SorterSort,
        // then scan them back via SorterNext/SorterData -- each emitted
        // row is the raw record blob, decoded here the same way
        // sqlite-rs's own sorter test harness does (an OpenPseudo cursor
        // would normally feed Column, which isn't wired for sorters in
        // this minimal, single-key port).
        let mut vm = Vm::new();
        let key = P4::SortKey(super::super::program::SortKeyColumn {
            index: 0,
            descending: false,
            collation: Collation::Binary,
            nulls_first: false,
        });
        let program = Program::new(vec![
            /* 0 */ Instruction::with_p4(Opcode::SorterOpen, 0, 0, 0, key),
            /* 1 */ Instruction::new(Opcode::Integer, 30, 1, 0),
            /* 2 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 3 */ Instruction::new(Opcode::SorterInsert, 0, 2, 0),
            /* 4 */ Instruction::new(Opcode::Integer, 10, 1, 0),
            /* 5 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 6 */ Instruction::new(Opcode::SorterInsert, 0, 2, 0),
            /* 7 */ Instruction::new(Opcode::Integer, 20, 1, 0),
            /* 8 */ Instruction::new(Opcode::MakeRecord, 1, 1, 2),
            /* 9 */ Instruction::new(Opcode::SorterInsert, 0, 2, 0),
            /* 10 */
            Instruction::new(Opcode::SorterSort, 0, 14, 0), // jump to Halt(14) if empty
            /* 11 */ Instruction::new(Opcode::SorterData, 0, 3, 0),
            /* 12 */ Instruction::new(Opcode::ResultRow, 3, 1, 0),
            /* 13 */
            Instruction::new(Opcode::SorterNext, 0, 11, 0), // jump to SorterData(11) if a next row exists
            /* 14 */ Instruction::new(Opcode::Halt, 0, 0, 0),
        ]);
        let rows = execute(&mut vm, &program).unwrap();
        let decoded: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let Value::Blob(bytes) = &row[0] else {
                    panic!("expected a Blob");
                };
                decode_record(bytes, TextEncoding::Utf8).unwrap()[0].clone()
            })
            .collect();
        assert_eq!(
            decoded,
            vec![Value::Integer(10), Value::Integer(20), Value::Integer(30)]
        );
    }

    #[test]
    fn sorter_data_before_sort_errors() {
        let mut vm = Vm::new();
        let key = P4::SortKey(super::super::program::SortKeyColumn {
            index: 0,
            descending: false,
            collation: Collation::Binary,
            nulls_first: false,
        });
        let program = Program::new(vec![
            Instruction::with_p4(Opcode::SorterOpen, 0, 0, 0, key),
            Instruction::new(Opcode::SorterData, 0, 1, 0),
        ]);
        assert!(matches!(
            execute(&mut vm, &program),
            Err(ExecError::MalformedInstruction {
                opcode: "SorterData",
                ..
            })
        ));
    }
}
