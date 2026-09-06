//! Jump-mode condition compilation -- see `super`'s module doc.

use super::value::{compile_value_depth, expr_affinity};
use super::{
    p4_coll_seq, CodegenError, CondTargets, Emitter, Label, NullTarget, RegAlloc, Result, Scope,
    Target, MAX_EXPR_DEPTH,
};
use crate::parser::ast::{BinaryOp, Expr, ExprKind, UnaryOp};
use crate::vm::row::{comparison_affinity, Collation, Instruction, Opcode};

/// A rough, static cost class for an expression, used only to order
/// `AND`/`OR` operands cheapest-first so short-circuit evaluation skips
/// the pricier side more often -- never to change what a query returns.
/// Lower is cheaper.
fn cost_class(expr: &Expr) -> u8 {
    match &expr.kind {
        ExprKind::Literal(_) | ExprKind::Column { .. } | ExprKind::Param(_) => 0,
        // Parentheses cost nothing of their own.
        ExprKind::Paren(inner) => cost_class(inner),
        ExprKind::Unary { expr: inner, .. } => cost_class(inner).max(1),
        ExprKind::IsNull { expr: inner, .. } => cost_class(inner).max(1),
        ExprKind::Is { lhs, rhs, .. } => cost_class(lhs).max(cost_class(rhs)).max(1),
        ExprKind::Binary { op, lhs, rhs } => {
            let base = match op {
                BinaryOp::And | BinaryOp::Or => 0,
                _ => 1,
            };
            cost_class(lhs).max(cost_class(rhs)).max(base)
        }
        ExprKind::Between {
            expr: inner,
            lo,
            hi,
            ..
        } => cost_class(inner)
            .max(cost_class(lo))
            .max(cost_class(hi))
            .max(1),
        ExprKind::In {
            expr: inner, list, ..
        } => list
            .iter()
            .map(cost_class)
            .fold(cost_class(inner), u8::max)
            .max(1),
        ExprKind::Like {
            expr: inner,
            pattern,
            ..
        } => cost_class(inner).max(cost_class(pattern)).max(2),
        ExprKind::Cast { expr: inner, .. } | ExprKind::Collate { expr: inner, .. } => {
            cost_class(inner).max(1)
        }
        ExprKind::Case { .. } | ExprKind::FunctionCall { .. } => 2,
        // A subquery scan is the priciest operand this compiler has.
        ExprKind::InSubquery { .. }
        | ExprKind::InSubqueryMulti { .. }
        | ExprKind::Exists { .. }
        | ExprKind::Subquery(_) => 3,
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
    match &expr.kind {
        // Parentheses are pure grouping; the tree shape already records
        // what they meant.
        ExprKind::Paren(inner) => {
            compile_cond_depth(em, reg, scope, inner, targets, depth.saturating_add(1))
        }

        // Swapping the targets is right, but only once `on_null` comes
        // along for the ride -- flipping it keeps the unknown outcome
        // on the same address across the swap.
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr: inner,
        } => compile_cond_depth(
            em,
            reg,
            scope,
            inner,
            targets.negate(),
            depth.saturating_add(1),
        ),

        ExprKind::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
        } => {
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
                depth.saturating_add(1),
            )?;
            compile_cond_depth(em, reg, scope, second, operand, depth.saturating_add(1))?;
            if is_new {
                em.place(false_label);
            }
            Ok(())
        }

        ExprKind::Binary {
            op: BinaryOp::Or,
            lhs,
            rhs,
        } => {
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
                depth.saturating_add(1),
            )?;
            compile_cond_depth(em, reg, scope, second, operand, depth.saturating_add(1))?;
            if is_new {
                em.place(true_label);
            }
            Ok(())
        }

        ExprKind::Binary { op, lhs, rhs }
            if matches!(
                op,
                BinaryOp::Eq
                    | BinaryOp::Ne
                    | BinaryOp::Lt
                    | BinaryOp::Le
                    | BinaryOp::Gt
                    | BinaryOp::Ge
            ) =>
        {
            let affinity =
                comparison_affinity(expr_affinity(scope, lhs), expr_affinity(scope, rhs));
            let collation = resolve_collation(lhs)?
                .or(resolve_collation(rhs)?)
                .unwrap_or(Collation::Binary);
            let l = compile_value_depth(em, reg, scope, lhs, depth.saturating_add(1))?;
            let r = compile_value_depth(em, reg, scope, rhs, depth.saturating_add(1))?;
            emit_compare_false_jump(em, *op, l, r, affinity, collation, targets)
        }

        ExprKind::IsNull {
            expr: inner,
            negated,
        } => {
            let r = compile_value_depth(em, reg, scope, inner, depth.saturating_add(1))?;
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

        ExprKind::InSubquery {
            expr: lhs,
            subquery,
            negated,
        } => super::subquery::compile_in_subquery(em, reg, scope, lhs, subquery, *negated, targets),

        ExprKind::Exists { subquery, negated } => {
            super::subquery::compile_exists(em, reg, scope, subquery, *negated, targets)
        }

        // `a IS [NOT] b` is NULL-safe equality: true if both sides are
        // NULL, true if both are non-NULL and equal, false otherwise --
        // never NULL itself (`value::is_definite` knows this). Desugared
        // to `(a IS NULL AND b IS NULL) OR (a IS NOT NULL AND b IS NOT
        // NULL AND a = b)` and delegated back through this same
        // function: `Eq`'s own three-valued codegen never actually sees
        // an unknown answer here, since both guarding NULL-checks have
        // already ruled that out by construction. `negated` (`IS NOT`)
        // flips the outer targets, the same trick `NOT` uses above.
        // `a IS b`: true when both NULL, or both non-NULL and equal --
        // unlike `=`, never propagates NULL to "unknown". No single
        // opcode expresses this; compute it into a 0/1 register first,
        // then test truthiness like any other value-mode boolean
        // (`LIKE`/`GLOB` take the same shape). `on_null` is deliberately
        // ignored here (as in `IsNull` above): this is one of the only
        // two conditions in SQL that are always definitely true or
        // definitely false, so swapping the targets for `negated` is
        // sound without flipping it. Ported from sqlite-rs's
        // `codegen/expr/cond.rs` `Is` arm, unchanged -- it deliberately
        // uses a bare `Eq` (no COLLATE/affinity p4), which this matches.
        ExprKind::Is { lhs, rhs, negated } => {
            let (t, f) = if *negated {
                (targets.on_false, targets.on_true)
            } else {
                (targets.on_true, targets.on_false)
            };
            let l = compile_value_depth(em, reg, scope, lhs, depth.saturating_add(1))?;
            let r = compile_value_depth(em, reg, scope, rhs, depth.saturating_add(1))?;
            let result = reg.alloc();
            let both_null = em.new_label();
            let done = em.new_label();
            let addr = em.emit(Instruction::new(Opcode::IsNull, l, 0, 0));
            em.patch_p2(addr, both_null);

            let eq_true = em.new_label();
            let addr = em.emit(Instruction::new(Opcode::Eq, l, 0, r));
            em.patch_p2(addr, eq_true);
            em.emit(Instruction::new(Opcode::Integer, 0, result, 0));
            em.goto(done);
            em.place(eq_true);
            em.emit(Instruction::new(Opcode::Integer, 1, result, 0));
            em.goto(done);

            em.place(both_null);
            let r_null = em.new_label();
            let addr = em.emit(Instruction::new(Opcode::IsNull, r, 0, 0));
            em.patch_p2(addr, r_null);
            em.emit(Instruction::new(Opcode::Integer, 0, result, 0));
            em.goto(done);
            em.place(r_null);
            em.emit(Instruction::new(Opcode::Integer, 1, result, 0));

            em.place(done);
            finish_bool(em, t, f, |em, false_label| {
                let addr = em.emit(Instruction::new(Opcode::IfNot, result, 0, 1));
                em.patch_p2(addr, false_label);
            });
            Ok(())
        }

        // `x BETWEEN lo AND hi` is `x >= lo AND x <= hi`, and `x NOT
        // BETWEEN lo AND hi` is `x < lo OR x > hi` -- NOT the same
        // shape with true/false swapped. Swapping targets would make a
        // NULL `x` (where neither comparison jumps at all, per
        // `emit_compare_false_jump`'s three-valued contract) come out
        // *true* for `NOT BETWEEN`, when the honest answer is
        // "unknown", which `WHERE` excludes just like false. Ported
        // from sqlite-rs's `codegen/expr/cond.rs` `Between` arm.
        ExprKind::Between {
            expr: inner,
            lo,
            hi,
            negated,
        } => {
            let cmp = |op, rhs: &Expr| Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: inner.clone(),
                    rhs: Box::new(rhs.clone()),
                },
                span: expr.span,
            };
            if *negated {
                let lt_lo = cmp(BinaryOp::Lt, lo);
                let gt_hi = cmp(BinaryOp::Gt, hi);
                let (t_label, t_is_new) = ensure_label(em, targets.on_true);
                let arm = targets.with_true(Target::Jump(t_label));
                compile_cond_depth(
                    em,
                    reg,
                    scope,
                    &lt_lo,
                    arm.with_false(Target::Fallthrough),
                    depth.saturating_add(1),
                )?;
                compile_cond_depth(em, reg, scope, &gt_hi, arm, depth.saturating_add(1))?;
                if t_is_new {
                    em.place(t_label);
                }
            } else {
                let ge_lo = cmp(BinaryOp::Ge, lo);
                let le_hi = cmp(BinaryOp::Le, hi);
                let (f_label, f_is_new) = ensure_label(em, targets.on_false);
                let arm = targets.with_false(Target::Jump(f_label));
                compile_cond_depth(
                    em,
                    reg,
                    scope,
                    &ge_lo,
                    arm.with_true(Target::Fallthrough),
                    depth.saturating_add(1),
                )?;
                compile_cond_depth(em, reg, scope, &le_hi, arm, depth.saturating_add(1))?;
                if f_is_new {
                    em.place(f_label);
                }
            }
            Ok(())
        }

        // `IN`'s three outcomes -- a definite match, a definite
        // non-match, or "unknown" (no match found, but `inner` or some
        // list item was NULL along the way) -- don't collapse to a
        // single true/false jump the way other comparisons do: `NOT
        // IN`'s definite-non-match and unknown outcomes diverge (`NOT
        // FALSE` = true, `NOT NULL` = still NULL), so a per-item
        // comparison can't just swap targets. `saw_null` is a small
        // exception to this module's "never an intermediate boolean
        // register" rule, needed to remember that exception past the
        // loop that discovers it. Ported from sqlite-rs's
        // `codegen/expr/cond.rs` `In` arm, unchanged.
        ExprKind::In {
            expr: inner,
            list,
            negated,
        } => {
            if list.is_empty() {
                // `x IN ()` is always false, even for a NULL `x` -- an
                // empty list leaves nothing to be uncertain against.
                // Not reachable through this grammar (it requires at
                // least one element); kept for parity with the oracle.
                let false_target = if *negated {
                    targets.on_true
                } else {
                    targets.on_false
                };
                if let Target::Jump(label) = false_target {
                    em.goto(label);
                }
                return Ok(());
            }

            let l = compile_value_depth(em, reg, scope, inner, depth.saturating_add(1))?;
            let saw_null = reg.alloc();
            em.emit(Instruction::new(Opcode::Integer, 0, saw_null, 0));

            let (true_label, true_is_new) = ensure_label(em, targets.on_true);
            let (false_label, false_is_new) = ensure_label(em, targets.on_false);
            let (found_label, unmatched_label) = if *negated {
                (false_label, true_label)
            } else {
                (true_label, false_label)
            };
            let null_label = match targets.on_null {
                NullTarget::True => true_label,
                NullTarget::False => false_label,
            };

            let inner_null_addr = em.emit(Instruction::new(Opcode::IsNull, l, 0, 0));
            em.patch_p2(inner_null_addr, null_label);

            for item in list {
                let collation = resolve_collation(inner)?.or(resolve_collation(item)?);
                let affinity =
                    comparison_affinity(expr_affinity(scope, inner), expr_affinity(scope, item));
                let p4 = p4_coll_seq(collation.unwrap_or(Collation::Binary), affinity);
                let r = compile_value_depth(em, reg, scope, item, depth.saturating_add(1))?;

                let item_null_label = em.new_label();
                let skip_label = em.new_label();
                let addr = em.emit(Instruction::new(Opcode::IsNull, r, 0, 0));
                em.patch_p2(addr, item_null_label);
                let addr = em.emit(Instruction::with_p4(Opcode::Eq, l, 0, r, p4));
                em.patch_p2(addr, found_label);
                em.goto(skip_label);

                em.place(item_null_label);
                em.emit(Instruction::new(Opcode::Integer, 1, saw_null, 0));
                em.place(skip_label);
            }

            // Exhausted the list without a match: route to
            // `unmatched_label` only if every comparison was a clean
            // non-match (`saw_null` still 0); otherwise at least one
            // comparison was against NULL, so the honest answer is
            // "unknown", which goes wherever `on_null` says.
            let addr = em.emit(Instruction::new(Opcode::IfNot, saw_null, 0, 0));
            em.patch_p2(addr, unmatched_label);
            em.goto(null_label);

            if false_is_new {
                em.place(false_label);
            }
            if true_is_new {
                em.place(true_label);
            }
            Ok(())
        }

        ExprKind::InSubqueryMulti { .. } => Err(CodegenError::Unsupported {
            reason: "a multi-column IN (SELECT ...) is not supported by codegen::row yet"
                .to_string(),
        }),

        // Any other expression used in boolean context (a bare column,
        // arithmetic, etc.): evaluate to a value and test truthiness.
        _ => {
            let r = compile_value_depth(em, reg, scope, expr, depth.saturating_add(1))?;
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

/// The [`Collation`] an explicit `expr COLLATE name` names, if either
/// comparison operand carries one -- `None` means "use the schema
/// affinity default" (`Collation::Binary`, unchanged from before this
/// existed). SQLite resolves a bare column's *declared* `COLLATE`
/// here too; this crate's `TableSchema` carries no per-column
/// collation yet, so only an explicit operand-level `COLLATE` is
/// honored -- silently defaulting to `Binary` for one would be a
/// wrong-answer bug, not a limitation, so an unrecognized name is
/// rejected rather than ignored.
fn resolve_collation(expr: &Expr) -> Result<Option<Collation>> {
    match &expr.kind {
        ExprKind::Paren(inner) => resolve_collation(inner),
        ExprKind::Collate { collation, .. } => match collation.to_ascii_uppercase().as_str() {
            "BINARY" => Ok(Some(Collation::Binary)),
            "NOCASE" => Ok(Some(Collation::NoCase)),
            "RTRIM" => Ok(Some(Collation::RTrim)),
            other => Err(CodegenError::Unsupported {
                reason: format!("unknown or unsupported collation {other:?}"),
            }),
        },
        _ => Ok(None),
    }
}

/// Emits the appropriate compare opcode as a "jump to `false_label` on
/// false" primitive, then resolves `true_target`/`false_target` via
/// [`finish_bool`]. `Ne` has no dedicated opcode -- it's `Eq`'s
/// complement, so its false-jump primitive is a plain `Eq` jump.
#[allow(clippy::too_many_arguments)]
fn emit_compare_false_jump(
    em: &mut Emitter,
    op: BinaryOp,
    lhs: i32,
    rhs: i32,
    affinity: crate::vm::row::Affinity,
    collation: Collation,
    targets: CondTargets,
) -> Result<()> {
    let p4 = p4_coll_seq(collation, affinity);
    let resolved = match op {
        BinaryOp::Ne => Some((Opcode::Eq, targets.negate())),
        BinaryOp::Eq => Some((Opcode::Eq, targets)),
        BinaryOp::Lt => Some((Opcode::Lt, targets)),
        BinaryOp::Le => Some((Opcode::Le, targets)),
        BinaryOp::Gt => Some((Opcode::Gt, targets)),
        BinaryOp::Ge => Some((Opcode::Ge, targets)),
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use crate::codegen::row::testutil::expr;
    use crate::codegen::row::TableSchema;
    use crate::vm::row::{execute, Value, Vm};

    fn schema(columns: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".into(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| String::new()).collect(),
            rowid_alias: None,
            root_page: 0,
            indexes: vec![],
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

    /// Runs `sql`'s expression (parsed via [`expr`]) as a condition over
    /// a scope with no columns.
    fn run(sql: &str) -> i64 {
        run_cond(&expr(sql), &Scope::single(schema(&[]), 0))
    }

    #[test]
    fn eq_true_and_false() {
        assert_eq!(run("5 = 5"), 1);
        assert_eq!(run("5 = 6"), 0);
    }

    #[test]
    fn ne_is_eqs_complement() {
        assert_eq!(run("5 <> 6"), 1);
        assert_eq!(run("5 <> 5"), 0);
    }

    #[test]
    fn ordering_comparisons() {
        assert_eq!(run("3 < 5"), 1);
        assert_eq!(run("5 < 3"), 0);
        assert_eq!(run("5 >= 5"), 1);
    }

    #[test]
    fn and_short_circuits_both_true_and_false() {
        assert_eq!(run("1 = 1 AND 1 = 1"), 1);
        assert_eq!(run("1 = 1 AND 1 = 2"), 0);
    }

    #[test]
    fn or_true_when_either_operand_true() {
        assert_eq!(run("1 = 2 OR 1 = 1"), 1);
        assert_eq!(run("1 = 2 OR 1 = 2"), 0);
    }

    #[test]
    fn not_negates_condition() {
        assert_eq!(run("NOT (1 = 1)"), 0);
    }

    #[test]
    fn is_null_true_and_is_not_null_false_for_a_literal() {
        // `ISNULL`/`NOTNULL` (no space) is SQLite's postfix spelling
        // and parses directly to `ExprKind::IsNull`. The two-word `IS
        // NULL`/`IS NOT NULL` instead parses through the general `IS`
        // operator as `ExprKind::Is { rhs: NULL literal, .. }` -- which
        // is semantically equivalent but not yet compiled (#150), so it
        // is not what this test exercises.
        assert_eq!(run("1 ISNULL"), 0);
        assert_eq!(run("1 NOTNULL"), 1);
    }

    /// Parenthesization is preserved in the AST but must not change
    /// what the condition evaluates to.
    #[test]
    fn paren_is_transparent_to_cond_codegen() {
        assert_eq!(run("(1 = 1) AND (2 = 2)"), 1);
        assert_eq!(run("NOT (1 = 2)"), 1);
    }

    #[test]
    fn column_truthiness_in_cond_context() {
        use crate::vm::row::InMemoryCursor;

        let scope = Scope::single(schema(&["a"]), 0);
        // A bare column used as a boolean condition tests truthiness of
        // its value, same as `WHERE some_int_column`.
        let expr = expr("a");

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
        let mut expr = expr("1");
        for _ in 0..(MAX_EXPR_DEPTH + 10) {
            expr = Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(expr),
                },
                span: crate::parser::Span::UNKNOWN,
            };
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

    /// `IS`/`BETWEEN`/`IN (list)`/`LIKE` are condition forms the AST can
    /// express that `codegen::row` cannot compile yet (#150). The
    /// explicit `Unsupported` arms are load-bearing: without them these
    /// would fall through to the `_` arm and be compiled as a truthy
    /// *value*, which is simply wrong for a condition.
    #[test]
    fn is_and_is_not_are_null_safe_equality() {
        assert_eq!(run("1 IS 1"), 1);
        assert_eq!(run("1 IS 2"), 0);
        assert_eq!(run("NULL IS NULL"), 1);
        assert_eq!(run("1 IS NULL"), 0);
        assert_eq!(run("NULL IS 1"), 0);
        assert_eq!(run("1 IS NOT 2"), 1);
        assert_eq!(run("NULL IS NOT NULL"), 0);
    }

    #[test]
    fn between_is_ge_lo_and_le_hi() {
        assert_eq!(run("3 BETWEEN 1 AND 5"), 1);
        assert_eq!(run("1 BETWEEN 1 AND 5"), 1);
        assert_eq!(run("5 BETWEEN 1 AND 5"), 1);
        assert_eq!(run("0 BETWEEN 1 AND 5"), 0);
        assert_eq!(run("6 BETWEEN 1 AND 5"), 0);
        assert_eq!(run("3 NOT BETWEEN 1 AND 5"), 0);
        assert_eq!(run("0 NOT BETWEEN 1 AND 5"), 1);
    }

    /// A `NULL` bound makes `BETWEEN` unknown unless the other bound
    /// already settles it -- exactly `AND`'s own short-circuit, since
    /// `BETWEEN` desugars straight into one.
    #[test]
    fn between_with_a_null_bound_is_null_unless_the_other_bound_already_decides() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_cond3(&expr("0 BETWEEN NULL AND 5"), &scope),
            None,
            "0 >= NULL is unknown, and 0 <= 5 doesn't rule BETWEEN out"
        );
        assert_eq!(
            run_cond3(&expr("6 BETWEEN NULL AND 5"), &scope),
            Some(false),
            "6 <= 5 is definitely false regardless of the unknown lower bound"
        );
    }

    #[test]
    fn in_list_matches_membership() {
        assert_eq!(run("2 IN (1, 2, 3)"), 1);
        assert_eq!(run("4 IN (1, 2, 3)"), 0);
        assert_eq!(run("2 NOT IN (1, 2, 3)"), 0);
        assert_eq!(run("4 NOT IN (1, 2, 3)"), 1);
    }

    /// A `NULL` in the list can't rule a non-match out -- `IN` desugars
    /// to `OR`, and `false OR unknown` is unknown, not false.
    #[test]
    fn in_list_with_an_unmatched_null_is_null() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(run_cond3(&expr("4 IN (1, NULL, 3)"), &scope), None);
        assert_eq!(
            run_cond3(&expr("2 IN (1, NULL, 2)"), &scope),
            Some(true),
            "a real match short-circuits the same way OR does"
        );
    }

    #[test]
    fn like_and_glob_match_and_propagate_null() {
        assert_eq!(run("'abc' LIKE 'a%'"), 1);
        assert_eq!(run("'abc' LIKE 'x%'"), 0);
        assert_eq!(run("'abc' NOT LIKE 'x%'"), 1);
        assert_eq!(run("'axc' GLOB 'a?c'"), 1);
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(run_cond3(&expr("'x' LIKE NULL"), &scope), None);
    }

    #[test]
    fn an_explicit_collate_is_honored_in_a_comparison() {
        // BINARY (the default) is case-sensitive; NOCASE folds ASCII
        // case before comparing.
        assert_eq!(run("'ABC' = 'abc'"), 0);
        assert_eq!(run("'ABC' = 'abc' COLLATE NOCASE"), 1);
        assert_eq!(run("'ABC' COLLATE NOCASE = 'abc'"), 1);
    }

    #[test]
    fn an_unknown_collation_name_is_unsupported() {
        let scope = Scope::single(schema(&[]), 0);
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        assert!(matches!(
            compile_cond(
                &mut em,
                &mut reg,
                &scope,
                &expr("'a' = 'a' COLLATE FRENCH"),
                CondTargets::null_is_false(Target::Fallthrough, Target::Fallthrough),
            ),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn multi_column_in_subquery_is_unsupported() {
        let scope = Scope::single(schema(&["a", "b"]), 0);
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        assert!(matches!(
            compile_cond(
                &mut em,
                &mut reg,
                &scope,
                &expr("(a, b) IN (SELECT x, y FROM u)"),
                CondTargets::null_is_false(Target::Fallthrough, Target::Fallthrough),
            ),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    /// Runs `expr` as a *value* (via `value::compile_value`, which
    /// materializes a condition three-valued by compiling it twice --
    /// see `compile_bool_to_value`) and returns its real answer, `None`
    /// for SQL's unknown -- unlike [`run`], which folds unknown into
    /// false the way `WHERE` does.
    fn run_cond3(expr: &Expr, scope: &Scope) -> Option<bool> {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let dest =
            crate::codegen::row::value::compile_value(&mut em, &mut reg, scope, expr).unwrap();
        em.emit(Instruction::new(Opcode::ResultRow, dest, 1, 0));
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        let program = em.finish();
        let mut vm = Vm::new();
        let rows = execute(&mut vm, &program).unwrap();
        match rows.into_iter().next().unwrap().into_iter().next().unwrap() {
            Value::Integer(1) => Some(true),
            Value::Integer(0) => Some(false),
            Value::Null => None,
            other => panic!("expected an integer or NULL, got {other:?}"),
        }
    }
}
