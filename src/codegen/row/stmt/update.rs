//! `Update` AST -> `Program` compilation -- see `super`'s module doc.
//! Mirrors [`super::delete`]'s scan shape, but on a match rebuilds the
//! row (assigned columns re-evaluated, the rest re-read unchanged from
//! the current row) and replaces it in place: old index entries
//! removed, `Delete`+`Insert` swap the row, new index entries added.
//!
//! **Scoped down**: reassigning the `INTEGER PRIMARY KEY` rowid-alias
//! column is rejected (`CodegenError::Unsupported`) rather than
//! supported -- doing so safely means moving the row to a new rowid
//! mid-scan, which (unlike an ordinary same-rowid rebuild) a cursor
//! walking rowid order could revisit later in the very same scan. No
//! real b-tree cursor concern here (see `stmt/delete.rs`'s doc comment)
//! justifies the extra complexity for a rare statement shape; ordinary
//! column updates are unaffected.

use super::super::index_maintenance::{
    emit_index_key_ops, emit_index_key_ops_from_regs, open_index_cursors,
};
use super::super::{
    CodegenError, CondTargets, Emitter, RegAlloc, Result, Scope, TableSchema, Target,
};
use super::{FIRST_INDEX_CURSOR, TABLE_CURSOR};
use crate::expr::Update;
use crate::vm::row::{Instruction, Opcode, Program};

/// Compiles `update` against `schema` (the resolved target table) into
/// a `Program`.
pub fn compile_update(schema: &TableSchema, update: &Update) -> Result<Program> {
    if !schema.name.eq_ignore_ascii_case(&update.table) {
        return Err(CodegenError::Unsupported {
            reason: format!(
                "UPDATE targets table {}, but the given schema is for {}",
                update.table, schema.name
            ),
        });
    }

    let mut assigned: Vec<Option<&crate::expr::Expr>> = vec![None; schema.columns.len()];
    for assignment in &update.assignments {
        let idx = schema
            .column_index(&assignment.column)
            .ok_or_else(|| CodegenError::UnknownColumn(assignment.column.clone()))?;
        if Some(idx) == schema.rowid_alias {
            return Err(CodegenError::Unsupported {
                reason: format!(
                    "UPDATE of the rowid-alias column {} is not supported yet",
                    assignment.column
                ),
            });
        }
        assigned[idx] = Some(&assignment.value);
    }

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::new(Opcode::OpenWrite, TABLE_CURSOR, 0, 0));
    open_index_cursors(&mut em, schema, FIRST_INDEX_CURSOR);

    let scope = Scope::single(schema.clone(), TABLE_CURSOR);
    let end_label = em.new_label();
    let rewind_addr = em.emit(Instruction::new(Opcode::Rewind, TABLE_CURSOR, 0, 0));
    em.patch_p2(rewind_addr, end_label);

    let loop_start = em.new_label();
    em.place(loop_start);

    let row_skip = em.new_label();
    if let Some(where_expr) = &update.where_clause {
        super::super::compile_cond(
            &mut em,
            &mut reg,
            &scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(row_skip)),
        )?;
    }

    // Old index entries reference the row's current on-disk values --
    // read them (and remove them) before anything here overwrites a
    // register an unassigned column's `emit_column_read` still needs.
    emit_index_key_ops(
        &mut em,
        &mut reg,
        schema,
        TABLE_CURSOR,
        FIRST_INDEX_CURSOR,
        Opcode::IdxDelete,
    )?;

    let rowid_reg = reg.alloc();
    em.emit(Instruction::new(Opcode::Rowid, TABLE_CURSOR, rowid_reg, 0));

    let mut col_regs = Vec::with_capacity(schema.columns.len());
    for (idx, expr) in assigned.iter().enumerate() {
        let dest = reg.alloc();
        if Some(idx) == schema.rowid_alias {
            em.emit(Instruction::new(Opcode::Null, 0, dest, dest));
        } else if let Some(expr) = expr {
            let value_reg = super::super::compile_value(&mut em, &mut reg, &scope, expr)?;
            em.emit(Instruction::new(Opcode::Copy, value_reg, dest, 0));
        } else {
            super::super::value::emit_column_read(&mut em, schema, TABLE_CURSOR, idx, dest)?;
        }
        col_regs.push(dest);
    }

    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        col_regs[0],
        i32::try_from(col_regs.len()).map_err(|_| CodegenError::Unsupported {
            reason: format!(
                "UPDATE row of {} columns does not fit in a p2 operand",
                col_regs.len()
            ),
        })?,
        record_reg,
    ));

    em.emit(Instruction::new(Opcode::Delete, TABLE_CURSOR, 0, 0));
    em.emit(Instruction::new(
        Opcode::Insert,
        TABLE_CURSOR,
        rowid_reg,
        record_reg,
    ));

    emit_index_key_ops_from_regs(
        &mut em,
        &mut reg,
        schema,
        &col_regs,
        rowid_reg,
        FIRST_INDEX_CURSOR,
    )?;

    em.place(row_skip);
    let next_addr = em.emit(Instruction::new(Opcode::Next, TABLE_CURSOR, 0, 0));
    em.patch_p2(next_addr, loop_start);

    em.place(end_label);
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
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
    use crate::codegen::row::{compile_select, IndexSchema};
    use crate::expr::{Assignment, BinOp, Expr, Query, SelectItem, Update};
    use crate::types::Literal;
    use crate::vm::row::{execute, Cursor, EphemeralTableCursor, Value, Vm};

    fn schema(columns: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".into(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| String::new()).collect(),
            rowid_alias: None,
            root_page: 0,
            indexes: Vec::new(),
        }
    }

    fn seed(schema: &TableSchema, vm: &mut Vm, rows: Vec<(i64, Vec<Value>)>) {
        let mut table = EphemeralTableCursor::new();
        for (rowid, values) in rows {
            table.insert(rowid, values);
        }
        vm.open_cursor(0, Box::new(table)).unwrap();
        for i in 0..schema.indexes.len() {
            vm.open_cursor(
                i32::try_from(i + 1).unwrap(),
                Box::new(EphemeralTableCursor::new()),
            )
            .unwrap();
        }
    }

    fn scan_all(schema: &TableSchema, vm: &mut Vm) -> Vec<Vec<Value>> {
        let query = Query {
            columns: vec![SelectItem::Star],
            from: schema.name.clone(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            order_by: None,
            limit: None,
        };
        let program = compile_select(schema, 0, &query).unwrap();
        execute(vm, &program).unwrap()
    }

    #[test]
    fn updates_matching_rows_and_leaves_others_unchanged() {
        let schema = schema(&["a", "b"]);
        let update = Update {
            table: "t".into(),
            assignments: vec![Assignment {
                column: "b".into(),
                value: Expr::Literal(Literal::Int(99)),
            }],
            where_clause: Some(Expr::BinaryOp(
                Box::new(Expr::Column("a".into())),
                BinOp::Eq,
                Box::new(Expr::Literal(Literal::Int(1))),
            )),
        };
        let program = compile_update(&schema, &update).unwrap();
        let mut vm = Vm::new();
        seed(
            &schema,
            &mut vm,
            vec![
                (1, vec![Value::Integer(1), Value::Integer(10)]),
                (2, vec![Value::Integer(2), Value::Integer(20)]),
            ],
        );
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            scan_all(&schema, &mut vm),
            vec![
                vec![Value::Integer(1), Value::Integer(99)],
                vec![Value::Integer(2), Value::Integer(20)],
            ]
        );
    }

    #[test]
    fn no_where_clause_updates_every_row_exactly_once() {
        let schema = schema(&["a"]);
        let update = Update {
            table: "t".into(),
            assignments: vec![Assignment {
                column: "a".into(),
                value: Expr::BinaryOp(
                    Box::new(Expr::Column("a".into())),
                    BinOp::Add,
                    Box::new(Expr::Literal(Literal::Int(100))),
                ),
            }],
            where_clause: None,
        };
        let program = compile_update(&schema, &update).unwrap();
        let mut vm = Vm::new();
        seed(
            &schema,
            &mut vm,
            vec![(1, vec![Value::Integer(1)]), (2, vec![Value::Integer(2)])],
        );
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            scan_all(&schema, &mut vm),
            vec![vec![Value::Integer(101)], vec![Value::Integer(102)]]
        );
    }

    #[test]
    fn maintains_a_secondary_index_across_the_rebuild() {
        let mut schema = schema(&["a", "b"]);
        schema.indexes.push(IndexSchema {
            name: "idx_b".into(),
            root_page: 0,
            columns: vec!["b".into()],
        });
        let update = Update {
            table: "t".into(),
            assignments: vec![Assignment {
                column: "b".into(),
                value: Expr::Literal(Literal::Int(99)),
            }],
            where_clause: Some(Expr::BinaryOp(
                Box::new(Expr::Column("a".into())),
                BinOp::Eq,
                Box::new(Expr::Literal(Literal::Int(1))),
            )),
        };
        let program = compile_update(&schema, &update).unwrap();

        let insert_program = crate::codegen::row::compile_insert(
            &schema,
            &crate::expr::Insert {
                table: "t".into(),
                columns: vec![],
                values: vec![vec![
                    Expr::Literal(Literal::Int(1)),
                    Expr::Literal(Literal::Int(10)),
                ]],
            },
        )
        .unwrap();

        let mut vm = Vm::new();
        seed(&schema, &mut vm, vec![]);
        execute(&mut vm, &insert_program).unwrap();
        // Fails (`IdxDelete` finds no matching entry for the *old*
        // value) unless `INSERT` built the index entry this `UPDATE`
        // now needs to remove before adding the new one.
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            scan_all(&schema, &mut vm),
            vec![vec![Value::Integer(1), Value::Integer(99)]]
        );
    }

    #[test]
    fn reassigning_the_rowid_alias_column_is_rejected() {
        let mut schema = schema(&["id", "b"]);
        schema.rowid_alias = Some(0);
        let update = Update {
            table: "t".into(),
            assignments: vec![Assignment {
                column: "id".into(),
                value: Expr::Literal(Literal::Int(5)),
            }],
            where_clause: None,
        };
        assert!(compile_update(&schema, &update).is_err());
    }

    #[test]
    fn wrong_table_name_is_rejected() {
        let schema = schema(&["a"]);
        let update = Update {
            table: "other".into(),
            assignments: vec![],
            where_clause: None,
        };
        assert!(compile_update(&schema, &update).is_err());
    }
}
