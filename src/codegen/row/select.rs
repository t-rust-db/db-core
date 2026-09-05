//! `SELECT` codegen -- see `super`'s module doc.
//!
//! **Single-table scan + projection + `WHERE` + `LIMIT`** (db-core#92),
//! **a single `INNER`/`LEFT` equi-join and `ORDER BY` via the existing
//! single-key sorter** (db-core#102, both without any stats/access-path
//! cost model). `GROUP BY`/aggregation is deferred to #93; the
//! join-order/access-path chooser, `planner::Stats`, `FULL OUTER`,
//! N-way joins and multi-table catalogs are deferred to #101; `DISTINCT`
//! is not yet supported.

use super::value::emit_column_read;
use super::{
    CodegenError, CondTargets, Emitter, Label, RegAlloc, Result, Scope, TableSchema, Target,
};
use crate::expr::{BinOp, Expr, Join, JoinKind, Query, SelectItem};
use crate::vm::row::{Collation, Instruction, Opcode, Program, SortKeyColumn, P4};

/// Compiles `query` (a single-table `SELECT`, no `JOIN`) against
/// `schema`, scanning the pre-wired cursor slot `cursor`. A query with a
/// `JOIN` must use [`compile_select_join`] instead, since resolving it
/// needs a second pre-wired cursor this signature has no room for.
pub fn compile_select(schema: &TableSchema, cursor: i32, query: &Query) -> Result<Program> {
    if !query.joins.is_empty() {
        return Err(CodegenError::Unsupported {
            reason: "SELECT with a JOIN must be compiled via compile_select_join".to_string(),
        });
    }
    compile_select_inner(schema, cursor, None, query)
}

/// Compiles `query` (a `SELECT` with a single `INNER`/`LEFT` equi-join)
/// against `schema`/`cursor` (the `FROM` table) joined to
/// `right_schema`/`right_cursor` (`query.joins[0].table`), both pre-wired
/// cursor slots. `Right`/`Full`/`Cross` joins, and more than one `JOIN`,
/// are deferred to #101 (they need `planner::Stats` and a multi-table
/// catalog `Scope` this crate doesn't have yet).
pub fn compile_select_join(
    schema: &TableSchema,
    cursor: i32,
    right_schema: &TableSchema,
    right_cursor: i32,
    query: &Query,
) -> Result<Program> {
    compile_select_inner(schema, cursor, Some((right_schema, right_cursor)), query)
}

fn compile_select_inner(
    schema: &TableSchema,
    cursor: i32,
    right: Option<(&TableSchema, i32)>,
    query: &Query,
) -> Result<Program> {
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
    if query.joins.len() > 1 {
        return Err(CodegenError::Unsupported {
            reason: "only a single JOIN is supported; N-way joins are deferred to #101".to_string(),
        });
    }
    let join = query.joins.first();
    match (join, right) {
        (Some(join), Some(_)) => {
            if !matches!(join.kind, JoinKind::Inner | JoinKind::Left) {
                return Err(CodegenError::Unsupported {
                    reason: "only INNER/LEFT JOIN are supported; the join-order/access-path \
chooser and FULL OUTER are deferred to #101"
                        .to_string(),
                });
            }
        }
        (Some(_), None) => {
            return Err(CodegenError::Unsupported {
                reason: "SELECT with a JOIN must be compiled via compile_select_join".to_string(),
            });
        }
        (None, Some(_)) => {
            return Err(CodegenError::Unsupported {
                reason: "a right cursor was supplied but the query has no JOIN".to_string(),
            });
        }
        (None, None) => {}
    }

    let scope = match right {
        Some((right_schema, right_cursor)) => {
            Scope::join(schema.clone(), cursor, right_schema.clone(), right_cursor)
        }
        None => Scope::single(schema.clone(), cursor),
    };

    let mut columns = Vec::with_capacity(query.columns.len());
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => columns.push(name.clone()),
            SelectItem::Star => {
                columns.extend(schema.columns.iter().cloned());
                if let Some((right_schema, _)) = right {
                    columns.extend(
                        right_schema
                            .columns
                            .iter()
                            .map(|c| format!("{}.{c}", right_schema.name)),
                    );
                }
            }
            SelectItem::Agg(..) | SelectItem::Window(_) => {
                return Err(CodegenError::Unsupported {
                    reason: "aggregate/window SELECT items are deferred to #93".to_string(),
                });
            }
        }
    }

    // When there's an `ORDER BY`, rows are buffered into a sorter instead
    // of being emitted directly, and `LIMIT` applies to the sorted
    // output rather than scan order -- see `sort_key`/`output_count`
    // below and the drain loop at the end of this function. `columns`
    // gains the sort key as an extra, trailing record column when it
    // isn't already part of the projection; `output_count` stays at the
    // original projection width so the drain loop never emits it.
    let output_count = columns.len();
    let sort_key = query.order_by.as_ref().map(|order_by| {
        let index = columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&order_by.column))
            .unwrap_or_else(|| {
                columns.push(order_by.column.clone());
                columns.len() - 1
            });
        SortKeyColumn {
            index,
            descending: order_by.descending,
            collation: Collation::Binary,
            nulls_first: false,
        }
    });

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

    // The sorter cursor uses a slot past every cursor the caller wired
    // up -- `Opcode::SorterOpen` opens it itself at runtime, so it needs
    // no caller-side wiring, just an id that can't collide.
    let sorter_cursor = right.map_or(cursor, |(_, c)| cursor.max(c)) + 1;
    if let Some(key) = sort_key {
        em.emit(Instruction::with_p4(
            Opcode::SorterOpen,
            sorter_cursor,
            0,
            0,
            P4::SortKey(vec![key]),
        ));
    }

    let end_label = em.new_label();
    let outer_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, cursor, 0, 0));
    em.patch_p2(outer_rewind_addr, end_label);

    let outer_loop_start = em.new_label();
    em.place(outer_loop_start);
    let outer_row_skip = em.new_label();

    match (join, right) {
        (Some(join), Some((_, right_cursor))) => {
            compile_join_body(
                &mut em,
                &mut reg,
                &scope,
                join,
                right_cursor,
                query,
                &columns,
                sort_key,
                sorter_cursor,
                limit_reg,
                end_label,
                outer_row_skip,
            )?;
        }
        _ => {
            if let Some(where_expr) = &query.where_clause {
                super::compile_cond(
                    &mut em,
                    &mut reg,
                    &scope,
                    where_expr,
                    // `WHERE` is where SQL's three-valued logic collapses
                    // to two: an unknown predicate excludes the row
                    // exactly like a false one.
                    CondTargets::null_is_false(Target::Fallthrough, Target::Jump(outer_row_skip)),
                )?;
            }
            emit_row(
                &mut em,
                &mut reg,
                &scope,
                &columns,
                None,
                sort_key,
                sorter_cursor,
                limit_reg,
                end_label,
            )?;
        }
    }

    em.place(outer_row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, cursor, 0, 0));
    em.patch_p2(next_addr, outer_loop_start);

    em.place(end_label);

    if sort_key.is_some() {
        let sorter_end_label = em.new_label();
        let sort_addr = em.emit(Instruction::new(Opcode::SorterSort, sorter_cursor, 0, 0));
        em.patch_p2(sort_addr, sorter_end_label);

        let sorter_loop_start = em.new_label();
        em.place(sorter_loop_start);

        if let Some(limit_reg) = limit_reg {
            emit_limit_guard(&mut em, limit_reg, sorter_end_label);
        }

        let mut regs = Vec::with_capacity(output_count);
        for idx in 0..output_count {
            let r = reg.alloc();
            em.emit(Instruction::new(
                Opcode::Column,
                sorter_cursor,
                i32::try_from(idx).map_err(|_| CodegenError::Unsupported {
                    reason: format!("column index {idx} does not fit in a p2 operand"),
                })?,
                r,
            ));
            regs.push(r);
        }
        if output_count > 0 {
            em.emit(Instruction::new(
                Opcode::ResultRow,
                regs[0],
                i32::try_from(output_count).map_err(|_| CodegenError::Unsupported {
                    reason: format!(
                        "SELECT list of {output_count} columns does not fit in a p2 operand"
                    ),
                })?,
                0,
            ));
        } else {
            em.emit(Instruction::new(Opcode::ResultRow, reg.alloc(), 0, 0));
        }

        let sorter_next_addr = em.emit(Instruction::new(Opcode::SorterNext, sorter_cursor, 0, 0));
        em.patch_p2(sorter_next_addr, sorter_loop_start);

        em.place(sorter_end_label);
    }

    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));

    Ok(em.finish())
}

/// Strips an optional `table.` qualifier off `name`.
fn unqualified(name: &str) -> &str {
    match name.find('.') {
        Some(idx) => &name[idx + 1..],
        None => name,
    }
}

/// Compiles the inner-join loop for `join` against `right_cursor`,
/// nested inside the already-open outer scan. Handles `LEFT`'s
/// null-extension: a `matched` flag tracks whether the `ON` condition
/// (not `WHERE`) matched any inner row, and a null-extended row is
/// emitted once, after the inner loop, when it never did.
#[allow(clippy::too_many_arguments)]
fn compile_join_body(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    join: &Join,
    right_cursor: i32,
    query: &Query,
    columns: &[String],
    sort_key: Option<SortKeyColumn>,
    sorter_cursor: i32,
    limit_reg: Option<i32>,
    end_label: Label,
    outer_row_skip: Label,
) -> Result<()> {
    let matched_reg = if join.kind == JoinKind::Left {
        let r = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, r, 0));
        Some(r)
    } else {
        None
    };

    let inner_end_label = em.new_label();
    let inner_rewind_addr = em.emit(Instruction::new(Opcode::Rewind, right_cursor, 0, 0));
    em.patch_p2(inner_rewind_addr, inner_end_label);

    let inner_loop_start = em.new_label();
    em.place(inner_loop_start);
    let inner_row_skip = em.new_label();

    // `left_col`/`right_col` name which table they belong to
    // structurally (the `Join` type's own contract), unlike a bare
    // `Expr::Column` elsewhere in the query -- so each is qualified
    // explicitly here rather than resolved via `Scope`'s
    // unqualified-defaults-to-left convention, which would otherwise
    // send an unqualified `right_col` to the wrong table.
    let right_table_name = scope
        .right
        .as_ref()
        .map(|(right_schema, _)| right_schema.name.clone())
        .unwrap_or_default();
    let join_cond = Expr::BinaryOp(
        Box::new(Expr::Column(format!(
            "{}.{}",
            scope.schema.name,
            unqualified(&join.left_col)
        ))),
        BinOp::Eq,
        Box::new(Expr::Column(format!(
            "{right_table_name}.{}",
            unqualified(&join.right_col)
        ))),
    );
    super::compile_cond(
        em,
        reg,
        scope,
        &join_cond,
        CondTargets::null_is_false(Target::Fallthrough, Target::Jump(inner_row_skip)),
    )?;

    if let Some(m) = matched_reg {
        em.emit(Instruction::new(Opcode::Integer, 1, m, 0));
    }

    if let Some(where_expr) = &query.where_clause {
        super::compile_cond(
            em,
            reg,
            scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(inner_row_skip)),
        )?;
    }

    emit_row(
        em,
        reg,
        scope,
        columns,
        None,
        sort_key,
        sorter_cursor,
        limit_reg,
        end_label,
    )?;

    em.place(inner_row_skip);
    let inner_next_addr = em.emit(Instruction::new(Opcode::Next, right_cursor, 0, 0));
    em.patch_p2(inner_next_addr, inner_loop_start);
    em.place(inner_end_label);

    if let Some(m) = matched_reg {
        let zero_reg = reg.alloc();
        em.emit(Instruction::new(Opcode::Integer, 0, zero_reg, 0));
        let eq_addr = em.emit(Instruction::new(Opcode::Eq, m, 0, zero_reg));
        let null_ext_label = em.new_label();
        em.patch_p2(eq_addr, null_ext_label);
        em.goto(outer_row_skip);

        em.place(null_ext_label);
        emit_row(
            em,
            reg,
            scope,
            columns,
            Some(right_cursor),
            sort_key,
            sorter_cursor,
            limit_reg,
            end_label,
        )?;
    }

    Ok(())
}

/// Emits one output row: either directly via `ResultRow` (no `ORDER BY`,
/// applying the `LIMIT` guard first), or into the sorter via
/// `MakeRecord`/`SorterInsert` (an `ORDER BY` is present, so `LIMIT`
/// applies later, during the post-sort drain). `null_cursor`, when set,
/// null-fills every column that would otherwise be read from it -- used
/// for `LEFT` join's unmatched-row null-extension.
#[allow(clippy::too_many_arguments)]
fn emit_row(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    columns: &[String],
    null_cursor: Option<i32>,
    sort_key: Option<SortKeyColumn>,
    sorter_cursor: i32,
    limit_reg: Option<i32>,
    end_label: Label,
) -> Result<()> {
    if sort_key.is_some() {
        let (first, count) = compile_row_values(em, reg, scope, columns, null_cursor)?;
        let blob_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::MakeRecord,
            first,
            i32::try_from(count).map_err(|_| CodegenError::Unsupported {
                reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
            })?,
            blob_reg,
        ));
        em.emit(Instruction::new(
            Opcode::SorterInsert,
            sorter_cursor,
            blob_reg,
            0,
        ));
        return Ok(());
    }

    if let Some(limit_reg) = limit_reg {
        emit_limit_guard(em, limit_reg, end_label);
    }
    let (first, count) = compile_row_values(em, reg, scope, columns, null_cursor)?;
    em.emit(Instruction::new(
        Opcode::ResultRow,
        first,
        i32::try_from(count).map_err(|_| CodegenError::Unsupported {
            reason: format!("SELECT list of {count} columns does not fit in a p2 operand"),
        })?,
        0,
    ));
    Ok(())
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
///
/// `null_cursor`, when set, null-fills every column resolved to that
/// cursor instead of reading it -- `LEFT` join's unmatched-row
/// null-extension, where the right cursor holds no current row to read.
fn compile_row_values(
    em: &mut Emitter,
    reg: &mut RegAlloc,
    scope: &Scope,
    names: &[String],
    null_cursor: Option<i32>,
) -> Result<(i32, usize)> {
    if names.is_empty() {
        return Ok((reg.alloc(), 0));
    }
    let mut regs = Vec::with_capacity(names.len());
    for name in names {
        let (cursor, idx) = scope.resolve(name)?;
        let r = reg.alloc();
        if Some(cursor) == null_cursor {
            em.emit(Instruction::new(Opcode::Null, 0, r, 0));
        } else {
            let table_schema = match &scope.right {
                Some((right_schema, right_cursor)) if cursor == *right_cursor => right_schema,
                _ => &scope.schema,
            };
            emit_column_read(em, table_schema, cursor, idx, r)?;
        }
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
        schema_named("t", columns)
    }

    fn schema_named(name: &str, columns: &[&str]) -> TableSchema {
        TableSchema {
            name: name.into(),
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

    fn run_join(
        schema: &TableSchema,
        right_schema: &TableSchema,
        query: &Query,
        left_rows: Vec<Vec<Value>>,
        right_rows: Vec<Vec<Value>>,
    ) -> Vec<Vec<Value>> {
        let program = compile_select_join(schema, 0, right_schema, 1, query).unwrap();
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(InMemoryCursor::new(left_rows)))
            .unwrap();
        vm.open_cursor(1, Box::new(InMemoryCursor::new(right_rows)))
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
    fn join_without_a_right_cursor_is_unsupported() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.joins = vec![Join {
            kind: JoinKind::Inner,
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
    fn full_outer_join_is_unsupported() {
        let schema = schema(&["a"]);
        let right = schema_named("u", &["b"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.joins = vec![Join {
            kind: JoinKind::Full,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        assert!(matches!(
            compile_select_join(&schema, 0, &right, 1, &query),
            Err(CodegenError::Unsupported { .. })
        ));
    }

    #[test]
    fn inner_join_matches_rows_on_equi_condition() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Inner,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(3), Value::Integer(300)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
    }

    #[test]
    fn left_join_null_extends_unmatched_rows() {
        let left = schema(&["a"]);
        let right = schema_named("u", &["b", "c"]);
        let mut query = base_query(vec![
            SelectItem::Column("a".into()),
            SelectItem::Column("u.c".into()),
        ]);
        query.joins = vec![Join {
            kind: JoinKind::Left,
            table: "u".into(),
            left_col: "a".into(),
            right_col: "b".into(),
        }];
        let rows = run_join(
            &left,
            &right,
            &query,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
            vec![vec![Value::Integer(1), Value::Integer(100)]],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Integer(100)],
                vec![Value::Integer(2), Value::Null],
            ]
        );
    }

    #[test]
    fn order_by_sorts_rows() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: false,
        });
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ]
        );
    }

    #[test]
    fn order_by_descending_sorts_rows() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: true,
        });
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(2)],
                vec![Value::Integer(1)],
            ]
        );
    }

    #[test]
    fn order_by_column_absent_from_select_list_still_sorts() {
        let schema = schema(&["a", "b"]);
        let mut query = base_query(vec![SelectItem::Column("b".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: false,
        });
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(1), Value::Integer(10)],
            ],
        );
        assert_eq!(
            rows,
            vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
        );
    }

    #[test]
    fn order_by_respects_limit_on_sorted_output() {
        let schema = schema(&["a"]);
        let mut query = base_query(vec![SelectItem::Column("a".into())]);
        query.order_by = Some(crate::expr::OrderBy {
            column: "a".into(),
            descending: false,
        });
        query.limit = Some(2);
        let rows = run(
            &schema,
            &query,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ],
        );
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
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
