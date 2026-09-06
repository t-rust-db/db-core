//! Value-mode expression compilation -- see `super`'s module doc.

use super::cond::compile_cond;
use super::{
    compile_contiguous_values, eq_expr, CodegenError, CondTargets, Emitter, RegAlloc, Result,
    Scope, Target, MAX_EXPR_DEPTH,
};
use crate::parser::ast::{BinaryOp, Expr, ExprKind, FunctionArgs, Literal, UnaryOp};
use crate::vm::row::{affinity_of, Affinity, Instruction, Opcode, P4};

/// Rebuilds the `"table.column"` string [`Scope::resolve`] expects.
/// `ExprKind::Column` carries `table` and `name` as separate fields;
/// `Scope::resolve` (unchanged since before #147) still takes one
/// dotted string, matching how `expr::Expr::Column` stored it.
fn qualified_name(table: Option<&str>, name: &str) -> String {
    match table {
        Some(table) => format!("{table}.{name}"),
        None => name.to_string(),
    }
}

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
    match &expr.kind {
        ExprKind::Column { table, name, .. } => {
            let (_, idx) = scope
                .resolve(&qualified_name(table.as_deref(), name))
                .ok()?;
            let declared = scope.schema.column_types.get(idx)?;
            Some(affinity_of(declared))
        }
        // Parentheses are preserved in the AST but carry no affinity of
        // their own -- `(a) = 'x'` must compare under `a`'s affinity
        // exactly as `a = 'x'` does.
        ExprKind::Paren(inner) => expr_affinity(scope, inner),
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
    match &expr.kind {
        ExprKind::Literal(lit) => {
            let r = reg.alloc();
            match lit {
                Literal::Integer(i) => match i32::try_from(*i) {
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
                Literal::Blob(bytes) => {
                    em.emit(Instruction::with_p4(
                        Opcode::Blob,
                        0,
                        r,
                        0,
                        P4::Blob(bytes.clone()),
                    ));
                }
                Literal::Null => {
                    em.emit(Instruction::new(Opcode::Null, 0, r, 0));
                }
                // SQLite has no boolean storage class: `TRUE`/`FALSE`
                // are the integers 1/0.
                Literal::True => {
                    em.emit(Instruction::new(Opcode::Integer, 1, r, 0));
                }
                Literal::False => {
                    em.emit(Instruction::new(Opcode::Integer, 0, r, 0));
                }
            }
            Ok(r)
        }

        ExprKind::Column { table, name, .. } => {
            let (cursor, idx) = scope.resolve(&qualified_name(table.as_deref(), name))?;
            let r = reg.alloc();
            emit_column_read(em, &scope.schema, cursor, idx, r)?;
            Ok(r)
        }

        // Parentheses affect only parse-time grouping, which the tree
        // shape already records; there is nothing to emit.
        ExprKind::Paren(inner) => compile_value_depth(em, reg, scope, inner, depth + 1),

        ExprKind::Unary { op, expr: inner } => {
            let r = compile_value_depth(em, reg, scope, inner, depth + 1)?;
            match op {
                // Unary `+` is a no-op on the value.
                UnaryOp::Plus => Ok(r),
                UnaryOp::Minus => {
                    let zero = reg.alloc();
                    em.emit(Instruction::new(Opcode::Integer, 0, zero, 0));
                    let dest = reg.alloc();
                    // `r[p3] = r[p2] - r[p1]` -> 0 - r = -r via
                    // p1=r, p2=zero.
                    em.emit(Instruction::new(Opcode::Subtract, r, zero, dest));
                    Ok(dest)
                }
                UnaryOp::Not => {
                    let dest = reg.alloc();
                    em.emit(Instruction::new(Opcode::Not, r, dest, 0));
                    Ok(dest)
                }
                UnaryOp::BitNot => {
                    let dest = reg.alloc();
                    em.emit(Instruction::new(Opcode::BitNot, r, dest, 0));
                    Ok(dest)
                }
            }
        }

        ExprKind::Binary { op, lhs, rhs } => {
            compile_binary(em, reg, scope, expr, *op, lhs, rhs, depth)
        }

        // Every condition form, used in a value context: each answers
        // true/false/unknown, which `compile_bool_to_value`
        // materializes three-valued by compiling it twice (db-core#95).
        ExprKind::IsNull { .. }
        | ExprKind::Is { .. }
        | ExprKind::Between { .. }
        | ExprKind::In { .. }
        | ExprKind::InSubquery { .. }
        | ExprKind::InSubqueryMulti { .. }
        | ExprKind::Exists { .. } => compile_bool_to_value(em, reg, scope, expr, depth),

        // `LIKE`/`GLOB` compute their own three-valued answer directly
        // (`vm::row::functions::like`/`glob` already propagate NULL),
        // so unlike the conditions above there is no cheaper/definite
        // split to make -- `compile_like` is the whole story.
        ExprKind::Like {
            expr: inner,
            pattern,
            glob,
            negated,
            escape,
        } => {
            let r = compile_like(
                em,
                reg,
                scope,
                inner,
                pattern,
                escape.as_deref(),
                *glob,
                depth,
            )?;
            if *negated {
                let dest = reg.alloc();
                em.emit(Instruction::new(Opcode::Not, r, dest, 0));
                Ok(dest)
            } else {
                Ok(r)
            }
        }

        // Parentheses affect grouping; `COLLATE` affects how a *later*
        // comparison treats this value, never the value itself -- both
        // are transparent here. `cond`'s comparison arm is where an
        // explicit `COLLATE` on an operand actually takes effect
        // (`resolve_collation`); silently ignoring it there, rather
        // than here, would be the actual bug.
        ExprKind::Collate { expr: inner, .. } => {
            compile_value_depth(em, reg, scope, inner, depth + 1)
        }

        ExprKind::Param(_) => Err(CodegenError::Unsupported {
            reason: "bind parameters are not supported by codegen::row yet (#162)".to_string(),
        }),
        ExprKind::FunctionCall {
            name,
            distinct,
            args,
            over,
        } => {
            if over.is_some() {
                return Err(CodegenError::Unsupported {
                    reason: format!("window function `{name}` is not supported by codegen::row"),
                });
            }
            if *distinct {
                return Err(CodegenError::Unsupported {
                    reason: format!("{name}(DISTINCT ...) is not supported outside an aggregate"),
                });
            }
            let FunctionArgs::List(args) = args else {
                return Err(CodegenError::Unsupported {
                    reason: format!("{name}(*) is only valid inside an aggregate call"),
                });
            };
            compile_function_call(em, reg, scope, name, args, depth)
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => compile_case(
            em,
            reg,
            scope,
            operand.as_deref(),
            whens,
            else_.as_deref(),
            depth,
        ),
        ExprKind::Cast {
            expr: inner,
            type_name,
        } => {
            let r = compile_value_depth(em, reg, scope, inner, depth + 1)?;
            let dest = reg.alloc();
            em.emit(Instruction::new(Opcode::Copy, r, dest, 0));
            let affinity = affinity_of(type_name);
            em.emit(Instruction::new(
                Opcode::Cast,
                dest,
                i32::from(affinity.to_p4_byte()),
                0,
            ));
            Ok(dest)
        }
        ExprKind::Subquery(_) => Err(CodegenError::Unsupported {
            reason: "scalar subqueries are not supported in value position yet (#163)".to_string(),
        }),
    }
}

/// Compiles a `name(args...)` scalar call via `Opcode::Function`,
/// dispatching into [`crate::vm::row::functions::call`]'s registry
/// (`upper`/`substr`/`coalesce`/`like`/... -- the same registry
/// [`compile_like`] uses for `LIKE`/`GLOB`). An unknown name or arity
/// is a runtime [`crate::vm::row::ExecError`] naming the call, not a
/// compile-time rejection -- this planner has no static copy of the
/// registry to check against ahead of time.
fn compile_function_call(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    name: &str,
    args: &[Expr],
    depth: usize,
) -> Result<i32> {
    if depth > MAX_EXPR_DEPTH {
        return Err(CodegenError::TooDeep);
    }
    let arg_refs: Vec<&Expr> = args.iter().collect();
    let first = compile_contiguous_values(em, reg, scope, &arg_refs)?;
    let dest = reg.alloc();
    let descriptor = format!("{}({})", name.to_ascii_lowercase(), args.len());
    em.emit(Instruction::with_p4(
        Opcode::Function,
        0,
        first,
        dest,
        P4::Str(descriptor),
    ));
    Ok(dest)
}

/// Compiles `expr [NOT] LIKE/GLOB pattern [ESCAPE escape]` via the same
/// `Opcode::Function` dispatch as a general call -- `LIKE`/`GLOB` are
/// syntactic sugar over the `like`/`glob` registry entries, not a
/// distinct VM primitive.
#[allow(clippy::too_many_arguments)]
fn compile_like(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    text: &Expr,
    pattern: &Expr,
    escape: Option<&Expr>,
    glob: bool,
    depth: usize,
) -> Result<i32> {
    if depth > MAX_EXPR_DEPTH {
        return Err(CodegenError::TooDeep);
    }
    if glob && escape.is_some() {
        return Err(CodegenError::Unsupported {
            reason: "GLOB does not take an ESCAPE clause".to_string(),
        });
    }
    // `vm::row::functions::like`/`glob` take (pattern, text[, escape]) --
    // the reverse of `text LIKE pattern`'s surface order.
    let args: Vec<&Expr> = match escape {
        Some(escape) => vec![pattern, text, escape],
        None => vec![pattern, text],
    };
    let first = compile_contiguous_values(em, reg, scope, &args)?;
    let dest = reg.alloc();
    let descriptor = format!("{}({})", if glob { "glob" } else { "like" }, args.len());
    em.emit(Instruction::with_p4(
        Opcode::Function,
        0,
        first,
        dest,
        P4::Str(descriptor),
    ));
    Ok(dest)
}

/// Compiles `CASE [operand] WHEN cond THEN result ... [ELSE else_] END`
/// into a single destination register, materializing each branch's
/// result there in turn -- the simple form (`operand` given) is
/// desugared to the searched form's `operand = cond` per `WHEN`
/// (db-core#150), since a `NULL` operand then never matches any `WHEN`
/// via ordinary `=`'s NULL propagation, matching SQL's own simple-CASE
/// semantics for free.
fn compile_case(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    operand: Option<&Expr>,
    whens: &[(Expr, Expr)],
    else_: Option<&Expr>,
    depth: usize,
) -> Result<i32> {
    if depth > MAX_EXPR_DEPTH {
        return Err(CodegenError::TooDeep);
    }
    let dest = reg.alloc();
    let end_label = em.new_label();
    for (cond, result) in whens {
        let next_label = em.new_label();
        let effective_cond = match operand {
            Some(operand) => eq_expr(operand.clone(), cond.clone()),
            None => cond.clone(),
        };
        compile_cond(
            em,
            reg,
            scope,
            &effective_cond,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(next_label)),
        )?;
        let r = compile_value_depth(em, reg, scope, result, depth + 1)?;
        em.emit(Instruction::new(Opcode::Copy, r, dest, 0));
        em.goto(end_label);
        em.place(next_label);
    }
    match else_ {
        Some(else_) => {
            let r = compile_value_depth(em, reg, scope, else_, depth + 1)?;
            em.emit(Instruction::new(Opcode::Copy, r, dest, 0));
        }
        None => {
            em.emit(Instruction::new(Opcode::Null, 0, dest, 0));
        }
    }
    em.place(end_label);
    Ok(dest)
}

/// Value-mode codegen for a binary operator. Split out of
/// [`compile_value_depth`] so the arithmetic/bitwise operand-order
/// rules live in one place: `Add`/`Multiply`/`BitAnd`/`BitOr` read as
/// `r[p3] = r[p1] <op> r[p2]`, while the non-commutative opcodes read
/// as `r[p3] = r[p2] <op> r[p1]` (sqlite-rs's own convention) and so
/// take their operands swapped.
#[allow(clippy::too_many_arguments)]
fn compile_binary(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    whole: &Expr,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    depth: usize,
) -> Result<i32> {
    let (opcode, reversed) = match op {
        BinaryOp::Add => (Opcode::Add, false),
        BinaryOp::Mul => (Opcode::Multiply, false),
        BinaryOp::BitAnd => (Opcode::BitAnd, false),
        BinaryOp::BitOr => (Opcode::BitOr, false),
        BinaryOp::Sub => (Opcode::Subtract, true),
        BinaryOp::Div => (Opcode::Divide, true),
        BinaryOp::Mod => (Opcode::Remainder, true),
        BinaryOp::Shl => (Opcode::ShiftLeft, true),
        BinaryOp::Shr => (Opcode::ShiftRight, true),
        BinaryOp::Concat => (Opcode::Concat, true),

        // Comparisons and the logical connectives are conditions used
        // in a value context.
        BinaryOp::Eq
        | BinaryOp::Ne
        | BinaryOp::Lt
        | BinaryOp::Le
        | BinaryOp::Gt
        | BinaryOp::Ge
        | BinaryOp::And
        | BinaryOp::Or => return compile_bool_to_value(em, reg, scope, whole, depth),
    };

    let l = compile_value_depth(em, reg, scope, lhs, depth + 1)?;
    let r = compile_value_depth(em, reg, scope, rhs, depth + 1)?;
    let dest = reg.alloc();
    if reversed {
        em.emit(Instruction::new(opcode, r, l, dest));
    } else {
        em.emit(Instruction::new(opcode, l, r, dest));
    }
    Ok(dest)
}

/// Whether a condition's outcome is always definitely true or
/// definitely false -- never SQL's unknown. `IS NULL`/`IS NOT NULL` and
/// `IS`/`IS NOT` both have this property: `cond`'s `Is` arm desugars to
/// `(both NULL) OR (both non-NULL AND equal)`, which by construction
/// never reaches `=`'s unknown case (the guarding `IS [NOT] NULL`
/// checks already ensure neither side is NULL by the time `=` runs).
fn is_definite(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::IsNull { .. } | ExprKind::Is { .. } => true,
        ExprKind::Paren(inner) => is_definite(inner),
        _ => false,
    }
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

    /// Wraps a kind into an [`Expr`] with an unknown span -- codegen
    /// never reads spans, so hand-built test expressions don't need
    /// real source positions.
    fn e(kind: ExprKind) -> Expr {
        Expr {
            kind,
            span: crate::parser::Span::UNKNOWN,
        }
    }

    fn int(i: i64) -> Expr {
        e(ExprKind::Literal(Literal::Integer(i)))
    }

    fn str_lit(s: &str) -> Expr {
        e(ExprKind::Literal(Literal::Str(s.to_string())))
    }

    fn binary(lhs: Expr, op: BinaryOp, rhs: Expr) -> Expr {
        e(ExprKind::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
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
        assert_eq!(run_value(&int(5), &scope), Value::Integer(5));
        let big = i64::from(i32::MAX) + 1;
        assert_eq!(run_value(&int(big), &scope), Value::Integer(big));
    }

    #[test]
    fn literal_float_and_str_roundtrip() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_value(&e(ExprKind::Literal(Literal::Float(1.5))), &scope),
            Value::Real(1.5)
        );
        assert_eq!(run_value(&str_lit("hi"), &scope), Value::Text("hi".into()));
    }

    /// The AST carries literal forms `expr::Literal` had no room for
    /// (#147): NULL, TRUE/FALSE, and blobs.
    #[test]
    fn literal_null_true_false_and_blob_compile() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_value(&e(ExprKind::Literal(Literal::Null)), &scope),
            Value::Null
        );
        assert_eq!(
            run_value(&e(ExprKind::Literal(Literal::True)), &scope),
            Value::Integer(1)
        );
        assert_eq!(
            run_value(&e(ExprKind::Literal(Literal::False)), &scope),
            Value::Integer(0)
        );
        assert_eq!(
            run_value(&e(ExprKind::Literal(Literal::Blob(vec![1, 2]))), &scope),
            Value::Blob(vec![1, 2].into())
        );
    }

    #[test]
    fn arithmetic_operand_order_matches_sql() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = binary(int(10), BinaryOp::Sub, int(3));
        assert_eq!(run_value(&expr, &scope), Value::Integer(7));

        let expr = binary(int(10), BinaryOp::Div, int(4));
        assert_eq!(run_value(&expr, &scope), Value::Integer(2));
    }

    /// `%`, `<<`, `>>`, `&`, `|` have no `expr::BinOp` equivalent and so
    /// could never reach codegen before #147. Operand order matters for
    /// the non-commutative three.
    #[test]
    fn bitwise_and_modulo_operand_order_matches_sql() {
        let scope = Scope::single(schema(&[]), 0);
        for (lhs, op, rhs, want) in [
            (10, BinaryOp::Mod, 3, 1),
            (1, BinaryOp::Shl, 3, 8),
            (16, BinaryOp::Shr, 2, 4),
            (12, BinaryOp::BitAnd, 10, 8),
            (12, BinaryOp::BitOr, 10, 14),
        ] {
            let expr = binary(int(lhs), op, int(rhs));
            assert_eq!(
                run_value(&expr, &scope),
                Value::Integer(want),
                "{lhs:?} {op:?} {rhs:?}"
            );
        }
    }

    #[test]
    fn concat_operand_order_matches_sql() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = binary(str_lit("a"), BinaryOp::Concat, str_lit("b"));
        assert_eq!(run_value(&expr, &scope), Value::Text("ab".into()));
    }

    #[test]
    fn unary_minus_negates_and_plus_is_a_no_op() {
        let scope = Scope::single(schema(&[]), 0);
        let neg = e(ExprKind::Unary {
            op: UnaryOp::Minus,
            expr: Box::new(int(5)),
        });
        assert_eq!(run_value(&neg, &scope), Value::Integer(-5));

        let plus = e(ExprKind::Unary {
            op: UnaryOp::Plus,
            expr: Box::new(int(5)),
        });
        assert_eq!(run_value(&plus, &scope), Value::Integer(5));

        let bitnot = e(ExprKind::Unary {
            op: UnaryOp::BitNot,
            expr: Box::new(int(0)),
        });
        assert_eq!(run_value(&bitnot, &scope), Value::Integer(-1));
    }

    /// `ExprKind::Paren` is preserved by the parser but must not change
    /// the emitted program or the expression's affinity.
    #[test]
    fn paren_is_transparent_to_value_codegen() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = e(ExprKind::Paren(Box::new(binary(
            int(10),
            BinaryOp::Sub,
            int(3),
        ))));
        assert_eq!(run_value(&expr, &scope), Value::Integer(7));
    }

    #[test]
    fn comparison_materializes_three_valued() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = binary(int(5), BinaryOp::Eq, int(5));
        assert_eq!(run_value(&expr, &scope), Value::Integer(1));

        let expr = binary(int(5), BinaryOp::Eq, int(6));
        assert_eq!(run_value(&expr, &scope), Value::Integer(0));
    }

    #[test]
    fn is_null_is_definite_true_or_false() {
        let scope = Scope::single(schema(&[]), 0);
        let expr = e(ExprKind::IsNull {
            expr: Box::new(int(1)),
            negated: false,
        });
        assert_eq!(run_value(&expr, &scope), Value::Integer(0));

        let expr = e(ExprKind::IsNull {
            expr: Box::new(int(1)),
            negated: true,
        });
        assert_eq!(run_value(&expr, &scope), Value::Integer(1));
    }

    #[test]
    fn in_subquery_without_a_catalog_is_unsupported() {
        let scope = Scope::single(schema(&[]), 0);
        // db-core#95 compiles `InSubquery` in a value context, but a
        // scope with no catalog can't resolve the subquery's own table.
        let expr = e(ExprKind::InSubquery {
            expr: Box::new(int(1)),
            subquery: Box::new(parse_one_select("SELECT x FROM u")),
            negated: false,
        });
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        assert!(matches!(
            compile_value(&mut em, &mut reg, &scope, &expr),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    /// Constructs the AST the way the engine will: by parsing real SQL
    /// through the crate's only grammar, rather than hand-building a
    /// planner-shaped struct (#147).
    fn parse_one_select(sql: &str) -> crate::parser::ast::Select {
        match crate::parser::row::parse_select(sql) {
            crate::parser::row::ParseOutcome::Accepted(select) => *select,
            other => panic!("expected {sql:?} to parse, got {other:?}"),
        }
    }

    /// Constructs that the AST can express but `codegen::row` cannot
    /// compile yet must fail soft, naming the construct -- never panic
    /// mid-query (#147).
    /// Bind parameters and scalar subqueries remain genuinely
    /// unsupported (own follow-up tickets, #162/#163) -- everything
    /// else `#150` listed is exercised for real below.
    #[test]
    fn params_and_scalar_subqueries_fail_soft_and_name_themselves() {
        let scope = Scope::single(schema(&["a"]), 0);
        let cases = [
            (
                e(ExprKind::Param(crate::parser::ast::ParamKind::Anonymous)),
                "bind parameter",
            ),
            (
                crate::codegen::row::testutil::expr("(SELECT a FROM t)"),
                "scalar subquery",
            ),
        ];
        for (expr, label) in cases {
            let mut em = Emitter::new();
            let mut reg = RegAlloc::new();
            assert!(
                matches!(
                    compile_value(&mut em, &mut reg, &scope, &expr),
                    Err(CodegenError::Unsupported { .. })
                ),
                "{label} should be reported as unsupported, not panic"
            );
        }
    }

    /// A window-function call and `DISTINCT`/`*` outside an aggregate
    /// are also rejected, distinctly from an unknown scalar function --
    /// see [`unknown_function_is_a_runtime_error`].
    #[test]
    fn window_calls_and_bare_star_are_unsupported_in_value_position() {
        let scope = Scope::single(schema(&["a"]), 0);
        for sql in ["COUNT(*) OVER (ORDER BY a)", "COUNT(DISTINCT a)"] {
            let expr = crate::codegen::row::testutil::expr(sql);
            let mut em = Emitter::new();
            let mut reg = RegAlloc::new();
            assert!(
                matches!(
                    compile_value(&mut em, &mut reg, &scope, &expr),
                    Err(CodegenError::Unsupported { .. })
                ),
                "{sql:?} should be reported as unsupported"
            );
        }
    }

    #[test]
    fn function_call_dispatches_into_the_vm_row_functions_registry() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_value(&crate::codegen::row::testutil::expr("UPPER('abc')"), &scope),
            Value::Text("ABC".into())
        );
        assert_eq!(
            run_value(&crate::codegen::row::testutil::expr("ABS(-5)"), &scope),
            Value::Integer(5)
        );
        assert_eq!(
            run_value(
                &crate::codegen::row::testutil::expr("SUBSTR('hello', 2, 3)"),
                &scope
            ),
            Value::Text("ell".into())
        );
    }

    /// An unknown function name/arity has no static registry to check
    /// against at compile time, so it surfaces as a runtime error --
    /// still a clear error naming the call, not a panic.
    #[test]
    fn unknown_function_is_a_runtime_error() {
        let scope = Scope::single(schema(&[]), 0);
        let mut em = Emitter::new();
        let mut reg = RegAlloc::new();
        let dest = compile_value(
            &mut em,
            &mut reg,
            &scope,
            &crate::codegen::row::testutil::expr("NOPE(1)"),
        )
        .unwrap();
        em.emit(Instruction::new(Opcode::ResultRow, dest, 1, 0));
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        let mut vm = crate::vm::row::Vm::new();
        assert!(crate::vm::row::execute(&mut vm, &em.finish()).is_err());
    }

    #[test]
    fn case_searched_and_simple_forms() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_value(
                &crate::codegen::row::testutil::expr(
                    "CASE WHEN 1 = 2 THEN 'a' WHEN 1 = 1 THEN 'b' ELSE 'c' END"
                ),
                &scope
            ),
            Value::Text("b".into())
        );
        assert_eq!(
            run_value(
                &crate::codegen::row::testutil::expr("CASE WHEN 1 = 2 THEN 'a' END"),
                &scope
            ),
            Value::Null,
            "no WHEN matched and there's no ELSE"
        );
        assert_eq!(
            run_value(
                &crate::codegen::row::testutil::expr("CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' END"),
                &scope
            ),
            Value::Text("b".into()),
            "simple form: operand = each WHEN value"
        );
        assert_eq!(
            run_value(
                &crate::codegen::row::testutil::expr("CASE NULL WHEN NULL THEN 'a' ELSE 'b' END"),
                &scope
            ),
            Value::Text("b".into()),
            "a NULL operand never matches any WHEN via ordinary ="
        );
    }

    #[test]
    fn cast_applies_the_named_affinity() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_value(
                &crate::codegen::row::testutil::expr("CAST('5' AS INTEGER)"),
                &scope
            ),
            Value::Integer(5)
        );
        assert_eq!(
            run_value(
                &crate::codegen::row::testutil::expr("CAST(5 AS TEXT)"),
                &scope
            ),
            Value::Text("5".into())
        );
    }

    /// `COLLATE` doesn't change the value itself -- only how a later
    /// comparison treats it (`cond`'s `resolve_collation`).
    #[test]
    fn collate_is_transparent_to_the_raw_value() {
        let scope = Scope::single(schema(&[]), 0);
        assert_eq!(
            run_value(
                &crate::codegen::row::testutil::expr("'abc' COLLATE NOCASE"),
                &scope
            ),
            Value::Text("abc".into())
        );
    }
}
