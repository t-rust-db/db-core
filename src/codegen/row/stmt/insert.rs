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
use super::super::{
    valid_table_root_page, CodegenError, Emitter, RegAlloc, Result, Scope, TableSchema,
};
use super::{FIRST_INDEX_CURSOR, TABLE_CURSOR};
use crate::parser::ast::{Insert, InsertSource};
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

    // The AST distinguishes "no column list given" (`None`) from an
    // explicit list, where `expr::Insert` used an empty `Vec` for both.
    let target_columns: Vec<usize> = match &insert.columns {
        None => (0..schema.columns.len()).collect(),
        Some(names) => names
            .iter()
            .map(|name| {
                schema
                    .column_index(name)
                    .ok_or_else(|| CodegenError::UnknownColumn(name.clone()))
            })
            .collect::<Result<_>>()?,
    };

    // `INSERT ... SELECT` and `DEFAULT VALUES` have no `expr::Insert`
    // equivalent and so have never had codegen (#147).
    let rows = match &insert.source {
        InsertSource::Values(rows) => rows,
        InsertSource::Select(_) => {
            return Err(CodegenError::Unsupported {
                reason: "INSERT ... SELECT is not supported by codegen::row yet".to_string(),
            })
        }
        InsertSource::DefaultValues => {
            return Err(CodegenError::Unsupported {
                reason: "INSERT ... DEFAULT VALUES is not supported by codegen::row yet"
                    .to_string(),
            })
        }
    };

    let mut em = Emitter::new();
    let mut reg = RegAlloc::new();

    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::new(
        Opcode::OpenWrite,
        TABLE_CURSOR,
        valid_table_root_page(schema)?,
        0,
    ));
    open_index_cursors(&mut em, schema, FIRST_INDEX_CURSOR)?;

    let scope = Scope::single(schema.clone(), TABLE_CURSOR);

    for row in rows {
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
            let slot = provided
                .get_mut(col_idx)
                .ok_or_else(|| CodegenError::Unsupported {
                    reason: format!(
                        "INSERT column index {col_idx} is out of range for table {}",
                        schema.name
                    ),
                })?;
            *slot = Some(r);
        }

        let explicit_rowid = schema
            .rowid_alias
            .and_then(|idx| provided.get(idx).copied().flatten());

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

        let first_col_reg = col_regs
            .first()
            .copied()
            .ok_or_else(|| CodegenError::Unsupported {
                reason: format!("INSERT into table {} which has no columns", schema.name),
            })?;
        let record_reg = reg.alloc();
        em.emit(Instruction::new(
            Opcode::MakeRecord,
            first_col_reg,
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
    use crate::codegen::row::testutil::{insert, select};
    use crate::codegen::row::{compile_select, IndexSchema};
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

        let select_query = select(&format!("SELECT * FROM {}", schema.name));
        let select_program = compile_select(schema, 0, &select_query).unwrap();
        execute(&mut vm, &select_program).unwrap()
    }

    #[test]
    fn inserts_a_single_row_with_all_columns() {
        let schema = schema(&["a", "b"]);
        let stmt = insert("INSERT INTO t VALUES (1, 2)");
        let rows = run_insert_then_scan(&schema, &stmt);
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
    }

    #[test]
    fn inserts_multiple_rows_in_order() {
        let schema = schema(&["a"]);
        let stmt = insert("INSERT INTO t VALUES (1), (2)");
        let rows = run_insert_then_scan(&schema, &stmt);
        assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    #[test]
    fn column_list_leaves_unnamed_columns_null() {
        let schema = schema(&["a", "b"]);
        let stmt = insert("INSERT INTO t (b) VALUES (9)");
        let rows = run_insert_then_scan(&schema, &stmt);
        assert_eq!(rows, vec![vec![Value::Null, Value::Integer(9)]]);
    }

    #[test]
    fn wrong_table_name_is_rejected() {
        let schema = schema(&["a"]);
        let stmt = insert("INSERT INTO other VALUES (1)");
        assert!(compile_insert(&schema, &stmt).is_err());
    }

    #[test]
    fn mismatched_value_count_is_rejected() {
        let schema = schema(&["a", "b"]);
        let stmt = insert("INSERT INTO t VALUES (1)");
        assert!(compile_insert(&schema, &stmt).is_err());
    }

    #[test]
    fn maintains_a_secondary_index() {
        let mut schema = schema(&["a", "b"]);
        schema.indexes.push(IndexSchema {
            name: "idx_b".into(),
            root_page: 3,
            columns: vec!["b".into()],
        });
        let stmt = insert("INSERT INTO t VALUES (1, 2)");
        let program = compile_insert(&schema, &stmt).unwrap();
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
        let stmt = insert("INSERT INTO t VALUES (42, 2)");
        let rows = run_insert_then_scan(&schema, &stmt);
        // Reading the rowid-alias column back yields the rowid itself.
        assert_eq!(rows, vec![vec![Value::Integer(42), Value::Integer(2)]]);
    }

    /// `INSERT ... SELECT` and `DEFAULT VALUES` are constructs
    /// `expr::Insert` could never represent, so codegen has never
    /// compiled them (#147). Failing soft with the construct named
    /// beats a panic once real INSERT statements start reaching this
    /// planner (#148).
    #[test]
    fn insert_select_and_default_values_are_unsupported() {
        let schema = schema(&["a"]);
        for sql in [
            "INSERT INTO t SELECT a FROM t",
            "INSERT INTO t DEFAULT VALUES",
        ] {
            let stmt = insert(sql);
            assert!(
                matches!(
                    compile_insert(&schema, &stmt),
                    Err(CodegenError::Unsupported { .. })
                ),
                "{sql:?} should be reported as unsupported"
            );
        }
    }
}
