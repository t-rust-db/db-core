//! `Insert` AST -> `Program` compilation -- see `super`'s module doc.
//!
//! One `MakeRecord`/`Insert`/index-maintenance sequence per `VALUES`
//! row. A column not named in `Insert::columns` (or, for the
//! `INSERT INTO t VALUES (...)` shorthand, every column the row's
//! `VALUES` tuple doesn't cover) stores `NULL` -- no declared
//! `DEFAULT`, since `TableSchema` doesn't carry one yet. The
//! `INTEGER PRIMARY KEY` rowid-alias column, if provided, supplies the
//! row's actual rowid (its own register in the record stays `NULL`,
//! matching every other read path's convention); otherwise
//! `Opcode::NewRowid` generates one.

use super::super::index_maintenance::{emit_index_key_ops_from_regs, open_index_cursors};
use super::super::{CodegenError, Emitter, RegAlloc, Result, Scope, TableSchema};
use super::{FIRST_INDEX_CURSOR, TABLE_CURSOR};
use crate::expr::Insert;
use crate::vm::row::{Instruction, Opcode, Program};

/// Compiles `insert` against `schema` (the resolved target table) into
/// a `Program`.
pub fn compile_insert(schema: &TableSchema, insert: &Insert) -> Result<Program> {
    if !schema.name.eq_ignore_ascii_case(&insert.table) {
        return Err(CodegenError::Unsupported {
            reason: format!(
                "INSERT targets table {}, but the given schema is for {}",
                insert.table, schema.name
            ),
        });
    }

    let target_columns: Vec<usize> = if insert.columns.is_empty() {
        (0..schema.columns.len()).collect()
    } else {
        insert
            .columns
            .iter()
            .map(|name| {
                schema
                    .column_index(name)
                    .ok_or_else(|| CodegenError::UnknownColumn(name.clone()))
            })
            .collect::<Result<_>>()?
    };

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::new(Opcode::OpenWrite, TABLE_CURSOR, 0, 0));
    open_index_cursors(&mut em, schema, FIRST_INDEX_CURSOR);

    let scope = Scope::single(schema.clone(), TABLE_CURSOR);

    for row in &insert.values {
        if row.len() != target_columns.len() {
            return Err(CodegenError::Unsupported {
                reason: format!(
                    "INSERT row has {} value(s), expected {}",
                    row.len(),
                    target_columns.len()
                ),
            });
        }

        let mut provided: Vec<Option<i32>> = vec![None; schema.columns.len()];
        for (value_expr, &col_idx) in row.iter().zip(&target_columns) {
            let r = super::super::compile_value(&mut em, &mut reg, &scope, value_expr)?;
            provided[col_idx] = Some(r);
        }

        let explicit_rowid = schema.rowid_alias.and_then(|idx| provided[idx]);

        let mut col_regs = Vec::with_capacity(schema.columns.len());
        for (idx, src) in provided.iter().enumerate() {
            let dest = reg.alloc();
            if Some(idx) == schema.rowid_alias {
                em.emit(Instruction::new(Opcode::Null, 0, dest, dest));
            } else {
                match src {
                    Some(src) => {
                        em.emit(Instruction::new(Opcode::Copy, *src, dest, 0));
                    }
                    None => {
                        em.emit(Instruction::new(Opcode::Null, 0, dest, dest));
                    }
                }
            }
            col_regs.push(dest);
        }

        let record_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::MakeRecord,
            col_regs[0],
            i32::try_from(col_regs.len()).map_err(|_| CodegenError::Unsupported {
                reason: format!(
                    "INSERT row of {} columns does not fit in a p2 operand",
                    col_regs.len()
                ),
            })?,
            record_reg,
        ));

        let rowid_reg = match explicit_rowid {
            Some(r) => r,
            None => {
                let r = reg.alloc();
                em.emit(Instruction::new(Opcode::NewRowid, TABLE_CURSOR, r, 0));
                r
            }
        };

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
    }

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
    use crate::expr::Expr;
    use crate::expr::{Insert, Query, SelectItem};
    use crate::types::Literal;
    use crate::vm::row::{execute, EphemeralTableCursor, Value, Vm};

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

    /// Runs `insert` against a fresh in-memory table (cursor 0, plus one
    /// cursor per `schema.indexes` starting at 1), then scans the table
    /// back (a second program, same `Vm`/cursor) to assert what landed.
    fn run_insert_then_scan(schema: &TableSchema, insert: &Insert) -> Vec<Vec<Value>> {
        let program = compile_insert(schema, insert).unwrap();
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(EphemeralTableCursor::new()))
            .unwrap();
        for i in 0..schema.indexes.len() {
            vm.open_cursor(
                i32::try_from(i + 1).unwrap(),
                Box::new(EphemeralTableCursor::new()),
            )
            .unwrap();
        }
        execute(&mut vm, &program).unwrap();

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
        };
        let select_program = compile_select(schema, 0, &query).unwrap();
        execute(&mut vm, &select_program).unwrap()
    }

    #[test]
    fn inserts_a_single_row_with_all_columns() {
        let schema = schema(&["a", "b"]);
        let insert = Insert {
            table: "t".into(),
            columns: vec![],
            values: vec![vec![
                Expr::Literal(Literal::Int(1)),
                Expr::Literal(Literal::Int(2)),
            ]],
        };
        let rows = run_insert_then_scan(&schema, &insert);
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
    }

    #[test]
    fn inserts_multiple_rows_in_order() {
        let schema = schema(&["a"]);
        let insert = Insert {
            table: "t".into(),
            columns: vec![],
            values: vec![
                vec![Expr::Literal(Literal::Int(1))],
                vec![Expr::Literal(Literal::Int(2))],
            ],
        };
        let rows = run_insert_then_scan(&schema, &insert);
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn column_list_leaves_unnamed_columns_null() {
        let schema = schema(&["a", "b"]);
        let insert = Insert {
            table: "t".into(),
            columns: vec!["b".into()],
            values: vec![vec![Expr::Literal(Literal::Int(9))]],
        };
        let rows = run_insert_then_scan(&schema, &insert);
        assert_eq!(rows, vec![vec![Value::Null, Value::Integer(9)]]);
    }

    #[test]
    fn wrong_table_name_is_rejected() {
        let schema = schema(&["a"]);
        let insert = Insert {
            table: "other".into(),
            columns: vec![],
            values: vec![vec![Expr::Literal(Literal::Int(1))]],
        };
        assert!(compile_insert(&schema, &insert).is_err());
    }

    #[test]
    fn mismatched_value_count_is_rejected() {
        let schema = schema(&["a", "b"]);
        let insert = Insert {
            table: "t".into(),
            columns: vec![],
            values: vec![vec![Expr::Literal(Literal::Int(1))]],
        };
        assert!(compile_insert(&schema, &insert).is_err());
    }

    #[test]
    fn maintains_a_secondary_index() {
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
                Expr::Literal(Literal::Int(2)),
            ]],
        };
        let program = compile_insert(&schema, &insert).unwrap();
        let mut vm = Vm::new();
        vm.open_cursor(0, Box::new(EphemeralTableCursor::new()))
            .unwrap();
        vm.open_cursor(1, Box::new(EphemeralTableCursor::new()))
            .unwrap();
        execute(&mut vm, &program).unwrap();
    }

    #[test]
    fn explicit_rowid_alias_value_is_used_as_the_rowid() {
        let mut schema = schema(&["id", "b"]);
        schema.rowid_alias = Some(0);
        let insert = Insert {
            table: "t".into(),
            columns: vec![],
            values: vec![vec![
                Expr::Literal(Literal::Int(42)),
                Expr::Literal(Literal::Int(2)),
            ]],
        };
        let rows = run_insert_then_scan(&schema, &insert);
        // Reading the rowid-alias column back yields the rowid itself.
        assert_eq!(rows, vec![vec![Value::Integer(42), Value::Integer(2)]]);
    }
}
