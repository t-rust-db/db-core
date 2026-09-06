//! `EXISTS`/`IN`/scalar subquery-expression compilation -- see `super`'s
//! module doc, including why the reference's multi-column-`IN` entry
//! point has no db-core counterpart.

use super::from_clause::resolve_subquery_schema;
use crate::codegen::row::cond::{compile_cond, ensure_label};
use crate::codegen::row::value::compile_value;
use crate::codegen::row::{
    CodegenError, CondTargets, Emitter, NullTarget, RegAlloc, Result, Scope, Target,
};
use crate::parser::ast::{Expr, ExprKind, ResultColumn, Select};
use crate::vm::row::{Instruction, Opcode, P4};

/// A subquery's single projected result column -- `IN (SELECT ...)` and
/// a scalar `(SELECT ...)` in value position both need exactly one
/// (`SELECT *`, an aggregate, or more than one column is `Unsupported`),
/// mirroring the reference's `single_result_expr`.
fn single_result_column(subquery: &Select) -> Result<&str> {
    match subquery.columns.as_slice() {
        [ResultColumn::Expr {
            expr:
                Expr {
                    kind: ExprKind::Column { name, .. },
                    ..
                },
            ..
        }] => Ok(name),
        _ => Err(CodegenError::Unsupported {
            reason: "a subquery in this position must project exactly one plain column".to_string(),
        }),
    }
}

/// Opens the subquery's own table cursor and builds its scope, with the
/// enclosing scope as [`Scope::outer`] so a correlated column reference
/// resolves there.
fn open_subquery_scan(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    outer_scope: &Scope,
    subquery: &Select,
) -> Result<(i32, Scope)> {
    let schema = resolve_subquery_schema(subquery, outer_scope)?;
    let sub_cursor = reg.alloc_cursor();
    em.emit(Instruction::new(
        Opcode::OpenRead,
        sub_cursor,
        i32::try_from(schema.root_page).map_err(|_| CodegenError::Unsupported {
            reason: format!(
                "root page {} does not fit in a p2 operand",
                schema.root_page
            ),
        })?,
        0,
    ));
    let sub_scope = Scope::single(schema, sub_cursor)
        .with_catalog(outer_scope.catalog.clone())
        .with_outer(outer_scope.clone());
    Ok((sub_cursor, sub_scope))
}

/// Compiles `[NOT] EXISTS (SELECT ...)` as a jump: runs the subquery's
/// scan and jumps to the true continuation as soon as one row satisfies
/// its `WHERE` clause (or immediately, if it has none), without
/// materializing anything -- cheaper than the `IN` form since `EXISTS`
/// never needs a row's actual values. `EXISTS` is always definitely true
/// or false (never SQL's unknown), so `targets.on_null` is not consulted.
pub fn compile_exists(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    outer_scope: &Scope,
    subquery: &Select,
    negated: bool,
    targets: CondTargets,
) -> Result<()> {
    let (sub_cursor, sub_scope) = open_subquery_scan(em, reg, outer_scope, subquery)?;

    let (exists_true, exists_false) = if negated {
        (targets.on_false, targets.on_true)
    } else {
        (targets.on_true, targets.on_false)
    };
    let (t_label, t_is_new) = ensure_label(em, exists_true);

    let not_found = em.new_label();
    let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, sub_cursor, 0, 0));
    em.patch_p2(rewind_addr, not_found);
    let loop_start = em.new_label();
    em.place(loop_start);

    let skip = em.new_label();
    if let Some(where_expr) = &subquery.where_clause {
        compile_cond(
            em,
            reg,
            &sub_scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(skip)),
        )?;
    }
    em.goto(t_label);
    em.place(skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, sub_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    em.place(not_found);

    if let Target::Jump(fl) = exists_false {
        em.goto(fl);
    }
    if t_is_new {
        em.place(t_label);
    }
    Ok(())
}

/// Compiles `expr IN (SELECT ...)`: materializes the subquery's single
/// result column into a fresh ephemeral index (the same
/// `OpenEphemeral`/`IdxInsert`/`Found` machinery the reference uses),
/// then tests `expr`'s value for membership.
///
/// Known simplification, carried over from the reference: a NULL `expr`
/// always routes to the unknown (`on_null`) continuation, rather than
/// SQLite's more precise rule that `NULL IN (<empty result>)` is
/// definitely false.
pub fn compile_in_subquery(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    outer_scope: &Scope,
    lhs: &Expr,
    subquery: &Select,
    negated: bool,
    targets: CondTargets,
) -> Result<()> {
    let col_name = single_result_column(subquery)?;

    let l = compile_value(em, reg, outer_scope, lhs)?;

    let eph_cursor = reg.alloc_cursor();
    em.emit(Instruction::new(Opcode::OpenEphemeral, eph_cursor, 0, 0));

    let (sub_cursor, sub_scope) = open_subquery_scan(em, reg, outer_scope, subquery)?;

    let scan_end = em.new_label();
    let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, sub_cursor, 0, 0));
    em.patch_p2(rewind_addr, scan_end);
    let loop_start = em.new_label();
    em.place(loop_start);

    let skip = em.new_label();
    if let Some(where_expr) = &subquery.where_clause {
        compile_cond(
            em,
            reg,
            &sub_scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(skip)),
        )?;
    }
    let v = compile_value(em, reg, &sub_scope, &super::super::column_expr(col_name))?;
    em.emit(Instruction::with_p4(
        Opcode::IdxInsert,
        eph_cursor,
        v,
        0,
        P4::Int(1),
    ));
    em.place(skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, sub_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    em.place(scan_end);

    let (true_label, true_is_new) = ensure_label(em, targets.on_true);
    let (false_label, false_is_new) = ensure_label(em, targets.on_false);
    let (found_label, notfound_label) = if negated {
        (false_label, true_label)
    } else {
        (true_label, false_label)
    };
    let null_label = match targets.on_null {
        NullTarget::True => true_label,
        NullTarget::False => false_label,
    };

    let null_addr = em.emit(Instruction::new(Opcode::IsNull, l, 0, 0));
    em.patch_p2(null_addr, null_label);
    let found_addr = em.emit(Instruction::with_p4(
        Opcode::Found,
        eph_cursor,
        0,
        l,
        P4::Int(1),
    ));
    em.patch_p2(found_addr, found_label);
    em.goto(notfound_label);

    if false_is_new {
        em.place(false_label);
    }
    if true_is_new {
        em.place(true_label);
    }
    Ok(())
}

/// Compiles a scalar `(SELECT ...)` used in value position: the first
/// row's single projected column, or `NULL` if the subquery produces no
/// rows. A second or later row is simply never reached -- unlike
/// [`compile_in_subquery`], which drains the whole scan into an
/// ephemeral index, this stops at the first match, matching SQLite's
/// behaviour for a scalar subquery.
pub fn compile_scalar_subquery(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    outer_scope: &Scope,
    subquery: &Select,
) -> Result<i32> {
    let col_name = single_result_column(subquery)?;

    let (sub_cursor, sub_scope) = open_subquery_scan(em, reg, outer_scope, subquery)?;

    let dest = reg.alloc();
    let no_rows = em.new_label();
    let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, sub_cursor, 0, 0));
    em.patch_p2(rewind_addr, no_rows);
    let loop_start = em.new_label();
    em.place(loop_start);

    let skip = em.new_label();
    if let Some(where_expr) = &subquery.where_clause {
        compile_cond(
            em,
            reg,
            &sub_scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(skip)),
        )?;
    }
    let v = compile_value(em, reg, &sub_scope, &super::super::column_expr(col_name))?;
    em.emit(Instruction::new(Opcode::Copy, v, dest, 0));
    let done = em.new_label();
    em.goto(done);

    em.place(skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, sub_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);

    em.place(no_rows);
    em.emit(Instruction::new(Opcode::Null, 0, dest, 0));

    em.place(done);
    Ok(dest)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::codegen::row::select::compile_select_with_catalog;
    use crate::codegen::row::TableSchema;
    use crate::vm::row::Program;

    fn table(name: &str, root_page: u32, columns: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: vec![String::new(); columns.len()],
            rowid_alias: None,
            root_page,
            indexes: Vec::new(),
        }
    }

    fn catalog() -> Vec<TableSchema> {
        vec![table("t", 2, &["a", "b"]), table("s", 3, &["x", "y"])]
    }

    fn compile(sql: &str) -> Result<Program> {
        // Parses through the crate's only grammar rather than
        // `parser::column`'s analytics-subset lowering, which exists to
        // feed `codegen::batch` and rejects most of what this planner
        // now accepts (#147).
        compile_select_with_catalog(&catalog(), &crate::codegen::row::testutil::select(sql))
    }

    fn opcodes(program: &Program) -> Vec<Opcode> {
        program.instructions.iter().map(|i| i.opcode).collect()
    }

    #[test]
    fn exists_plain_scan() {
        let program = compile("SELECT a FROM t WHERE EXISTS (SELECT x FROM s)").unwrap();
        let ops = opcodes(&program);
        assert!(
            ops.iter().filter(|o| **o == Opcode::Rewind).count() >= 2,
            "{ops:?}"
        );
        assert!(!ops.contains(&Opcode::OpenEphemeral), "{ops:?}");
    }

    #[test]
    fn not_exists_scan_with_where() {
        let program =
            compile("SELECT a FROM t WHERE NOT EXISTS (SELECT x FROM s WHERE s.x = t.a)").unwrap();
        let ops = opcodes(&program);
        assert!(ops.contains(&Opcode::Rewind), "{ops:?}");
        assert!(ops.contains(&Opcode::Next), "{ops:?}");
    }

    #[test]
    fn exists_subquery_over_unknown_table_is_unsupported() {
        let err = compile("SELECT a FROM t WHERE EXISTS (SELECT z FROM nope)").unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(
                    reason.contains("isn't visible to this compiler's catalog"),
                    "{reason}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn in_subquery_materializes_an_ephemeral_index() {
        let program = compile("SELECT a FROM t WHERE a IN (SELECT x FROM s)").unwrap();
        let ops = opcodes(&program);
        assert!(ops.contains(&Opcode::OpenEphemeral), "{ops:?}");
        assert!(ops.contains(&Opcode::IdxInsert), "{ops:?}");
        assert!(ops.contains(&Opcode::Found), "{ops:?}");
    }

    #[test]
    fn in_subquery_star_projection_is_unsupported() {
        let err = compile("SELECT a FROM t WHERE a IN (SELECT * FROM s)").unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("exactly one plain column"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn subquery_with_limit_is_unsupported() {
        let err = compile("SELECT a FROM t WHERE a IN (SELECT x FROM s LIMIT 1)").unwrap_err();
        assert!(matches!(err, CodegenError::Unsupported { .. }), "{err:?}");
    }

    fn run(sql: &str, t_rows: Vec<Vec<crate::vm::row::Value>>) -> Vec<Vec<crate::vm::row::Value>> {
        let program = compile(sql).unwrap();
        let mut vm = crate::vm::row::Vm::new();
        // `t` (cursor 0) is the outer table `compile_select_with_catalog`
        // always leaves pre-wired by the caller, exactly like
        // `compile_select`'s own tests. No `CursorFactory` is installed
        // here (implementing that trait outside the VM's own dyn
        // boundary -- see `MVL_LIMIT_EXCLUDE` in the Makefile -- would
        // put a `Box<dyn Cursor>`-returning fn signature outside the
        // qualified subset), so the subquery's own cursor slot is
        // pre-wired the same way: find its `OpenRead`'s slot (the one
        // that isn't `t`'s) and wire it directly via `open_cursor`,
        // exactly like `Opcode::OpenRead`'s own pre-wired fallback path
        // expects when no factory is installed.
        vm.open_cursor(0, Box::new(crate::vm::row::InMemoryCursor::new(t_rows)))
            .unwrap();
        let sub_slot = match program
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::OpenRead && i.p1 != 0)
        {
            Some(instr) => instr.p1,
            None => panic!("compiled program opens a subquery cursor"),
        };
        vm.open_cursor(
            sub_slot,
            Box::new(crate::vm::row::InMemoryCursor::new(s_rows())),
        )
        .unwrap();
        crate::vm::row::execute(&mut vm, &program).unwrap()
    }

    fn s_rows() -> Vec<Vec<crate::vm::row::Value>> {
        use crate::vm::row::Value;
        vec![
            vec![Value::Integer(10), Value::Integer(1)],
            vec![Value::Integer(20), Value::Integer(2)],
        ]
    }

    // `compile_select`'s plain projection path only resolves bare
    // columns by name in the SELECT list (see `select.rs`'s
    // `only bare column references are supported` check) -- a scalar
    // subquery in *that* position is therefore blocked by a
    // pre-existing, unrelated limitation, not anything from #163. `WHERE`
    // is the value position this planner already compiles arbitrary
    // expressions in (it's how `a IN (SELECT ...)` above is exercised
    // too), so these tests exercise the scalar subquery there instead.

    #[test]
    fn scalar_subquery_returns_the_first_rows_column() {
        use crate::vm::row::Value;
        let rows = run(
            "SELECT a FROM t WHERE (SELECT x FROM s) = 10",
            vec![vec![Value::Integer(1), Value::Integer(2)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn scalar_subquery_over_zero_rows_is_null() {
        use crate::vm::row::Value;
        let rows = run(
            "SELECT a FROM t WHERE (SELECT x FROM s WHERE x = 999) IS NULL",
            vec![vec![Value::Integer(1), Value::Integer(2)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn scalar_subquery_ignores_rows_after_the_first_match() {
        use crate::vm::row::Value;
        let rows = run(
            "SELECT a FROM t WHERE (SELECT x FROM s WHERE x > 5) = 10",
            vec![vec![Value::Integer(1), Value::Integer(2)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn scalar_subquery_is_correlated_against_the_outer_row() {
        use crate::vm::row::Value;
        let rows = run(
            "SELECT a FROM t WHERE (SELECT x FROM s WHERE y = a) = 20",
            vec![
                vec![Value::Integer(1), Value::Integer(0)],
                vec![Value::Integer(2), Value::Integer(0)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn scalar_subquery_star_projection_is_unsupported() {
        let err = compile("SELECT a FROM t WHERE (SELECT * FROM s) = 10").unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("exactly one plain column"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn correlated_column_resolves_against_the_outer_scope() {
        // `t.a` is not a column of `s`, so it can only resolve through
        // `Scope::outer` -- and it must read the *outer* cursor.
        let program =
            compile("SELECT a FROM t WHERE EXISTS (SELECT x FROM s WHERE s.x = t.a)").unwrap();
        let cursors: Vec<i32> = program
            .instructions
            .iter()
            .filter(|i| i.opcode == Opcode::Column)
            .map(|i| i.p1)
            .collect();
        assert!(cursors.contains(&0), "{cursors:?}");
        assert!(cursors.iter().any(|c| *c != 0), "{cursors:?}");
    }
}
