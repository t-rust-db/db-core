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
    valid_table_root_page, CodegenError, CondTargets, Emitter, RegAlloc, Result, Scope,
    TableSchema, Target,
};
use super::{FIRST_INDEX_CURSOR, TABLE_CURSOR};
use crate::parser::ast::{Expr, Update};
use crate::vm::row::{Instruction, Opcode, Program};

/// Compiles `update` against `schema` (the resolved target table) into
/// a `Program`.
pub fn compile_update(schema: &TableSchema, update: &Update) -> Result<Program> {
    compile_update_with_catalog(schema, update, &[])
}

/// As [`compile_update`], but also resolves `catalog` for any scalar/
/// `IN`/`EXISTS` subquery in `update`'s `WHERE`/assignment expressions
/// that references another table (db-core#206) -- `compile_update`
/// itself just calls through with an empty catalog.
pub fn compile_update_with_catalog(
    schema: &TableSchema,
    update: &Update,
    catalog: &[TableSchema],
) -> Result<Program> {
    if !schema.name.eq_ignore_ascii_case(&update.table) {
        return Err(CodegenError::Unsupported {
            reason: format!(
                "UPDATE targets table {}, but the given schema is for {}",
                update.table, schema.name
            ),
        });
    }

    let mut assigned: Vec<Option<&Expr>> = vec![None; schema.columns.len()];
    for assignment in &update.assignments {
        // `expr::Assignment` was one column per entry. The AST keeps a
        // `Vec<String>` so the tuple form `(a, b) = (x, y)` can expand
        // into one entry per column -- but each entry still pairs with
        // a single RHS expression, so anything other than one column
        // here is a shape this planner has no value to assign.
        let [column] = assignment.columns.as_slice() else {
            return Err(CodegenError::Unsupported {
                reason: "a tuple assignment in UPDATE ... SET is not supported yet".to_string(),
            });
        };
        let idx = schema
            .column_index(column)
            .ok_or_else(|| CodegenError::UnknownColumn(column.clone()))?;
        if Some(idx) == schema.rowid_alias {
            return Err(CodegenError::Unsupported {
                reason: format!("UPDATE of the rowid-alias column {column} is not supported yet"),
            });
        }
        let slot = assigned
            .get_mut(idx)
            .ok_or_else(|| CodegenError::UnknownColumn(column.clone()))?;
        *slot = Some(&assignment.value);
    }

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

    let scope = Scope::single(schema.clone(), TABLE_CURSOR).with_catalog(catalog.to_vec());
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

    // `MakeRecord` below reads `col_regs.len()` *contiguous* registers
    // starting at `col_regs[0]` -- so every dest register is allocated
    // up front, in one unbroken block, before any assigned expression's
    // own (possibly multi-register) evaluation can allocate a register
    // in between and break that contiguity. A single assigned column
    // never triggered this; two or more (e.g. a tuple assignment) did.
    let col_regs: Vec<i32> = (0..schema.columns.len()).map(|_| reg.alloc()).collect();
    for (idx, (expr, &dest)) in assigned.iter().zip(&col_regs).enumerate() {
        if Some(idx) == schema.rowid_alias {
            em.emit(Instruction::new(Opcode::Null, 0, dest, dest));
        } else if let Some(expr) = expr {
            let value_reg = super::super::compile_value(&mut em, &mut reg, &scope, expr)?;
            em.emit(Instruction::new(Opcode::Copy, value_reg, dest, 0));
        } else {
            super::super::value::emit_column_read(&mut em, schema, TABLE_CURSOR, idx, dest)?;
        }
    }

    let first_col_reg = col_regs
        .first()
        .copied()
        .ok_or_else(|| CodegenError::Unsupported {
            reason: format!("UPDATE of table {} which has no columns", schema.name),
        })?;
    let record_reg = reg.alloc();
    em.emit(Instruction::new(
        Opcode::MakeRecord,
        first_col_reg,
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
    use crate::codegen::row::testutil::{select, update};
    use crate::codegen::row::{compile_select, IndexSchema};
    use crate::vm::row::{execute, Cursor, EphemeralTableCursor, Value, Vm};

    fn schema(columns: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".into(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| String::new()).collect(),
            rowid_alias: None,
            root_page: 0,
            indexes: Vec::new(),
            ..Default::default()
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
        let query = select(&format!("SELECT * FROM {}", schema.name));
        let program = compile_select(schema, 0, &query).unwrap();
        execute(vm, &program).unwrap()
    }

    #[test]
    fn updates_matching_rows_and_leaves_others_unchanged() {
        let schema = schema(&["a", "b"]);
        let stmt = update("UPDATE t SET b = 99 WHERE a = 1");
        let program = compile_update(&schema, &stmt).unwrap();
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
        let stmt = update("UPDATE t SET a = a + 100");
        let program = compile_update(&schema, &stmt).unwrap();
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
            root_page: 3,
            unique: false,
            columns: vec![crate::codegen::row::IndexedColumn {
                name: "b".into(),
                ..Default::default()
            }],
        });
        let program = compile_update(&schema, &update("UPDATE t SET b = 99 WHERE a = 1")).unwrap();

        let insert_program = crate::codegen::row::compile_insert(
            &schema,
            &crate::codegen::row::testutil::insert("INSERT INTO t VALUES (1, 10)"),
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
        let stmt = update("UPDATE t SET id = 5");
        assert!(compile_update(&schema, &stmt).is_err());
    }

    #[test]
    fn wrong_table_name_is_rejected() {
        let schema = schema(&["a"]);
        let stmt = update("UPDATE other SET a = 1");
        assert!(compile_update(&schema, &stmt).is_err());
    }

    /// A tuple assignment `(a, b) = (x, y)` looked like a shape
    /// `expr::Assignment` (one column per entry) could never carry, but
    /// the parser already expands it into one single-column `Assignment`
    /// per tuple element (`ast::Assignment`'s own doc comment) -- so it
    /// compiles through the same per-column loop as `SET a = x, b = y`
    /// with no extra codegen needed.
    #[test]
    fn tuple_assignment_expands_to_one_assignment_per_column() {
        let schema = schema(&["a", "b"]);
        let stmt = update("UPDATE t SET (a, b) = (1, 2)");
        let program = compile_update(&schema, &stmt).unwrap();
        let mut vm = Vm::new();
        seed(
            &schema,
            &mut vm,
            vec![(1, vec![Value::Integer(9), Value::Integer(9)])],
        );
        execute(&mut vm, &program).unwrap();
        assert_eq!(
            scan_all(&schema, &mut vm),
            vec![vec![Value::Integer(1), Value::Integer(2)]]
        );
    }
}
