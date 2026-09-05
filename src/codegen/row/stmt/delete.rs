//! `Delete` AST -> `Program` compilation -- see `super`'s module doc.
//! Mirrors [`super::super::select`]'s `Init -> OpenWrite -> Rewind ->
//! [WHERE test] -> Next -> Halt` scan shape, swapping the result-row
//! emission for a per-index `IdxDelete` plus a table `Delete` per
//! matched row.
//!
//! **Scoped down**: no `WHERE rowid = ...` seek fast path (sqlite-rs's
//! own #336) -- every `Program` here does a full table scan, deferred
//! alongside #94's index/range-scan codegen.

use super::super::index_maintenance::{emit_index_key_ops, open_index_cursors};
use super::super::{
    CodegenError, CondTargets, Emitter, RegAlloc, Result, Scope, TableSchema, Target,
};
use super::{FIRST_INDEX_CURSOR, TABLE_CURSOR};
use crate::expr::Delete;
use crate::vm::row::{Instruction, Opcode, Program};

/// Compiles `delete` against `schema` (the resolved target table) into
/// a `Program`.
pub fn compile_delete(schema: &TableSchema, delete: &Delete) -> Result<Program> {
    if !schema.name.eq_ignore_ascii_case(&delete.table) {
        return Err(CodegenError::Unsupported {
            reason: format!(
                "DELETE targets table {}, but the given schema is for {}",
                delete.table, schema.name
            ),
        });
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
    if let Some(where_expr) = &delete.where_clause {
        super::super::compile_cond(
            &mut em,
            &mut reg,
            &scope,
            where_expr,
            CondTargets::null_is_false(Target::Fallthrough, Target::Jump(row_skip)),
        )?;
    }

    emit_index_key_ops(
        &mut em,
        &mut reg,
        schema,
        TABLE_CURSOR,
        FIRST_INDEX_CURSOR,
        Opcode::IdxDelete,
    )?;
    em.emit(Instruction::new(Opcode::Delete, TABLE_CURSOR, 0, 0));

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
    use crate::expr::{BinOp, Delete, Expr, Query, SelectItem};
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
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        };
        let program = compile_select(schema, 0, &query).unwrap();
        execute(vm, &program).unwrap()
    }

    #[test]
    fn deletes_rows_matching_where_clause() {
        let schema = schema(&["a"]);
        let delete = Delete {
            table: "t".into(),
            where_clause: Some(Expr::BinaryOp(
                Box::new(Expr::Column("a".into())),
                BinOp::Eq,
                Box::new(Expr::Literal(Literal::Int(2))),
            )),
        };
        let program = compile_delete(&schema, &delete).unwrap();
        let mut vm = Vm::new();
        seed(
            &schema,
            &mut vm,
            vec![
                (1, vec![Value::Integer(1)]),
                (2, vec![Value::Integer(2)]),
                (3, vec![Value::Integer(3)]),
            ],
        );
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            scan_all(&schema, &mut vm),
            vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]
        );
    }

    #[test]
    fn no_where_clause_deletes_every_row() {
        let schema = schema(&["a"]);
        let delete = Delete {
            table: "t".into(),
            where_clause: None,
        };
        let program = compile_delete(&schema, &delete).unwrap();
        let mut vm = Vm::new();
        seed(
            &schema,
            &mut vm,
            vec![(1, vec![Value::Integer(1)]), (2, vec![Value::Integer(2)])],
        );
        execute(&mut vm, &program).unwrap();
        assert!(scan_all(&schema, &mut vm).is_empty());
    }

    #[test]
    fn removes_secondary_index_entries() {
        use crate::codegen::row::compile_insert;
        use crate::expr::Insert;

        let mut schema = schema(&["a", "b"]);
        schema.indexes.push(IndexSchema {
            name: "idx_b".into(),
            root_page: 0,
            columns: vec!["b".into()],
        });

        let insert = Insert {
            table: "t".into(),
            columns: vec![],
            values: vec![vec![
                Expr::Literal(Literal::Int(1)),
                Expr::Literal(Literal::Int(10)),
            ]],
        };
        let insert_program = compile_insert(&schema, &insert).unwrap();

        let delete = Delete {
            table: "t".into(),
            where_clause: Some(Expr::BinaryOp(
                Box::new(Expr::Column("a".into())),
                BinOp::Eq,
                Box::new(Expr::Literal(Literal::Int(1))),
            )),
        };
        let delete_program = compile_delete(&schema, &delete).unwrap();

        let mut vm = Vm::new();
        seed(&schema, &mut vm, vec![]);
        execute(&mut vm, &insert_program).unwrap();
        // Fails (`IdxDelete` finds no matching entry) unless `INSERT`
        // built the index entry `DELETE` now needs to remove -- the
        // real assertion here is that this doesn't error.
        execute(&mut vm, &delete_program).unwrap();
        assert!(scan_all(&schema, &mut vm).is_empty());
    }

    #[test]
    fn wrong_table_name_is_rejected() {
        let schema = schema(&["a"]);
        let delete = Delete {
            table: "other".into(),
            where_clause: None,
        };
        assert!(compile_delete(&schema, &delete).is_err());
    }
}
