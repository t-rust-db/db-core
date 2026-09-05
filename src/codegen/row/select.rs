//! `SELECT` codegen -- see `super`'s module doc.
//!
//! **Scoped to single-table scan + projection + `WHERE` + `LIMIT`**
//! (db-core#92's first slice, recorded in that issue's comments):
//! joins, `ORDER BY`, `GROUP BY`/aggregation, and `DISTINCT` are
//! deferred -- joins and `ORDER BY` to #102 (mechanical single-join/
//! sorter execution, no cost model), the join-order/access-path chooser
//! and `FULL OUTER` to #101 (needs `planner::Stats`, which db-core
//! doesn't have yet), `GROUP BY`/aggregation to #93.

use super::value::emit_column_read;
use super::{CodegenError, CondTargets, Emitter, RegAlloc, Result, Scope, TableSchema, Target};
use crate::expr::{Query, SelectItem};
use crate::vm::row::{Instruction, Opcode, Program};

/// Compiles `query` (a single-table `SELECT` -- no joins, `ORDER BY`,
/// `GROUP BY`, or `DISTINCT`) against `schema`, scanning the pre-wired
/// cursor slot `cursor`.
pub fn compile_select(schema: &TableSchema, cursor: i32, query: &Query) -> Result<Program> {
    if !query.joins.is_empty() {
        return Err(CodegenError::Unsupported {
            reason: "SELECT with a JOIN is deferred to #102".to_string(),
        });
    }
    if query.order_by.is_some() {
        return Err(CodegenError::Unsupported {
            reason: "ORDER BY is deferred to #102".to_string(),
        });
    }
    if query.distinct {
        return Err(CodegenError::Unsupported {
            reason: "DISTINCT is not yet supported".to_string(),
        });
    }
    if !query.group_by.is_empty() {
        return Err(CodegenError::Unsupported {
            reason: "GROUP BY is deferred to #93".to_string(),
        });
    }

    let mut columns = Vec::with_capacity(query.columns.len());
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => columns.push(name.clone()),
            SelectItem::Star => columns.extend(schema.columns.iter().cloned()),
            SelectItem::Agg(..) | SelectItem::Window(_) => {
                return Err(CodegenError::Unsupported {
                    reason: "aggregate/window SELECT items are deferred to #93".to_string(),
                });
            }
        }
    }

    let scope = Scope::single(schema.clone(), cursor);
    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    let limit_reg = match query.limit {
        Some(limit) => {
            let p1 = i32::try_from(limit).map_err(|_| CodegenError::Unsupported {
                reason: format!("LIMIT {limit} does not fit in a p1 operand"),
            })?;
            let r = reg.alloc();
            em.emit(Instruction::new(Opcode::Integer, p1, r, 0));
            Some(r)
        }
        None => None,
    };

    let end_label = em.new_label();
    let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, cursor, 0, 0));
    em.patch_p2(rewind_addr, end_label);

    let loop_start = em.new_label();
    em.place(loop_start);

    let row_skip = em.new_label();
    if let Some(where_expr) = &query.where_clause {
        super::compile_cond(
            &mut em,
            &mut reg,
            &scope,
            where_expr,
            // `WHERE` is where SQL's three-valued logic collapses to
            // two: an unknown predicate excludes the row exactly like
            // a false one.
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(row_skip)),
        )?;
    }

    if let Some(limit_reg) = limit_reg {
        emit_limit_guard(&mut em, limit_reg, end_label);
    }

    let (first, count) = compile_row_values(&mut em, &mut reg, &scope, &columns)?;
    em.emit(Instruction::new(
        Opcode::ResultRow,
        first,
        i32::try_from(count).map_err(|_| CodegenError::Unsupported {
            reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
        })?,
        0,
    ));

    em.place(row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, cursor, 0, 0));
    em.patch_p2(next_addr, loop_start);

    em.place(end_label);
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));

    Ok(em.finish())
}

/// Emits the `LIMIT` stop-guard: called once per row, before emitting
/// it. `IfNotZero` decrements `limit_reg` only while it's positive and
/// jumps whenever it's nonzero -- a negative `LIMIT` (the "no limit"
/// convention) never reaches zero and always takes that jump, staying
/// unbounded -- so the `Goto` below is reached only when `limit_reg`
/// has hit exactly zero, stopping the scan before this row is emitted.
/// Checking before emitting (not after) matters for `LIMIT 0`: an
/// after-the-fact check would let the first row escape before the
/// guard ever ran.
fn emit_limit_guard(em: &mut Emitter, limit_reg: i32, end_label: super::Label) {
    let has_budget_addr = em.emit(Instruction::new(Opcode::IfNotZero, limit_reg, 0, 0));
    let stop_addr = em.emit(Instruction::new(Opcode::Goto, 0, 0, 0));
    em.patch_p2(stop_addr, end_label);
    let continue_label = em.new_label();
    em.patch_p2(has_budget_addr, continue_label);
    em.place(continue_label);
}

/// Compiles each of `names` into a register, returning the contiguous
/// `(first_register, count)` window `Opcode::ResultRow` reads. Columns
/// land in freshly allocated, and therefore already-adjacent, registers
/// by construction -- the contiguity check/`Copy`-based fallback mirrors
/// [`super::value::compile_value_depth`]'s `FunctionCall` handling
/// defensively rather than assuming it can never trip.
fn compile_row_values(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    names: &[String],
) -> Result<(i32, usize)> {
    if names.is_empty() {
        return Ok((reg.alloc(), 0));
    }
    let mut regs = Vec::with_capacity(names.len());
    for name in names {
        let (cursor, idx) = scope.resolve(name)?;
        let r = reg.alloc();
        emit_column_read(em, &scope.schema, cursor, idx, r)?;
        regs.push(r);
    }
    let first = regs[0];
    let already_contiguous = regs
        .iter()
        .enumerate()
        .all(|(i, &r)| r == first.saturating_add(i32::try_from(i).unwrap_or(i32::MAX)));
    if already_contiguous {
        return Ok((first, regs.len()));
    }
    let dests: Vec<i32> = (0..regs.len()).map(|_| reg.alloc()).collect();
    for (&r, &dest) in regs.iter().zip(&dests) {
        em.emit(Instruction::new(Opcode::Copy, r, dest, 0));
    }
    Ok((dests[0], dests.len()))
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
    use crate::expr::{BinOp, Expr};
    use crate::types::Literal;
    use crate::vm::row::{execute, InMemoryCursor, Value, Vm};

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

    fn base_query(columns: Vec<SelectItem>) -> Query {
        Query {
            columns,
            from: "t".into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            order_by: None,
            limit: None,
        }
    }

    fn run(schema: &TableSchema, query: &Query, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        let program = compile_select(schema, 0, query).unwrap();
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(rows)))
            .unwrap();
        execute(&mut vm, &program).unwrap()
    }

    #[test]
    fn scans_every_row_projecting_selected_columns() {
        let schema = schema(&["a", "b"]);
        let query = base_query(vec![SelectItem::Column("b".into())]);
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
        );
        assert_eq!(
            rows,
            vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
        );
    }

    #[test]
    fn star_expands_to_every_schema_column_in_order() {
        let schema = schema(&["a", "b"]);
        let query = base_query(vec![SelectItem::Star]);
        let rows = run(
            &schema,
            &query,
            vec![vec![Value::Integer(1), Value::Integer(10)]],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(10)]]);
    }

    #[test]
    fn where_clause_filters_rows() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.where_clause = Some(Expr::BinaryOp(
            Box::new(Expr::Column("a".into())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(1))),
        ));
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
    }

    #[test]
    fn limit_stops_the_scan_early() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.limit = Some(2);
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn limit_zero_emits_no_rows() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.limit = Some(0);
        let rows = run(&schema, &query, vec![vec![Value::Integer(1)]]);
        assert!(rows.is_empty());
    }

    #[test]
    fn empty_table_scans_zero_rows() {
        let schema = schema(&["a"]);
        let query = base_query(vec![SelectItem::Column("a".into())]);
        let rows = run(&schema, &query, vec![]);
        assert!(rows.is_empty());
    }

    #[test]
    fn join_is_unsupported() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.joins = vec![crate::expr::Join {
            kind: crate::expr::JoinKind::Inner,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn order_by_is_unsupported() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: false,
        });
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn aggregate_select_item_is_unsupported() {
        let schema = schema(&["a"]);
        let query = base_query(vec![SelectItem::Agg(crate::expr::AggFunc::Count, None)]);
        assert!(matches!(
            compile_select(&schema, 0, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }
}
