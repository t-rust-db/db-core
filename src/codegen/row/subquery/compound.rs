//! Compound `SELECT` (`UNION`/`UNION ALL`) codegen (db-core#175).
//!
//! Each arm is compiled as a plain single-table scan (no `JOIN`, no
//! `GROUP BY`/aggregate) inline into one shared instruction stream, in
//! the same "materialize into an ephemeral table" style
//! [`super::from_clause::materialize_from_subquery`] already uses for a
//! `FROM`-subquery: every arm's projected row is `MakeRecord`ed and
//! `Insert`ed into one shared ephemeral table (`dest_cursor`), which the
//! caller then scans like any other single-table `SELECT`.
//!
//! A `UNION` (as opposed to `UNION ALL`) chain adds one shared ephemeral
//! *index* (`dedup_cursor`), keyed on every output column, that each
//! arm's row is `Found`-probed against before insertion -- the same
//! `OpenEphemeral`/`IdxInsert`/`Found` membership dance
//! [`super::scalar::compile_in_subquery`] uses, generalized from a
//! single-column key to the whole row. A chain of *only* `UNION` arms is
//! exactly "distinct over the concatenation of every arm": `UNION` is
//! associative and idempotent, so deduping against one index shared
//! across all arms (including the first) is equivalent to SQLite's
//! pairwise-left-associative reading. A **mixed** `UNION`/`UNION ALL`
//! chain has no such simplification (SQLite's compound evaluates
//! left-to-right, each step's distinctness independent of the next) and
//! is rejected outright rather than compiled wrong.
//!
//! **Deferred**, tracked on #175 same as the rest of that ticket's gap
//! map: `ORDER BY`/`LIMIT` over the whole compound (would need the
//! combined result materialized before either can apply -- the ephemeral
//! table this module already builds is exactly that materialization, so
//! wiring the existing sorter/limit machinery over `dest_cursor` instead
//! of a real table is the natural follow-up); `GROUP BY`/aggregation in
//! any arm; `INTERSECT`/`EXCEPT` (the AST has no such [`CompoundOp`]
//! variant yet -- see its own doc).

use crate::codegen::row::cond::compile_cond;
use crate::codegen::row::value::compile_value;
use crate::codegen::row::{
    column_expr, CodegenError, CondTargets, Emitter, RegAlloc, Result, Scope, Target,
};
use crate::parser::ast::{CompoundOp, Expr, ExprKind, FromClause, ResultColumn, Select};
use crate::vm::row::{Instruction, Opcode, Program, P4};

use super::from_clause::resolve_from_table_schema;

/// Compiles `query`, a compound `SELECT` (`query.compound` non-empty),
/// against `catalog`. See this module's own doc for the exact supported
/// shape and what's deferred.
pub fn compile_compound_select(
    catalog: &[crate::codegen::row::TableSchema],
    query: &Select,
) -> Result<Program> {
    if query.with_clause.is_some() {
        return Err(CodegenError::Unsupported {
            reason: "WITH combined with a compound SELECT is not yet supported".to_string(),
        });
    }
    if !query.order_by.is_empty() || query.limit.is_some() {
        return Err(CodegenError::Unsupported {
            reason: "ORDER BY/LIMIT over a compound SELECT is not yet supported".to_string(),
        });
    }
    let uniform_op = query.compound.first().map(|arm| arm.op);
    if query.compound.iter().any(|arm| Some(arm.op) != uniform_op) {
        return Err(CodegenError::Unsupported {
            reason: "mixing UNION and UNION ALL in one compound SELECT is not yet supported"
                .to_string(),
        });
    }
    let dedup = uniform_op == Some(CompoundOp::Union);

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    let dest_cursor = reg.alloc_cursor();
    em.emit(Instruction {
        p5: 1,
        ..Instruction::new(Opcode::OpenEphemeral, dest_cursor, 0, 0)
    });
    let dedup_cursor = if dedup {
        let c = reg.alloc_cursor();
        em.emit(Instruction::new(Opcode::OpenEphemeral, c, 0, 0));
        Some(c)
    } else {
        None
    };

    let mut expected_arity = None;
    compile_arm(
        &mut em,
        &mut reg,
        catalog,
        &query.columns,
        query.from.as_ref(),
        query.where_clause.as_ref(),
        &query.group_by,
        query.having.as_ref(),
        dest_cursor,
        dedup_cursor,
        &mut expected_arity,
    )?;
    for arm in &query.compound {
        compile_arm(
            &mut em,
            &mut reg,
            catalog,
            &arm.columns,
            arm.from.as_ref(),
            arm.where_clause.as_ref(),
            &arm.group_by,
            arm.having.as_ref(),
            dest_cursor,
            dedup_cursor,
            &mut expected_arity,
        )?;
    }

    // `dest_cursor` already holds every arm's rows in insertion order
    // (db-core#182: the cursor a program itself opened, not a caller-wired
    // one) -- no need to duplicate it, just rewind and scan it directly.
    let end_label = em.new_label();
    let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, dest_cursor, 0, 0));
    em.patch_p2(rewind_addr, end_label);
    let loop_start = em.new_label();
    em.place(loop_start);

    let count = expected_arity.unwrap_or(0);
    let count_i32 = i32::try_from(count).map_err(|_| CodegenError::Unsupported {
        reason: format!("{count} columns do not fit in a p2 operand"),
    })?;
    let mut first = None;
    for idx in 0..count {
        let r = reg.alloc();
        first.get_or_insert(r);
        em.emit(Instruction::new(
            Opcode::Column,
            dest_cursor,
            i32::try_from(idx).unwrap_or(i32::MAX),
            r,
        ));
    }
    em.emit(Instruction::new(
        Opcode::ResultRow,
        first.unwrap_or_else(|| reg.alloc()),
        count_i32,
        0,
    ));

    let next_addr = em.emit(Instruction::new(Opcode::Next, dest_cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    em.place(end_label);
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// Compiles one arm's plain scan, projecting its output columns into
/// `dest_cursor` (deduped against `dedup_cursor` first, when present).
/// `expected_arity` is set by the first arm compiled and checked against
/// every arm after it -- SQLite requires every arm of a compound to
/// project the same number of columns. Takes an arm's fields directly
/// (rather than a `Select`/`CompoundSelect` reference) since those are
/// two distinct AST types with the same shape but no common trait.
#[allow(clippy::too_many_arguments)]
fn compile_arm(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    catalog: &[crate::codegen::row::TableSchema],
    columns: &[ResultColumn],
    from: Option<&FromClause>,
    where_clause: Option<&Expr>,
    group_by: &[Expr],
    having: Option<&Expr>,
    dest_cursor: i32,
    dedup_cursor: Option<i32>,
    expected_arity: &mut Option<usize>,
) -> Result<()> {
    if !group_by.is_empty() || having.is_some() {
        return Err(CodegenError::Unsupported {
            reason: "GROUP BY/HAVING in a compound SELECT arm is not yet supported".to_string(),
        });
    }
    let schema = resolve_from_table_schema(from, catalog)?;
    if !joins_of_from(from).is_empty() {
        return Err(CodegenError::Unsupported {
            reason: "a compound SELECT arm with a JOIN is not yet supported".to_string(),
        });
    }

    let output_exprs = resolve_output_exprs(columns, &schema)?;
    match expected_arity {
        None => *expected_arity = Some(output_exprs.len()),
        Some(want) if *want != output_exprs.len() => {
            return Err(CodegenError::Unsupported {
                reason: format!(
                    "every arm of a compound SELECT must project the same number of columns \
                     ({want} vs {})",
                    output_exprs.len()
                ),
            });
        }
        Some(_) => {}
    }

    let cursor = reg.alloc_cursor();
    em.emit(Instruction::new(
        Opcode::OpenRead,
        cursor,
        crate::codegen::row::valid_table_root_page(&schema)?,
        0,
    ));
    let scope = Scope::single(schema, cursor).with_catalog(catalog.to_vec());

    let end_label = em.new_label();
    let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, cursor, 0, 0));
    em.patch_p2(rewind_addr, end_label);
    let loop_start = em.new_label();
    em.place(loop_start);
    let skip = em.new_label();

    if let Some(where_expr) = where_clause {
        compile_cond(
            em,
            reg,
            &scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(skip)),
        )?;
    }

    let mut first_reg = None;
    for (i, expr) in output_exprs.iter().enumerate() {
        let r = compile_value(em, reg, &scope, expr)?;
        match first_reg {
            None => first_reg = Some(r),
            Some(first) => {
                let want = first.saturating_add(i32::try_from(i).unwrap_or(i32::MAX));
                if r != want {
                    return Err(CodegenError::Unsupported {
                        reason: "a compound SELECT arm's projection must compile into \
                                 contiguous registers"
                            .to_string(),
                    });
                }
            }
        }
    }
    let Some(first) = first_reg else {
        return Err(CodegenError::Unsupported {
            reason: "a compound SELECT arm must project at least one column".to_string(),
        });
    };
    let count = i32::try_from(output_exprs.len()).map_err(|_| CodegenError::Unsupported {
        reason: format!("{} columns do not fit in a p2 operand", output_exprs.len()),
    })?;

    if let Some(dedup_cursor) = dedup_cursor {
        let record_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::MakeRecord,
            first,
            count,
            record_reg,
        ));
        let dup = em.new_label();
        let found_addr = em.emit(Instruction::with_p4(
            Opcode::Found,
            dedup_cursor,
            0,
            first,
            P4::Int(count.into()),
        ));
        em.patch_p2(found_addr, dup);
        em.emit(Instruction::with_p4(
            Opcode::IdxInsert,
            dedup_cursor,
            first,
            0,
            P4::Int(count.into()),
        ));
        let rowid_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::Sequence,
            dest_cursor,
            rowid_reg,
            0,
        ));
        em.emit(Instruction::new(
            Opcode::Insert,
            dest_cursor,
            rowid_reg,
            record_reg,
        ));
        em.place(dup);
    } else {
        let rowid_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::Sequence,
            dest_cursor,
            rowid_reg,
            0,
        ));
        let record_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::MakeRecord,
            first,
            count,
            record_reg,
        ));
        em.emit(Instruction::new(
            Opcode::Insert,
            dest_cursor,
            rowid_reg,
            record_reg,
        ));
    }

    em.place(skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);
    em.place(end_label);
    Ok(())
}

/// One arm's projected columns as [`Expr`]s -- a bare column resolves to
/// [`column_expr`] (the existing fast/uniform representation
/// [`compile_value`] and every other `Expr`-shaped column expects),
/// `*`/`table.*` expand against `schema`, and anything else compiles as
/// the arbitrary expression it already is.
fn resolve_output_exprs(
    columns: &[ResultColumn],
    schema: &crate::codegen::row::TableSchema,
) -> Result<Vec<Expr>> {
    let mut out = Vec::with_capacity(columns.len());
    for col in columns {
        match col {
            ResultColumn::Expr { expr, .. } => match &expr.kind {
                ExprKind::Column {
                    table: None, name, ..
                } => out.push(column_expr(name.clone())),
                ExprKind::Column {
                    table: Some(table),
                    name,
                    ..
                } => out.push(column_expr(format!("{table}.{name}"))),
                _ => out.push(expr.clone()),
            },
            ResultColumn::Star => out.extend(schema.columns.iter().cloned().map(column_expr)),
            ResultColumn::TableStar { table } => {
                if !table.eq_ignore_ascii_case(&schema.name) {
                    return Err(CodegenError::Unsupported {
                        reason: format!("`{table}.*` refers to an unknown table"),
                    });
                }
                out.extend(schema.columns.iter().cloned().map(column_expr));
            }
        }
    }
    Ok(out)
}

/// [`joins_of`], generalized to a bare `Option<&FromClause>` -- every
/// compound-arm caller here already has the `FromClause` in hand rather
/// than a whole [`Select`]/[`CompoundSelect`] to re-borrow one out of.
fn joins_of_from(from: Option<&FromClause>) -> &[crate::parser::ast::Join] {
    from.map_or(&[], |from| &from.joins)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::codegen::row::select::compile_select_with_catalog;
    use crate::codegen::row::TableSchema;
    use crate::vm::row::{execute, InMemoryCursor, Value, Vm};

    fn table(name: &str, root_page: u32, columns: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: vec![String::new(); columns.len()],
            rowid_alias: None,
            root_page,
            indexes: Vec::new(),
            ..Default::default()
        }
    }

    fn catalog() -> Vec<TableSchema> {
        vec![table("t1", 2, &["a"]), table("t2", 3, &["b"])]
    }

    // Every arm allocates its own fresh cursor slot (db-core#182: a
    // program opens its own cursors), so unlike a single-table `SELECT`
    // the real-table slots aren't fixed -- find each `OpenRead`'s p2
    // (the table's root page, distinct per catalog table) and open the
    // matching data onto its p1 slot instead of assuming 0/1.
    fn run(sql: &str, t1_rows: Vec<Vec<Value>>, t2_rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        let program =
            compile_select_with_catalog(&catalog(), &crate::codegen::row::testutil::select(sql))
                .unwrap();
        let mut vm = Vm::new();
        for instr in &program.instructions {
            if instr.opcode == Opcode::OpenRead {
                let rows = match instr.p2 {
                    2 => t1_rows.clone(),
                    3 => t2_rows.clone(),
                    other => panic!("unexpected root page {other}"),
                };
                vm.open_cursor(instr.p1, Box::new(InMemoryCursor::new(rows)))
                    .unwrap();
            }
        }
        execute(&mut vm, &program).unwrap()
    }

    #[test]
    fn union_all_keeps_duplicates_across_both_arms() {
        let rows = run(
            "SELECT a FROM t1 UNION ALL SELECT b FROM t2",
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![vec![Value::Integer(2)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn union_dedups_across_both_arms_and_within_an_arm() {
        let rows = run(
            "SELECT a FROM t1 UNION SELECT b FROM t2",
            vec![vec![Value::Integer(1)], vec![Value::Integer(1)]],
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn three_way_union_all_chains_every_arm() {
        let rows = run(
            "SELECT a FROM t1 UNION ALL SELECT b FROM t2 UNION ALL SELECT a FROM t1",
            vec![vec![Value::Integer(1)]],
            vec![vec![Value::Integer(9)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(9)],
                vec![Value::Integer(1)],
            ]
        );
    }

    #[test]
    fn mixed_union_and_union_all_is_unsupported() {
        let err = compile_select_with_catalog(
            &catalog(),
            &crate::codegen::row::testutil::select(
                "SELECT a FROM t1 UNION SELECT b FROM t2 UNION ALL SELECT a FROM t1",
            ),
        )
        .unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("mixing UNION"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn arity_mismatch_across_arms_is_unsupported() {
        // t1 has two columns, t2 one -- `SELECT *` over each arm makes
        // that mismatch concrete.
        let catalog = vec![table("t1", 2, &["a", "c"]), table("t2", 3, &["b"])];
        let err = compile_select_with_catalog(
            &catalog,
            &crate::codegen::row::testutil::select("SELECT * FROM t1 UNION ALL SELECT b FROM t2"),
        )
        .unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("same number of columns"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn order_by_over_a_compound_select_is_unsupported() {
        let err = compile_select_with_catalog(
            &catalog(),
            &crate::codegen::row::testutil::select(
                "SELECT a FROM t1 UNION ALL SELECT b FROM t2 ORDER BY a",
            ),
        )
        .unwrap_err();
        assert!(matches!(err, CodegenError::Unsupported { .. }), "{err:?}");
    }
}
