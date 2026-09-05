//! Value-mode expression compilation -- see `super`'s module doc.

use super::cond::compile_cond;
use super::{CodegenError, CondTargets, Emitter, RegAlloc, Result, Scope, Target, MAX_EXPR_DEPTH};
use crate::expr::{BinOp, Expr};
use crate::types::Literal;
use crate::vm::row::{affinity_of, Affinity, Instruction, Opcode, P4};

/// Reads column `idx` of the row at `cursor` into `dest`, emitting
/// `Rowid` rather than `Column` for a rowid-alias column. A table's
/// `INTEGER PRIMARY KEY` column is stored as a NULL placeholder in
/// every record -- reading it with `Column` yields NULL.
pub(crate) fn emit_column_read(
    em: &mut Emitter,
    schema: &super::TableSchema,
    cursor: i32,
    idx: usize,
    dest: i32,
) -> Result<()> {
    if schema.rowid_alias == Some(idx) {
        em.emit(Instruction::new(Opcode::Rowid, cursor, dest, 0));
        return Ok(());
    }
    em.emit(Instruction::new(
        Opcode::Column,
        cursor,
        i32::try_from(idx).map_err(|_| CodegenError::Unsupported {
            reason: format!("column index {idx} does not fit in a p2 operand"),
        })?,
        dest,
    ));
    // A REAL-affinity column's on-disk value may use the integer-0/1
    // serial-type optimization; `RealAffinity` undoes that on read so
    // `SELECT r FROM t` for a REAL column holding `0.0` answers `0.0`,
    // not `0`.
    if schema
        .column_types
        .get(idx)
        .is_some_and(|t| affinity_of(t) == Affinity::Real)
    {
        em.emit(Instruction::new(Opcode::RealAffinity, dest, 0, 0));
    }
    Ok(())
}

/// An expression's own affinity: a bare column carries its
/// declared-type affinity; every other expression (literals,
/// arithmetic) has none of its own.
pub(crate) fn expr_affinity(scope: &Scope, expr: &Expr) -> Option<Affinity> {
    match expr {
        Expr::Column(name) => {
            let (_, idx) = scope.resolve(name).ok()?;
            let declared = scope.schema.column_types.get(idx)?;
            Some(affinity_of(declared))
        }
        _ => None,
    }
}

/// Compiles `expr` into a fresh register holding its value (value
/// mode) -- used for result columns, function arguments, and as the
/// operand feed for jump-mode comparisons.
pub fn compile_value(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    expr: &Expr,
) -> Result<i32> {
    compile_value_depth(em, reg, scope, expr, 0)
}

pub(crate) fn compile_value_depth(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    expr: &Expr,
    depth: usize,
) -> Result<i32> {
    if depth > MAX_EXPR_DEPTH {
        return Err(CodegenError::TooDeep);
    }
    match expr {
        Expr::Literal(lit) => {
            let r = reg.alloc();
            match lit {
                Literal::Int(i) => match i32::try_from(*i) {
                    Ok(p1) => {
                        em.emit(Instruction::new(Opcode::Integer, p1, r, 0));
                    }
                    Err(_) => {
                        em.emit(Instruction::with_p4(Opcode::Int64, 0, r, 0, P4::Int(*i)));
                    }
                },
                Literal::Float(f) => {
                    em.emit(Instruction::with_p4(Opcode::Real, 0, r, 0, P4::Real(*f)));
                }
                Literal::Str(s) => {
                    em.emit(Instruction::with_p4(
                        Opcode::String8,
                        0,
                        r,
                        0,
                        P4::Str(s.clone()),
                    ));
                }
            }
            Ok(r)
        }

        Expr::Column(name) => {
            let (cursor, idx) = scope.resolve(name)?;
            let r = reg.alloc();
            emit_column_read(em, &scope.schema, cursor, idx, r)?;
            Ok(r)
        }

        Expr::Neg(inner) => {
            let r = compile_value_depth(em, reg, scope, inner, depth + 1)?;
            let zero = reg.alloc();
            em.emit(Instruction::new(Opcode::Integer, 0, zero, 0));
            let dest = reg.alloc();
            // `r[p3] = r[p2] - r[p1]` -> 0 - r = -r via p1=r, p2=zero.
            em.emit(Instruction::new(Opcode::Subtract, r, zero, dest));
            Ok(dest)
        }

        Expr::Not(inner) => {
            let r = compile_value_depth(em, reg, scope, inner, depth + 1)?;
            let dest = reg.alloc();
            em.emit(Instruction::new(Opcode::Not, r, dest, 0));
            Ok(dest)
        }

        Expr::BinaryOp(lhs, op, rhs)
            if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) =>
        {
            let l = compile_value_depth(em, reg, scope, lhs, depth + 1)?;
            let r = compile_value_depth(em, reg, scope, rhs, depth + 1)?;
            let dest = reg.alloc();
            let opcode = match op {
                BinOp::Add => Opcode::Add,
                BinOp::Sub => Opcode::Subtract,
                BinOp::Mul => Opcode::Multiply,
                BinOp::Div => Opcode::Divide,
                _ => unreachable!("guarded by the outer match's matches! filter"),
            };
            // Subtract/Divide read as `r[p3] = r[p2] <op> r[p1]`
            // (sqlite-rs's own operand order) -- pass (rhs=p1, lhs=p2)
            // so `lhs <op> rhs` is what's computed.
            match opcode {
                Opcode::Subtract | Opcode::Divide => {
                    em.emit(Instruction::new(opcode, r, l, dest));
                }
                _ => {
                    em.emit(Instruction::new(opcode, l, r, dest));
                }
            }
            Ok(dest)
        }

        Expr::BinaryOp(lhs, BinOp::Concat, rhs) => {
            let l = compile_value_depth(em, reg, scope, lhs, depth + 1)?;
            let r = compile_value_depth(em, reg, scope, rhs, depth + 1)?;
            let dest = reg.alloc();
            // `r[p3] = r[p2] || r[p1]` -- pass (rhs=p1, lhs=p2) so
            // `lhs || rhs` is what's computed.
            em.emit(Instruction::new(Opcode::Concat, r, l, dest));
            Ok(dest)
        }

        // Comparisons and the logical connectives are conditions used
        // in a value context: they answer true/false/unknown, which
        // `compile_bool_to_value` materializes three-valued.
        Expr::BinaryOp(
            _,
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or,
            _,
        ) => compile_bool_to_value(em, reg, scope, expr, depth),

        Expr::IsNull { .. } => compile_bool_to_value(em, reg, scope, expr, depth),

        Expr::InSubquery { .. } => Err(CodegenError::Unsupported {
            reason: "InSubquery codegen is deferred to #95 (subquery materialization)".to_string(),
        }),

        // Unreachable: the three `BinaryOp` guards above jointly cover
        // every `BinOp` variant. Kept as a defensive fallback rather
        // than a `match`-level `unreachable!()` so a future `BinOp`
        // addition fails soft (a codegen error) instead of panicking
        // mid-query.
        Expr::BinaryOp(..) => Err(CodegenError::Unsupported {
            reason: "binary operator not covered by any compile_value arm".to_string(),
        }),
    }
}

/// Whether a condition's outcome is always definitely true or
/// definitely false -- never SQL's unknown. `IS NULL`/`IS NOT NULL` is
/// the only such condition db-core's `Expr` has today.
fn is_definite(expr: &Expr) -> bool {
    matches!(expr, Expr::IsNull { .. })
}

/// Materializes a condition's answer into a register. A condition has
/// three possible answers and jump-mode code only has two
/// destinations, so a genuinely three-valued expression is compiled
/// twice: once asking "is it definitely true?" and once asking "is it
/// definitely false?" (the same condition with `NullTarget::True`, so
/// unknown separates from false instead of joining it). Anything that
/// answers neither is unknown, and lands on the `Null` opcode.
fn compile_bool_to_value(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    expr: &Expr,
    depth: usize,
) -> Result<i32> {
    let dest = reg.alloc();
    let true_label = em.new_label();
    let end_label = em.new_label();

    if is_definite(expr) {
        super::cond::compile_cond_depth(
            em,
            reg,
            scope,
            expr,
            CondTargets::null_is_false(Target::Jump(true_label), Target::Fallthrough),
            depth + 1,
        )?;
        em.emit(Instruction::new(Opcode::Integer, 0, dest, 0));
        em.goto(end_label);
        em.place(true_label);
        em.emit(Instruction::new(Opcode::Integer, 1, dest, 0));
        em.place(end_label);
        return Ok(dest);
    }

    let null_label = em.new_label();
    let false_label = em.new_label();
    // Pass 1: definitely true? Unknown joins false here, so reaching
    // the fallthrough means "false or unknown".
    compile_cond(
        em,
        reg,
        scope,
        expr,
        CondTargets::null_is_false(Target::Jump(true_label), Target::Fallthrough),
    )?;
    // Pass 2: which of the two was it? `NullTarget::True` sends
    // unknown to the true side, which pass 1 already ruled out, so
    // that side can only be reached by an unknown answer.
    compile_cond(
        em,
        reg,
        scope,
        expr,
        CondTargets::null_is_true(Target::Jump(null_label), Target::Jump(false_label)),
    )?;

    em.place(false_label);
    em.emit(Instruction::new(Opcode::Integer, 0, dest, 0));
    em.goto(end_label);
    em.place(null_label);
    em.emit(Instruction::new(Opcode::Null, 0, dest, 0));
    em.goto(end_label);
    em.place(true_label);
    em.emit(Instruction::new(Opcode::Integer, 1, dest, 0));
    em.place(end_label);
    Ok(dest)
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

    fn run_value(expr: &Expr, scope: &Scope) -> Value {
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let dest = compile_value(&mut em, &mut reg, scope, expr).unwrap();
        em.emit(Instruction::new(Opcode::ResultRow, dest, 1, 0));
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        let program = em.finish();
        let mut vm = Vm::new();
        let rows = execute(&mut vm, &program).unwrap();
        rows.into_iter().next().unwrap().into_iter().next().unwrap()
    }

    #[test]
    fn literal_int_compiles_to_integer_or_int64() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_value(&Expr::Literal(Literal::Int(5)), &scope),
            Value::Integer(5)
        );
        let big = i64::from(i32::MAX) + 1;
        assert_eq!(
            run_value(&Expr::Literal(Literal::Int(big)), &scope),
            Value::Integer(big)
        );
    }

    #[test]
    fn literal_float_and_str_roundtrip() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_value(&Expr::Literal(Literal::Float(1.5)), &scope),
            Value::Real(1.5)
        );
        assert_eq!(
            run_value(&Expr::Literal(Literal::Str("hi".into())), &scope),
            Value::Text("hi".into())
        );
    }

    #[test]
    fn arithmetic_operand_order_matches_sql() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = Expr::BinaryOp(
            Box::new(Expr::Literal(Literal::Int(10))),
            BinOp::Sub,
            Box::new(Expr::Literal(Literal::Int(3))),
        );
        assert_eq!(run_value(&expr, &scope), Value::Integer(7));

        let expr = Expr::BinaryOp(
            Box::new(Expr::Literal(Literal::Int(10))),
            BinOp::Div,
            Box::new(Expr::Literal(Literal::Int(4))),
        );
        assert_eq!(run_value(&expr, &scope), Value::Integer(2));
    }

    #[test]
    fn concat_operand_order_matches_sql() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = Expr::BinaryOp(
            Box::new(Expr::Literal(Literal::Str("a".into()))),
            BinOp::Concat,
            Box::new(Expr::Literal(Literal::Str("b".into()))),
        );
        assert_eq!(run_value(&expr, &scope), Value::Text("ab".into()));
    }

    #[test]
    fn neg_negates_value() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = Expr::Neg(Box::new(Expr::Literal(Literal::Int(5))));
        assert_eq!(run_value(&expr, &scope), Value::Integer(-5));
    }

    #[test]
    fn comparison_materializes_three_valued() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = Expr::BinaryOp(
            Box::new(Expr::Literal(Literal::Int(5))),
            BinOp::Eq,
            Box::new(Expr::Literal(Literal::Int(5))),
        );
        assert_eq!(run_value(&expr, &scope), Value::Integer(1));

        let expr = Expr::BinaryOp(
            Box::new(Expr::Literal(Literal::Int(5))),
            BinOp::Eq,
            Box::new(Expr::Literal(Literal::Int(6))),
        );
        assert_eq!(run_value(&expr, &scope), Value::Integer(0));
    }

    #[test]
    fn is_null_is_definite_true_or_false() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = Expr::IsNull {
            expr: Box::new(Expr::Literal(Literal::Int(1))),
            negated: false,
        };
        assert_eq!(run_value(&expr, &scope), Value::Integer(0));

        let expr = Expr::IsNull {
            expr: Box::new(Expr::Literal(Literal::Int(1))),
            negated: true,
        };
        assert_eq!(run_value(&expr, &scope), Value::Integer(1));
    }

    #[test]
    fn in_subquery_is_unsupported() {
        let scope = Scope::single(schema(&[]), 0);
        let inner = crate::expr::Query {
            columns: vec![crate::expr::SelectItem::Column("x".into())],
            from: "u".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
        };
        let expr = Expr::InSubquery {
            expr: Box::new(Expr::Literal(Literal::Int(1))),
            subquery: Box::new(inner),
        };
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        assert!(matches!(
            compile_value(&mut em, &mut reg, &scope, &expr),
            Err(CodegenError::Unsupported { .. })
        ));
    }
}
