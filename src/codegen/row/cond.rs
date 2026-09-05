//! Jump-mode condition compilation -- see `super`'s module doc.

use super::value::{compile_value_depth, expr_affinity};
use super::{
    p4_coll_seq, CodegenError, CondTargets, Emitter, Label, NullTarget, RegAlloc, Result, Scope,
    Target, MAX_EXPR_DEPTH,
};
use crate::expr::{BinOp, Expr};
use crate::vm::row::{comparison_affinity, Collation, Instruction, Opcode};

/// A rough, static cost class for an expression, used only to order
/// `AND`/`OR` operands cheapest-first so short-circuit evaluation skips
/// the pricier side more often -- never to change what a query returns.
/// Lower is cheaper.
fn cost_class(expr: &Expr) -> u8 {
    match expr {
        Expr::Literal(_) | Expr::Column(_) => 0,
        Expr::Not(inner) | Expr::Neg(inner) => cost_class(inner).max(1),
        Expr::IsNull { expr: inner, .. } => cost_class(inner).max(1),
        Expr::BinaryOp(lhs, op, rhs) => {
            let base = match op {
                BinOp::And | BinOp::Or => 0,
                _ => 1,
            };
            cost_class(lhs).max(cost_class(rhs)).max(base)
        }
        Expr::InSubquery { .. } => 3,
    }
}

/// Whether `rhs` is strictly cheaper than `lhs` -- safe for `AND`/`OR`
/// because both are commutative under SQL's three-valued logic and
/// this crate's expression language has no operand with an evaluation
/// side effect that order could observably change. Ties keep the
/// original left-to-right order.
fn rhs_is_cheaper(lhs: &Expr, rhs: &Expr) -> bool {
    cost_class(rhs) < cost_class(lhs)
}

/// Compiles `expr` as a boolean condition. See [`CondTargets`]'s doc
/// comment for the true/false/unknown continuation contract.
pub fn compile_cond(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    expr: &Expr,
    targets: CondTargets,
) -> Result<()> {
    compile_cond_depth(em, reg, scope, expr, targets, 0)
}

pub(crate) fn compile_cond_depth(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    expr: &Expr,
    targets: CondTargets,
    depth: usize,
) -> Result<()> {
    if depth > MAX_EXPR_DEPTH {
        return Err(CodegenError::TooDeep);
    }
    match expr {
        // Swapping the targets is right, but only once `on_null` comes
        // along for the ride -- flipping it keeps the unknown outcome
        // on the same address across the swap.
        Expr::Not(inner) => compile_cond_depth(em, reg, scope, inner, targets.negate(), depth + 1),

        Expr::BinaryOp(lhs, BinOp::And, rhs) => {
            let (first, second) = if rhs_is_cheaper(lhs, rhs) {
                (rhs.as_ref(), lhs.as_ref())
            } else {
                (lhs.as_ref(), rhs.as_ref())
            };
            let (false_label, is_new) = ensure_label(em, targets.on_false);
            let operand = targets.with_false(Target::Jump(false_label));
            compile_cond_depth(
                em,
                reg,
                scope,
                first,
                operand.with_true(Target::Fallthrough),
                depth + 1,
            )?;
            compile_cond_depth(em, reg, scope, second, operand, depth + 1)?;
            if is_new {
                em.place(false_label);
            }
            Ok(())
        }

        Expr::BinaryOp(lhs, BinOp::Or, rhs) => {
            let (first, second) = if rhs_is_cheaper(lhs, rhs) {
                (rhs.as_ref(), lhs.as_ref())
            } else {
                (lhs.as_ref(), rhs.as_ref())
            };
            let (true_label, is_new) = ensure_label(em, targets.on_true);
            let operand = targets.with_true(Target::Jump(true_label));
            compile_cond_depth(
                em,
                reg,
                scope,
                first,
                operand.with_false(Target::Fallthrough),
                depth + 1,
            )?;
            compile_cond_depth(em, reg, scope, second, operand, depth + 1)?;
            if is_new {
                em.place(true_label);
            }
            Ok(())
        }

        Expr::BinaryOp(lhs, op, rhs)
            if matches!(
                op,
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
            ) =>
        {
            let affinity =
                comparison_affinity(expr_affinity(scope, lhs), expr_affinity(scope, rhs));
            let l = compile_value_depth(em, reg, scope, lhs, depth + 1)?;
            let r = compile_value_depth(em, reg, scope, rhs, depth + 1)?;
            emit_compare_false_jump(em, *op, l, r, affinity, targets)
        }

        Expr::IsNull {
            expr: inner,
            negated,
        } => {
            let r = compile_value_depth(em, reg, scope, inner, depth + 1)?;
            // negated=false (IS NULL): condition true iff NULL, so its
            // false-jump primitive fires when NOT null -> `NotNull`.
            // negated=true (IS NOT NULL): condition true iff not NULL,
            // so its false-jump primitive fires when NULL -> `IsNull`.
            let false_jump_op = if *negated {
                Opcode::IsNull
            } else {
                Opcode::NotNull
            };
            finish_bool(em, targets.on_true, targets.on_false, |em, false_label| {
                let addr = em.emit(Instruction::new(false_jump_op, r, 0, 0));
                em.patch_p2(addr, false_label);
            });
            Ok(())
        }

        Expr::InSubquery { .. } => Err(CodegenError::Unsupported {
            reason: "InSubquery codegen is deferred to #95 (subquery materialization)".to_string(),
        }),

        // Any other expression used in boolean context (a bare column,
        // arithmetic, etc.): evaluate to a value and test truthiness.
        _ => {
            let r = compile_value_depth(em, reg, scope, expr, depth + 1)?;
            finish_truthy(em, r, targets);
            Ok(())
        }
    }
}

/// Tests an already-computed value register for truthiness as a
/// three-valued condition. `IfNot`'s `p3` flag folds NULL into the
/// false jump, which covers `NullTarget::False` in one instruction; the
/// other setting needs an explicit `IsNull` probe first.
pub(super) fn finish_truthy(em: &mut Emitter, r: i32, targets: CondTargets) {
    match targets.on_null {
        NullTarget::False => {
            finish_bool(em, targets.on_true, targets.on_false, |em, false_label| {
                let addr = em.emit(Instruction::new(Opcode::IfNot, r, 0, 1));
                em.patch_p2(addr, false_label);
            });
        }
        NullTarget::True => {
            let (t_label, t_is_new) = ensure_label(em, targets.on_true);
            let addr = em.emit(Instruction::new(Opcode::IsNull, r, 0, 0));
            em.patch_p2(addr, t_label);
            finish_bool(
                em,
                Target::Jump(t_label),
                targets.on_false,
                |em, false_label| {
                    let addr = em.emit(Instruction::new(Opcode::IfNot, r, 0, 0));
                    em.patch_p2(addr, false_label);
                },
            );
            if t_is_new {
                em.place(t_label);
            }
        }
    }
}

/// Resolves `target` to a real label usable as an immediate jump
/// destination, returning whether that label still needs `em.place`-ing.
pub(crate) fn ensure_label(em: &mut Emitter, target: Target) -> (Label, bool) {
    match target {
        Target::Jump(l) => (l, false),
        Target::Fallthrough => (em.new_label(), true),
    }
}

/// Given a primitive that emits a "jump to `false_label` when the
/// condition is false, fall through when true" instruction, resolves
/// the full `(on_true, on_false)` combination.
pub(super) fn finish_bool(
    em: &mut Emitter,
    true_target: Target,
    false_target: Target,
    emit_false_jump: impl FnOnce(&mut Emitter, Label),
) {
    match (true_target, false_target) {
        (Target::Fallthrough, Target::Jump(f)) => emit_false_jump(em, f),
        (Target::Jump(t), Target::Fallthrough) => {
            let synth = em.new_label();
            emit_false_jump(em, synth);
            em.goto(t);
            em.place(synth);
        }
        (Target::Jump(t), Target::Jump(f)) => {
            emit_false_jump(em, f);
            em.goto(t);
        }
        (Target::Fallthrough, Target::Fallthrough) => {
            let synth = em.new_label();
            emit_false_jump(em, synth);
            em.place(synth);
        }
    }
}

/// Emits the appropriate compare opcode as a "jump to `false_label` on
/// false" primitive, then resolves `true_target`/`false_target` via
/// [`finish_bool`]. `Ne` has no dedicated opcode -- it's `Eq`'s
/// complement, so its false-jump primitive is a plain `Eq` jump.
fn emit_compare_false_jump(
    em: &mut Emitter,
    op: BinOp,
    lhs: i32,
    rhs: i32,
    affinity: crate::vm::row::Affinity,
    targets: CondTargets,
) -> Result<()> {
    let p4 = p4_coll_seq(Collation::Binary, affinity);
    let resolved = match op {
        BinOp::Ne => Some((Opcode::Eq, targets.negate())),
        BinOp::Eq => Some((Opcode::Eq, targets)),
        BinOp::Lt => Some((Opcode::Lt, targets)),
        BinOp::Le => Some((Opcode::Le, targets)),
        BinOp::Gt => Some((Opcode::Gt, targets)),
        BinOp::Ge => Some((Opcode::Ge, targets)),
        _ => None,
    };
    let Some((opcode, targets)) = resolved else {
        return Err(CodegenError::Unsupported {
            reason: "emit_compare_false_jump called with a non-comparison operator".to_string(),
        });
    };
    let (t_label, t_is_new) = ensure_label(em, targets.on_true);
    // A NULL operand makes the compare opcode not jump at all, so it
    // otherwise always lands on false. When the unknown outcome
    // belongs with true instead, probe for it explicitly first.
    if targets.on_null == NullTarget::True {
        let addr = em.emit(Instruction::new(Opcode::IsNull, lhs, 0, 0));
        em.patch_p2(addr, t_label);
        let addr = em.emit(Instruction::new(Opcode::IsNull, rhs, 0, 0));
        em.patch_p2(addr, t_label);
    }
    let addr = em.emit(Instruction::with_p4(opcode, lhs, 0, rhs, p4));
    em.patch_p2(addr, t_label);
    if let Target::Jump(fl) = targets.on_false {
        em.goto(fl);
    }
    if t_is_new {
        em.place(t_label);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::row::TableSchema;
    use crate::types::Literal;
    use crate::vm::row::{execute, Value, Vm};

    fn schema(columns: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".into(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| String::new()).collect(),
            rowid_alias: None,
        }
    }

    /// Compiles `expr` as a `WHERE`-style condition (`null_is_false`)
    /// and returns 1 if it took the true branch, 0 otherwise.
    fn run_cond(expr: &Expr, scope: &Scope) -> i64 {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let true_label = em.new_label();
        let end_label = em.new_label();
        compile_cond(
            &mut em,
            &mut reg,
            scope,
            expr,
            CondTargets::null_is_false(Target::Jump(true_label), Target::Fallthrough),
        )
        .unwrap();
        let dest = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, dest, 0));
        em.goto(end_label);
        em.place(true_label);
        em.emit(Instruction::new(Opcode::Integer, 1, dest, 0));
        em.place(end_label);
        em.emit(Instruction::new(Opcode::ResultRow, dest, 1, 0));
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        let program = em.finish();
        let mut vm = Vm::new();
        let rows = execute(&mut vm, &program).unwrap();
        match rows.into_iter().next().unwrap().into_iter().next().unwrap() {
            Value::Integer(i) => i,
            other => panic!("expected an integer, got {other:?}"),
        }
    }

    fn lit(i: i64) -> Expr {
        Expr::Literal(Literal::Int(i))
    }

    #[test]
    fn eq_true_and_false() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = Expr::BinaryOp(Box::new(lit(5)), BinOp::Eq, Box::new(lit(5)));
        assert_eq!(run_cond(&expr, &scope), 1);
        let expr = Expr::BinaryOp(Box::new(lit(5)), BinOp::Eq, Box::new(lit(6)));
        assert_eq!(run_cond(&expr, &scope), 0);
    }

    #[test]
    fn ne_is_eqs_complement() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = Expr::BinaryOp(Box::new(lit(5)), BinOp::Ne, Box::new(lit(6)));
        assert_eq!(run_cond(&expr, &scope), 1);
        let expr = Expr::BinaryOp(Box::new(lit(5)), BinOp::Ne, Box::new(lit(5)));
        assert_eq!(run_cond(&expr, &scope), 0);
    }

    #[test]
    fn ordering_comparisons() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_cond(
                &Expr::BinaryOp(Box::new(lit(3)), BinOp::Lt, Box::new(lit(5))),
                &scope
            ),
            1
        );
        assert_eq!(
            run_cond(
                &Expr::BinaryOp(Box::new(lit(5)), BinOp::Lt, Box::new(lit(3))),
                &scope
            ),
            0
        );
        assert_eq!(
            run_cond(
                &Expr::BinaryOp(Box::new(lit(5)), BinOp::Ge, Box::new(lit(5))),
                &scope
            ),
            1
        );
    }

    #[test]
    fn and_short_circuits_both_true_and_false() {
        let scope = Scope::single(schema(&[]), 0);
        let t = Expr::BinaryOp(Box::new(lit(1)), BinOp::Eq, Box::new(lit(1)));
        let f = Expr::BinaryOp(Box::new(lit(1)), BinOp::Eq, Box::new(lit(2)));
        let expr = Expr::BinaryOp(Box::new(t.clone()), BinOp::And, Box::new(t.clone()));
        assert_eq!(run_cond(&expr, &scope), 1);
        let expr = Expr::BinaryOp(Box::new(t.clone()), BinOp::And, Box::new(f.clone()));
        assert_eq!(run_cond(&expr, &scope), 0);
    }

    #[test]
    fn or_true_when_either_operand_true() {
        let scope = Scope::single(schema(&[]), 0);
        let t = Expr::BinaryOp(Box::new(lit(1)), BinOp::Eq, Box::new(lit(1)));
        let f = Expr::BinaryOp(Box::new(lit(1)), BinOp::Eq, Box::new(lit(2)));
        let expr = Expr::BinaryOp(Box::new(f.clone()), BinOp::Or, Box::new(t.clone()));
        assert_eq!(run_cond(&expr, &scope), 1);
        let expr = Expr::BinaryOp(Box::new(f.clone()), BinOp::Or, Box::new(f.clone()));
        assert_eq!(run_cond(&expr, &scope), 0);
    }

    #[test]
    fn not_negates_condition() {
        let scope = Scope::single(schema(&[]), 0);
        let t = Expr::BinaryOp(Box::new(lit(1)), BinOp::Eq, Box::new(lit(1)));
        let expr = Expr::Not(Box::new(t));
        assert_eq!(run_cond(&expr, &scope), 0);
    }

    #[test]
    fn is_null_true_and_is_not_null_false_for_a_literal() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = Expr::IsNull {
            expr: Box::new(lit(1)),
            negated: false,
        };
        assert_eq!(run_cond(&expr, &scope), 0);
        let expr = Expr::IsNull {
            expr: Box::new(lit(1)),
            negated: true,
        };
        assert_eq!(run_cond(&expr, &scope), 1);
    }

    #[test]
    fn column_truthiness_in_cond_context() {
        use crate::vm::row::InMemoryCursor;

        let scope = Scope::single(schema(&["a"]), 0);
        // A bare column used as a boolean condition tests truthiness of
        // its value, same as `WHERE some_int_column`.
        let expr = Expr::Column("a".into());

        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let positioned = em.new_label();
        let addr = em.emit(Instruction::new(Opcode::Rewind, 0, 0, 0));
        em.patch_p2(addr, positioned);
        em.place(positioned);
        let true_label = em.new_label();
        let end_label = em.new_label();
        compile_cond(
            &mut em,
            &mut reg,
            &scope,
            &expr,
            CondTargets::null_is_false(Target::Jump(true_label), Target::Fallthrough),
        )
        .unwrap();
        let dest = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, dest, 0));
        em.goto(end_label);
        em.place(true_label);
        em.emit(Instruction::new(Opcode::Integer, 1, dest, 0));
        em.place(end_label);
        em.emit(Instruction::new(Opcode::ResultRow, dest, 1, 0));
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        let program = em.finish();

        let mut vm = Vm::new();
        vm.open_cursor(
            0,
            Box::new(InMemoryCursor::new(vec![vec![Value::Integer(0)]])),
        )
        .unwrap();
        let rows = execute(&mut vm, &program).unwrap();
        assert_eq!(rows[0][0], Value::Integer(0));
    }

    #[test]
    fn depth_bound_rejects_deeply_nested_expressions() {
        let scope = Scope::single(schema(&[]), 0);
        let mut expr = lit(1);
        for _ in 0..(MAX_EXPR_DEPTH + 10) {
            expr = Expr::Not(Box::new(expr));
        }
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let result = compile_cond(
            &mut em,
            &mut reg,
            &scope,
            &expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Fallthrough),
        );
        assert_eq!(result, Err(CodegenError::TooDeep));
    }
}
