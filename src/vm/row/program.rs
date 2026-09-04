//! `vm::row`'s `Opcode`/`Instruction`/`Program` skeleton (ADR 0008,
//! following ADR 0007's shape for `vm::batch`): one instruction list,
//! `EXPLAIN`-listable, typed operands on `Opcode` rather than
//! sqlite-rs's raw `p1..p5` integer slots.
//!
//! **Partial today.** This only carries the opcodes needed to exercise
//! the value-semantics slice ported alongside it (comparison, cast,
//! coercion, arithmetic, three-valued logic) -- not sqlite-rs's full
//! ~65-opcode VDBE set. The execution loop, cursor opcodes, and the
//! rest of the set land with the next phase of db-core#18.

use super::affinity::Affinity;
use super::value::{Collation, Value};

/// A comparison operator, as used by [`Opcode::Compare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Lt,
    Is,
    IsNot,
}

/// A checked arithmetic/bitwise binary operator, as used by
/// [`Opcode::Arith`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    ShiftLeft,
    ShiftRight,
    Concat,
}

/// Three-valued logic connective, as used by [`Opcode::Logic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicOp {
    And,
    Or,
}

/// One row-VM instruction. Registers hold one [`Value`] each (contrast
/// [`crate::vm::batch`]'s registers, which each hold a whole column).
#[derive(Debug, Clone, PartialEq)]
pub enum Opcode {
    /// Load a constant into a register.
    LoadConst { reg: usize, value: Value },
    /// `registers[dst] = op(registers[a], registers[b])`, three-valued
    /// (NULL-propagating per [`CompareOp::Eq`]/[`CompareOp::Lt`]'s rule,
    /// non-propagating for `Is`/`IsNot`) -- `dst` holds `Value::Integer`
    /// 0/1 for a `Some` result, `Value::Null` for `None`.
    Compare {
        dst: usize,
        op: CompareOp,
        a: usize,
        b: usize,
        collation: Collation,
    },
    /// `registers[dst] = NOT registers[a]`, three-valued (`Value::Null`
    /// in, `Value::Null` out).
    Not { dst: usize, a: usize },
    /// `registers[dst] = registers[a] op registers[b]`, three-valued
    /// logic ([`LogicOp`]) over registers holding `Value::Integer(0|1)`
    /// or `Value::Null`.
    Logic {
        dst: usize,
        op: LogicOp,
        a: usize,
        b: usize,
    },
    /// `registers[dst] = registers[a] op registers[b]`, checked
    /// arithmetic/bitwise ([`ArithOp`]) with text-operand coercion and
    /// REAL-promotion-on-overflow.
    Arith {
        dst: usize,
        op: ArithOp,
        a: usize,
        b: usize,
    },
    /// `registers[dst] = ~registers[a]`, coerced to INTEGER first.
    BitNot { dst: usize, a: usize },
    /// `registers[dst] = -registers[a]` (`Int`/`Real` negate; anything
    /// else, including `Null`, is `Null`).
    Neg { dst: usize, a: usize },
    /// `registers[dst] = CAST(registers[a] AS <target>)`.
    Cast {
        dst: usize,
        a: usize,
        target: Affinity,
    },
    /// Applies column-declared-type affinity to a register in place
    /// (`registers[reg] = apply_affinity(registers[reg], affinity)`).
    ApplyAffinity { reg: usize, affinity: Affinity },
    /// Terminates the program.
    Halt,
}

/// One instruction plus an optional `EXPLAIN` comment, mirroring
/// sqlite-rs's `EXPLAIN`-listing convention (ADR 0007).
#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub opcode: Opcode,
    pub comment: Option<String>,
}

impl Instruction {
    pub fn new(opcode: Opcode) -> Self {
        Instruction {
            opcode,
            comment: None,
        }
    }

    pub fn with_comment(opcode: Opcode, comment: impl Into<String>) -> Self {
        Instruction {
            opcode,
            comment: Some(comment.into()),
        }
    }
}

/// An executable row-VM program: a flat instruction list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Program {
    pub instructions: Vec<Instruction>,
}

impl Program {
    pub fn new() -> Self {
        Program::default()
    }

    pub fn push(&mut self, opcode: Opcode) -> &mut Self {
        self.instructions.push(Instruction::new(opcode));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_push_appends_instructions_in_order() {
        let mut program = Program::new();
        program
            .push(Opcode::LoadConst {
                reg: 0,
                value: Value::Integer(1),
            })
            .push(Opcode::Halt);
        assert_eq!(program.instructions.len(), 2);
        assert_eq!(program.instructions[1].opcode, Opcode::Halt);
    }

    #[test]
    fn instruction_with_comment_carries_it() {
        let instr = Instruction::with_comment(Opcode::Halt, "done");
        assert_eq!(instr.comment.as_deref(), Some("done"));
    }
}
