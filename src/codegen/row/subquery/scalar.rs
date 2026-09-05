//! `EXISTS`/`IN` subquery-expression compilation -- see `super`'s module
//! doc, including why the reference's scalar-subquery and
//! multi-column-`IN` entry points have no db-core counterpart.

use super::from_clause::resolve_subquery_schema;
use crate::codegen::row::cond::{compile_cond, ensure_label};
use crate::codegen::row::value::compile_value;
use crate::codegen::row::{
    CodegenError, CondTargets, Emitter, NullTarget, RegAlloc, Result, Scope, Target,
};
use crate::expr::{Expr, Query, SelectItem};
use crate::vm::row::{Instruction, Opcode, P4};

/// A subquery's single projected result column -- `IN (SELECT ...)`
/// needs exactly one (`SELECT *`, an aggregate, or more than one column
/// is `Unsupported`), mirroring the reference's `single_result_expr`.
fn single_result_column(subquery: &Query) -> Result<&str> {
    match subquery.columns.as_slice() {
        [SelectItem::Column(name)] => Ok(name),
        _ => Err(CodegenError::Unsupported {
            reason: "an IN subquery must project exactly one plain column".to_string(),
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
    subquery: &Query,
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
    subquery: &Query,
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
    subquery: &Query,
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
    let v = compile_value(em, reg, &sub_scope, &Expr::Column(col_name.to_string()))?;
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
        let query = crate::parser::column::parse(sql).unwrap();
        compile_select_with_catalog(&catalog(), &query)
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
