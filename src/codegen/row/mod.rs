//! sqlite-rs-style planner -- one of `codegen`'s three planners (see
//! module docs).
//!
//! **Started (db-core#91, tracking issue db-core#20).** Ported from
//! sqlite-rs's `src/codegen.rs` + `src/codegen/expr.rs` +
//! `src/codegen/expr/{cond,value}.rs`, targeting
//! [`crate::vm::row::Opcode`] directly (the #18 decision) -- expressions
//! compile to jump-based control flow, never an intermediate boolean
//! register, exactly like the oracle.
//!
//! **Scoped down from a byte-faithful port** (see #91's comment
//! recording this decision): sqlite-rs's `Scope`/`RegAlloc` carry a full
//! table catalog, `ANALYZE` stats, join bindings, and correlated-
//! subquery hoisting/memoization caches -- none of which db-core has
//! yet (they belong to #92 joins / #95 subqueries), and none of which
//! db-core's current [`crate::expr::Expr`] (no `Case`/`Cast`/`Like`/
//! `Between`/`Exists`/qualified columns, unlike sqlite-rs's richer
//! `parser::ast::Expr`) could exercise anyway. This module ports the
//! *mechanism* -- [`Label`]/[`Target`]/[`NullTarget`]/[`CondTargets`]/
//! [`Emitter`], a plain-bump [`RegAlloc`] (no CTE cache, no
//! subquery-cursor allocation), and a single-table [`Scope`] (bare
//! column name to index, no catalog/joins/hoisting) -- sized to what
//! `Expr` has today. `InSubquery` codegen is deferred to #95, which is
//! where subquery materialization actually lands.
//!
//! [`TableSchema`] here is a placeholder: just enough (declared column
//! types, one optional rowid-alias column) for [`value::compile_value`]'s
//! column-read/affinity logic to be correct. It is not a real catalog --
//! #101/#102 are expected to replace it with one once a real join needs
//! multi-table resolution.
//!
//! [`select::compile_select`] (db-core#92) covers a single-table scan
//! plus projection (bare columns, `SELECT *`) plus `WHERE` plus
//! `LIMIT`. Joins and `ORDER BY` are deferred to #102 (mechanical
//! single-join/sorter execution); the join-order/access-path chooser
//! and `FULL OUTER` to #101 (needs `planner::Stats`, which db-core
//! doesn't have yet); `GROUP BY`/aggregation to #93.

#![forbid(unsafe_code)]

pub mod cond;
pub mod select;
pub mod value;

use std::collections::HashMap;
use std::fmt;

use crate::vm::row::{Instruction, Opcode, P4};

pub use cond::compile_cond;
pub use select::compile_select;
pub use value::compile_value;

/// The nesting bound this compiler enforces while walking an [`Expr`]
/// tree, mirroring sqlite-rs's ADR 0014 (200, deliberately below
/// SQLite's own 1000 -- a debug-build stack-overflow investigation
/// found the guard was otherwise unreachable at this crate's real
/// recursion depth per `Expr` level).
pub const MAX_EXPR_DEPTH: usize = 200;

/// Codegen failures: an unresolvable column, an `Expr` tree deeper than
/// [`MAX_EXPR_DEPTH`], or a construct this scoped-down compiler doesn't
/// implement yet (`InSubquery` -- see this module's doc comment).
#[derive(Debug, Clone, PartialEq)]
pub enum CodegenError {
    UnknownColumn(String),
    TooDeep,
    Unsupported { reason: String },
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodegenError::UnknownColumn(name) => write!(f, "unknown column: {name}"),
            CodegenError::TooDeep => {
                write!(f, "expression nesting exceeds {MAX_EXPR_DEPTH} levels")
            }
            CodegenError::Unsupported { reason } => write!(f, "unsupported: {reason}"),
        }
    }
}

impl std::error::Error for CodegenError {}

pub type Result<T> = std::result::Result<T, CodegenError>;

/// A not-yet-resolved jump target, placed later via [`Emitter::place`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Label(usize);

/// Where a boolean condition's true/false outcome continues: either an
/// explicit jump target, or "fall through to the next emitted
/// instruction" -- the classic jumping-code-generation technique, used
/// throughout this module so AND/OR compose without materializing an
/// intermediate boolean register.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    Jump(Label),
    Fallthrough,
}

/// Where a condition's *unknown* (SQL NULL) outcome continues -- names
/// one of [`CondTargets`]'s other two targets rather than being a third
/// [`Target`] of its own; see sqlite-rs's `NullTarget` doc for why an
/// independent third continuation doesn't work for `AND`/`OR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullTarget {
    /// NULL continues where [`CondTargets::on_true`] does.
    True,
    /// NULL continues where [`CondTargets::on_false`] does -- what
    /// `WHERE` wants.
    False,
}

/// The full jump-mode contract: where a condition's true, false, and
/// unknown outcomes each continue. Bundled rather than three parameters
/// because [`CondTargets::negate`] has to move all three together --
/// swapping true and false without flipping `on_null` reproduces
/// sqlite-rs's #134 bug.
#[derive(Debug, Clone, Copy)]
pub struct CondTargets {
    pub on_true: Target,
    pub on_false: Target,
    pub on_null: NullTarget,
}

impl CondTargets {
    /// The setting every boolean consumer here wants: unknown joins
    /// false.
    pub fn null_is_false(on_true: Target, on_false: Target) -> Self {
        CondTargets {
            on_true,
            on_false,
            on_null: NullTarget::False,
        }
    }

    /// Unknown joins true -- used only to separate "definitely false"
    /// from "unknown" when materializing a condition into a register.
    pub fn null_is_true(on_true: Target, on_false: Target) -> Self {
        CondTargets {
            on_true,
            on_false,
            on_null: NullTarget::True,
        }
    }

    /// The contract for the operand of a `NOT`: true and false trade
    /// places, and `on_null` flips so the unknown outcome still names
    /// the address it named before the swap.
    pub fn negate(self) -> Self {
        CondTargets {
            on_true: self.on_false,
            on_false: self.on_true,
            on_null: match self.on_null {
                NullTarget::True => NullTarget::False,
                NullTarget::False => NullTarget::True,
            },
        }
    }

    pub fn with_true(self, on_true: Target) -> Self {
        CondTargets { on_true, ..self }
    }

    pub fn with_false(self, on_false: Target) -> Self {
        CondTargets { on_false, ..self }
    }
}

/// Builds a [`crate::vm::row::Program`] with forward-referenceable jump
/// targets: `new_label`/`place` mark an address, `patch_p2` records a
/// pending fixup (every jump-carrying opcode this module emits targets
/// `p2`), and [`Emitter::finish`] resolves every pending fixup in one
/// pass.
#[derive(Debug, Default)]
pub struct Emitter {
    instructions: Vec<Instruction>,
    labels: HashMap<Label, usize>,
    patches: Vec<(usize, Label)>,
    next_label: usize,
}

impl Emitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn emit(&mut self, instr: Instruction) -> usize {
        self.instructions.push(instr);
        self.instructions.len().saturating_sub(1)
    }

    pub fn here(&self) -> usize {
        self.instructions.len()
    }

    pub fn new_label(&mut self) -> Label {
        let label = Label(self.next_label);
        self.next_label = self.next_label.saturating_add(1);
        label
    }

    /// Binds `label` to the current (next-to-be-emitted) address.
    pub fn place(&mut self, label: Label) {
        self.labels.insert(label, self.here());
    }

    pub fn patch_p2(&mut self, addr: usize, label: Label) {
        self.patches.push((addr, label));
    }

    /// Resolves every pending patch against its placed label's address,
    /// consuming the emitter into a finished [`crate::vm::row::Program`].
    pub fn finish(mut self) -> crate::vm::row::Program {
        for (addr, label) in &self.patches {
            let Some(&resolved) = self.labels.get(label) else {
                continue; // Every patched label is always placed by construction; skip defensively rather than panic.
            };
            #[allow(clippy::cast_possible_wrap)]
            let target = resolved as i32;
            if let Some(instr) = self.instructions.get_mut(*addr) {
                instr.p2 = target;
            }
        }
        crate::vm::row::Program::new(self.instructions)
    }

    /// Emits an unconditional jump to `label`, patched once placed.
    pub fn goto(&mut self, label: Label) {
        let addr = self.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
        self.patch_p2(addr, label);
    }
}

/// A monotonically-increasing register bump allocator -- the simplest
/// correct scheme for this scoped-down compiler; a real allocator that
/// reuses freed slots, and cursor/CTE-cache bookkeeping for subqueries,
/// are deferred to whichever sub-ticket (#95) actually needs them.
#[derive(Debug, Default)]
pub struct RegAlloc {
    next: i32,
}

impl RegAlloc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc(&mut self) -> i32 {
        let r = self.next;
        self.next = self.next.saturating_add(1);
        r
    }

    /// The register the next [`RegAlloc::alloc`] call would hand out,
    /// without allocating it.
    pub fn peek(&self) -> i32 {
        self.next
    }
}

pub(crate) fn p4_coll_seq(
    collation: crate::vm::row::Collation,
    affinity: crate::vm::row::Affinity,
) -> P4 {
    P4::CollSeq {
        collation,
        affinity: affinity.to_p4_byte(),
    }
}

/// A placeholder single-table schema -- see this module's doc comment
/// for why it isn't a real catalog yet.
#[derive(Debug, Clone, Default)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<String>,
    /// Declared type string per column, parallel to `columns` -- used
    /// only for [`crate::vm::row::affinity_of`]. A missing/short entry
    /// is treated as no declared type (BLOB affinity).
    pub column_types: Vec<String>,
    /// The `INTEGER PRIMARY KEY` rowid-alias column, if any -- reading
    /// it must emit `Opcode::Rowid` rather than `Opcode::Column` (see
    /// [`value::emit_column_read`]'s doc comment).
    pub rowid_alias: Option<usize>,
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
    }
}

/// The single table a query's column references resolve against --
/// db-core's current [`crate::expr::Expr::Column`] has no qualifier, so
/// unlike sqlite-rs's `Scope` this never needs to disambiguate between
/// multiple bindings. Multi-table resolution (joins, subqueries) is
/// deferred to #92/#95, which are expected to grow this into something
/// closer to sqlite-rs's own `Scope`.
#[derive(Debug, Clone)]
pub struct Scope {
    pub schema: TableSchema,
    pub cursor: i32,
}

impl Scope {
    pub fn single(schema: TableSchema, cursor: i32) -> Self {
        Scope { schema, cursor }
    }

    /// Resolves a bare column name to `(cursor, column_index)`.
    pub fn resolve(&self, name: &str) -> Result<(i32, usize)> {
        self.schema
            .column_index(name)
            .map(|idx| (self.cursor, idx))
            .ok_or_else(|| CodegenError::UnknownColumn(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitter_resolves_forward_jump() {
        let mut em = Emitter::new();
        let label = em.new_label();
        let addr = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
        em.patch_p2(addr, label);
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        em.place(label);
        let program = em.finish();
        assert_eq!(program.instructions[0].p2, 2);
    }

    #[test]
    fn cond_targets_negate_flips_null_target() {
        let true_label = Label(0);
        let false_label = Label(1);
        let targets =
            CondTargets::null_is_false(Target::Jump(true_label), Target::Jump(false_label));
        let negated = targets.negate();
        assert_eq!(negated.on_null, NullTarget::True);
        assert!(matches!(negated.on_true, Target::Jump(l) if l == false_label));
        assert!(matches!(negated.on_false, Target::Jump(l) if l == true_label));
    }

    #[test]
    fn reg_alloc_hands_out_increasing_registers() {
        let mut reg = RegAlloc::new();
        assert_eq!(reg.alloc(), 0);
        assert_eq!(reg.alloc(), 1);
        assert_eq!(reg.peek(), 2);
    }

    #[test]
    fn scope_resolves_known_column_and_rejects_unknown() {
        let schema = TableSchema {
            name: "t".into(),
            columns: vec!["a".into(), "b".into()],
            column_types: vec![String::new(), String::new()],
            rowid_alias: None,
        };
        let scope = Scope::single(schema, 3);
        assert_eq!(scope.resolve("b"), Ok((3, 1)));
        assert_eq!(scope.resolve("B"), Ok((3, 1)));
        assert_eq!(
            scope.resolve("z"),
            Err(CodegenError::UnknownColumn("z".to_string()))
        );
    }
}
