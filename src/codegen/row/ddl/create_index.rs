// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `CreateIndex` AST -> `Program` compilation (#215). A single
//! `Opcode::CreateIndex` instruction allocates the index's root page,
//! populates it with one entry per pre-existing row of the target table,
//! registers the row in `sqlite_master`, and bumps the schema cookie —
//! all at exec time, mirroring `create_table.rs`'s single-opcode shape.
//!
//! `DESC` key columns (#945) travel as per-column flags in the `P4`, so
//! the b-tree is built (and later searched) in the order sqlite3 reads it
//! back in.

use crate::codegen::row::expr::column_index;
use crate::codegen::row::index_maintenance::{
    emit_index_key_columns, index_sort_key, valid_table_root_page, KeySource,
};
use crate::codegen::row::select::CodegenError;
use crate::codegen::row::{Emitter, IndexSchema, IndexedColumn, RegAlloc, TableSchema};
use crate::parser::ast::{CreateIndex, Expr, ExprKind};
use crate::value::Collation;
use crate::vm::row::{Instruction, Opcode, Program, OPFLAG_P2ISREG, P4};

/// Compiles `CREATE INDEX` into a single `Opcode::CreateIndex` instruction
/// that allocates the index's root page, populates it from the target
/// table's existing rows, registers it in `sqlite_master`, and bumps the
/// schema cookie at exec time.
pub fn compile_create_index(
    ci: &CreateIndex,
    schema: &TableSchema,
    source: &str,
) -> Result<Program, CodegenError> {
    if ci.where_clause.is_some() {
        // Built over every row, a partial index would not be the one
        // sqlite3 reads back (it expects only the rows the WHERE keeps).
        return Err(CodegenError::Unsupported {
            reason: format!(
                "partial index {} (CREATE INDEX ... WHERE) isn't supported yet",
                ci.name
            ),
        });
    }
    let mut column_indices = Vec::with_capacity(ci.columns.len());
    let mut key_columns = Vec::with_capacity(ci.columns.len());
    let mut has_expression = false;
    let desc: Vec<bool> = ci.columns.iter().map(|c| c.desc == Some(true)).collect();
    for col in &ci.columns {
        let (inner, collation) = key_term(&col.expr);
        let text = span_text(source, inner.span)?;
        if let crate::parser::ast::ExprKind::Column { name, .. } = &inner.kind {
            let idx = column_index(schema, name).ok_or_else(|| CodegenError::Unsupported {
                reason: format!(
                    "index {} references a column this codegen can't resolve: {}",
                    ci.name, name
                ),
            })?;
            column_indices.push(if Some(idx) == schema.rowid_alias {
                crate::schema::INDEX_KEY_ROWID
            } else {
                idx
            });
            key_columns.push(IndexedColumn {
                name: name.clone(),
                desc: col.desc == Some(true),
                collation,
            });
        } else {
            has_expression = true;
            check_index_expression(&ci.name, inner)?;
            key_columns.push(IndexedColumn {
                name: text,
                desc: col.desc == Some(true),
                collation,
            });
        }
    }

    let sql = span_text(source, ci.span)?;

    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    let mut p4 = P4::CreateIndex {
        name: ci.name.clone(),
        table_name: schema.name.clone(),
        table_root_page: schema.root_page,
        sql,
        column_indices,
        unique: ci.unique,
        desc,
        root_reg: None,
    };
    if !has_expression && !ci.unique {
        em.emit(Instruction::with_p4(Opcode::CreateIndex, 0, 0, 0, p4));
        em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
        return Ok(em.finish());
    }

    // An expression key column is evaluated per row, which only the VM can
    // do, and a UNIQUE index must refuse rows that already collide:
    // `CreateIndex` allocates and registers the index but leaves it empty,
    // and the program fills it -- scan the table, build each row's key,
    // `IdxInsert` (sqlite3's own CREATE INDEX shape; #945).
    let mut reg = RegAlloc::new();
    let root_reg = reg.alloc();
    if let P4::CreateIndex { root_reg: r, .. } = &mut p4 {
        *r = Some(root_reg);
    }
    em.emit(Instruction::with_p4(Opcode::CreateIndex, 0, 0, 0, p4));
    let table_root = valid_table_root_page(schema)?;
    em.emit(Instruction::new(
        Opcode::OpenRead,
        TABLE_CURSOR,
        table_root,
        schema.database_p3(),
    ));
    let index = IndexSchema {
        name: ci.name.clone(),
        unique: ci.unique,
        columns: key_columns,
        root_page: 0,
    };
    let mut open = Instruction::new(Opcode::OpenWrite, INDEX_CURSOR, root_reg, 0);
    open.p5 = 1 | OPFLAG_P2ISREG;
    if let Some(key) = index_sort_key(&index) {
        open.p4 = P4::SortKey(key);
    }
    em.emit(open);
    let end = em.new_label();
    let rewind = em.emit(Instruction::new(Opcode::Rewind, TABLE_CURSOR, 0, 0));
    em.patch_p2(rewind, end);
    let top = em.new_label();
    em.place(top);
    let start = emit_index_key_columns(
        &mut em,
        &mut reg,
        schema,
        &index,
        1,
        &KeySource::Cursor {
            schema: schema.clone(),
            cursor: TABLE_CURSOR,
            record: false,
        },
    )?;
    let count = i32::try_from(index.columns.len()).unwrap_or(0);
    let insert = em.new_label();
    if ci.unique {
        // A NULL key part never collides (NULLs are distinct in a UNIQUE
        // index); otherwise a key already present is sqlite3's error. The
        // slot after the key receives the other row's rowid.
        for offset in 0..count {
            let null = em.emit(Instruction::new(
                Opcode::IsNull,
                start.saturating_add(offset),
                0,
                0,
            ));
            em.patch_p2(null, insert);
        }
        let probe = em.emit(Instruction::with_p4(
            Opcode::NoConflict,
            INDEX_CURSOR,
            0,
            start,
            P4::Int(i64::from(count)),
        ));
        em.patch_p2(probe, insert);
        let message = if has_expression {
            format!("UNIQUE constraint failed: index '{}'", ci.name)
        } else {
            crate::codegen::row::index_maintenance::unique_violation_message(schema, &index)
        };
        em.emit(Instruction::with_p4(
            Opcode::Halt,
            crate::codegen::row::insert::SQLITE_CONSTRAINT_UNIQUE,
            0,
            0,
            P4::Str(message),
        ));
    }
    em.place(insert);
    em.emit(Instruction::new(
        Opcode::Rowid,
        TABLE_CURSOR,
        start.saturating_add(count),
        0,
    ));
    em.emit(Instruction::with_p4(
        Opcode::IdxInsert,
        INDEX_CURSOR,
        start,
        0,
        P4::Int(i64::from(count.saturating_add(1))),
    ));
    let next = em.emit(Instruction::new(Opcode::Next, TABLE_CURSOR, 0, 0));
    em.patch_p2(next, top);
    em.place(end);
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// sqlite3's rules for an index expression: no parameters, subqueries or
/// non-deterministic functions (their value could differ from the one
/// stored when the row is looked up again).
fn check_index_expression(index: &str, expr: &Expr) -> Result<(), CodegenError> {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        let prohibited = match &node.kind {
            ExprKind::Param(_) => Some("parameters"),
            ExprKind::Subquery(_)
            | ExprKind::Exists { .. }
            | ExprKind::InSubquery { .. }
            | ExprKind::InSubqueryMulti { .. } => Some("subqueries"),
            ExprKind::FunctionCall { name, .. }
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "random" | "randomblob" | "changes" | "total_changes" | "last_insert_rowid"
                ) =>
            {
                Some("non-deterministic functions")
            }
            _ => None,
        };
        if let Some(what) = prohibited {
            return Err(CodegenError::Unsupported {
                reason: format!("{what} prohibited in index expressions (index {index})"),
            });
        }
        stack.extend(crate::parser::row::grammar::children(node));
    }
    Ok(())
}

/// The table cursor and the new index's write cursor of the populate loop.
const TABLE_CURSOR: i32 = 0;
const INDEX_CURSOR: i32 = 1;

/// `term` without its `COLLATE` wrappers and parentheses, and the
/// collation it declares: the *innermost* (leftmost) `COLLATE` wins, as in
/// sqlite3 (`x COLLATE a COLLATE b` collates by `a`).
fn key_term(term: &Expr) -> (&Expr, Collation) {
    let mut expr = term;
    let mut collation = Collation::Binary;
    loop {
        match &expr.kind {
            ExprKind::Paren(inner) => expr = inner,
            ExprKind::Collate {
                expr: inner,
                collation: name,
            } => {
                collation = if name.eq_ignore_ascii_case("NOCASE") {
                    Collation::NoCase
                } else if name.eq_ignore_ascii_case("RTRIM") {
                    Collation::RTrim
                } else {
                    Collation::Binary
                };
                expr = inner;
            }
            _ => return (expr, collation),
        }
    }
}

/// The source text a parser span covers.
fn span_text(source: &str, span: crate::parser::Span) -> Result<String, CodegenError> {
    let start = span.offset as usize;
    let end = start.saturating_add(span.len as usize);
    source
        .get(start..end)
        .map(str::to_string)
        .ok_or_else(|| CodegenError::Unsupported {
            reason: "CREATE INDEX span out of bounds of the source text".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::row::error::{parse_create_index, ParseOutcome};

    fn schema() -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            database: 0,
            root_page: 2,
            columns: vec!["a".to_string(), "b".to_string()],
            without_rowid: false,
            strict: false,
            column_types: vec!["INTEGER".to_string(), "TEXT".to_string()],
            column_collations: vec![],
            is_virtual: false,
            sql: "CREATE TABLE t(a INTEGER, b TEXT)".to_string(),
            indexes: vec![],
            rowid_alias: None,
        }
        .with_computed_rowid_alias()
    }

    #[test]
    fn resolves_column_and_carries_verbatim_sql() {
        let sql = "CREATE INDEX idx_t_b ON t(b)";
        let ci = match parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_create_index(&ci, &schema(), sql).unwrap();

        match &program.instructions[1].p4 {
            P4::CreateIndex {
                name,
                column_indices,
                sql: got_sql,
                ..
            } => {
                assert_eq!(name, "idx_t_b");
                assert_eq!(column_indices, &vec![1]);
                assert_eq!(got_sql, sql);
            }
            other => panic!("expected P4::CreateIndex, got {other:?}"),
        }
    }

    #[test]
    fn desc_columns_travel_as_flags() {
        let sql = "CREATE INDEX idx_t_ab ON t(a, b DESC)";
        let ci = match parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_create_index(&ci, &schema(), sql).unwrap();
        match &program.instructions[1].p4 {
            P4::CreateIndex { desc, .. } => assert_eq!(desc, &vec![false, true]),
            other => panic!("expected P4::CreateIndex, got {other:?}"),
        }
    }

    #[test]
    fn expression_columns_are_populated_by_a_row_loop() {
        // #945: the VM cannot evaluate an expression inside `CreateIndex`,
        // so the program scans the table and `IdxInsert`s each row's key.
        let sql = "CREATE INDEX idx_t_expr ON t(a + 1, b DESC)";
        let ci = match parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_create_index(&ci, &schema(), sql).unwrap();
        let ops: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert!(
            ops.contains(&Opcode::IdxInsert) && ops.contains(&Opcode::Rewind),
            "{ops:?}"
        );
        match &program.instructions[1].p4 {
            P4::CreateIndex { root_reg, .. } => assert!(root_reg.is_some()),
            other => panic!("expected P4::CreateIndex, got {other:?}"),
        }
        let open = program
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::OpenWrite)
            .unwrap();
        assert_eq!(open.p5 & OPFLAG_P2ISREG, OPFLAG_P2ISREG);
    }

    #[test]
    fn a_partial_index_is_refused_rather_than_built_over_every_row() {
        let sql = "CREATE INDEX idx_t_p ON t(a) WHERE b > 1";
        let ci = match parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let err = compile_create_index(&ci, &schema(), sql).unwrap_err();
        assert!(matches!(err, CodegenError::Unsupported { .. }), "{err:?}");
    }

    #[test]
    fn rejects_unresolvable_column() {
        let sql = "CREATE INDEX idx_t_c ON t(c)";
        let ci = match parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let err = compile_create_index(&ci, &schema(), sql).unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("can't resolve"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn rejects_out_of_bounds_span() {
        let sql = "CREATE INDEX idx_t_b ON t(b)";
        let mut ci = match parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        ci.span.offset = u32::MAX - 1;
        ci.span.len = 10;
        let err = compile_create_index(&ci, &schema(), sql).unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("out of bounds"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
