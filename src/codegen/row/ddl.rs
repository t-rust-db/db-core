//! DDL codegen (db-core#97, ported from sqlite-rs's
//! `src/codegen/ddl/{create_table,create_index,create_view,drop_table,
//! drop_index}.rs`): `CREATE TABLE`/`DROP TABLE`/`CREATE INDEX`/
//! `DROP INDEX`/`CREATE VIEW`. Each compiles to a single procedural
//! opcode at exec time -- no per-row cursor work, unlike the DML
//! statements the parent `codegen` module eventually gains. Kept as one
//! file (rather than sqlite-rs's five-file `ddl/` submodule) since each
//! function here is a few dozen lines; db-core has no `#[allow(unused)]`
//! multi-thousand-line file to split up yet.

use crate::parser::row::ast::{CreateIndex, CreateTable, CreateView, DropIndex, DropTable};
use crate::vm::row::{Instruction, Opcode, Program, P4};

use super::{CodegenError, Emitter, Result, TableSchema};

/// Compiles `CREATE TABLE` into a single `Opcode::CreateTable` instruction
/// that allocates the root page, registers the verbatim source text as
/// the `sqlite_master` row, and bumps the schema cookie at exec time.
///
/// `sqlite_master.sql` gets the **verbatim** source text of the
/// statement (sliced via `create.span`), not a reconstruction from the
/// parsed `ColumnDef`s -- matching stock SQLite's own storage
/// convention.
pub fn compile_create_table(create: &CreateTable, source: &str) -> Result<Program> {
    let sql = slice_span(source, create.span.offset, create.span.len)?;

    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::with_p4(
        Opcode::CreateTable,
        0,
        0,
        0,
        P4::CreateTable {
            name: create.name.clone(),
            sql,
        },
    ));
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// Compiles `CREATE VIEW` into a single `Opcode::CreateView` instruction
/// that writes the verbatim source text into `sqlite_master.sql` with
/// `rootpage` `0` (views have no b-tree of their own).
pub fn compile_create_view(create: &CreateView, source: &str) -> Result<Program> {
    let sql = slice_span(source, create.span.offset, create.span.len)?;

    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::with_p4(
        Opcode::CreateView,
        0,
        0,
        0,
        P4::CreateView {
            name: create.name.clone(),
            sql,
        },
    ));
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// Compiles `DROP TABLE` into a single `Opcode::DropTable` instruction
/// that frees the table's pages, cascade-drops its indexes, and removes
/// its `sqlite_master` row(s) at exec time.
pub fn compile_drop_table(drop: &DropTable, schema: &TableSchema) -> Program {
    let indexes = schema
        .indexes
        .iter()
        .map(|idx| (idx.name.clone(), idx.root_page))
        .collect();

    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::with_p4(
        Opcode::DropTable,
        0,
        0,
        0,
        P4::DropTable {
            name: drop.name.clone(),
            root_page: schema.root_page,
            indexes,
        },
    ));
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    em.finish()
}

/// Compiles `DROP INDEX` into a single `Opcode::DropIndex` instruction
/// that frees the index's pages, removes its `sqlite_master` row, and
/// bumps the schema cookie at exec time.
pub fn compile_drop_index(di: &DropIndex, root_page: u32) -> Program {
    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::with_p4(
        Opcode::DropIndex,
        0,
        0,
        0,
        P4::DropIndex {
            name: di.name.clone(),
            root_page,
        },
    ));
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    em.finish()
}

/// Compiles `CREATE INDEX` into a single `Opcode::CreateIndex` instruction
/// that allocates the index's root page, populates it from the target
/// table's existing rows, registers it in `sqlite_master`, and bumps the
/// schema cookie at exec time.
///
/// Column resolution rejects a `DESC` column and an indexed expression
/// (rather than a bare column ref): no index b-tree comparator in this
/// codebase is aware of per-column sort direction or evaluates arbitrary
/// expressions yet.
pub fn compile_create_index(
    ci: &CreateIndex,
    schema: &TableSchema,
    source: &str,
) -> Result<Program> {
    let mut column_indices = Vec::with_capacity(ci.columns.len());
    for col in &ci.columns {
        if col.desc == Some(true) {
            return Err(CodegenError::Unsupported {
                reason: format!(
                    "index {} has a DESC column; descending index keys aren't supported yet",
                    ci.name
                ),
            });
        }
        let crate::parser::row::ast::ExprKind::Column { name, .. } = &col.expr.kind else {
            return Err(CodegenError::Unsupported {
                reason: format!(
                    "index {} indexes an expression, not a plain column; not supported yet",
                    ci.name
                ),
            });
        };
        let idx = schema
            .column_index(name)
            .ok_or_else(|| CodegenError::Unsupported {
                reason: format!(
                    "index {} references a column this codegen can't resolve: {name}",
                    ci.name
                ),
            })?;
        column_indices.push(idx);
    }

    let sql = slice_span(source, ci.span.offset, ci.span.len)?;

    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::with_p4(
        Opcode::CreateIndex,
        0,
        0,
        0,
        P4::CreateIndex {
            name: ci.name.clone(),
            table_name: schema.name.clone(),
            table_root_page: schema.root_page,
            sql,
            column_indices,
            unique: ci.unique,
        },
    ));
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(em.finish())
}

/// Slices `source[offset..offset+len]`, mapping an out-of-bounds span
/// (shouldn't happen for a span the parser itself produced, but the
/// codegen boundary is where every consumer here checks it) to
/// [`CodegenError::Unsupported`] rather than panicking.
fn slice_span(source: &str, offset: u32, len: u32) -> Result<String> {
    let start = offset as usize;
    let end = start.saturating_add(len as usize);
    source
        .get(start..end)
        .map(str::to_string)
        .ok_or_else(|| CodegenError::Unsupported {
            reason: "statement span out of bounds of the source text".to_string(),
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::codegen::row::IndexSchema;
    use crate::parser::row::error::ParseOutcome;
    use crate::vm::row::Opcode;

    fn schema(columns: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            root_page: 2,
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            column_types: columns.iter().map(|_| String::new()).collect(),
            rowid_alias: None,
            indexes: vec![],
        }
    }

    #[test]
    fn create_table_compiles_to_init_create_table_halt() {
        let sql = "CREATE TABLE t(a INTEGER, b TEXT)";
        let create = match crate::parser::row::error::parse_create_table(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_create_table(&create, sql).unwrap();

        let opcodes: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert_eq!(
            opcodes,
            vec![Opcode::Init, Opcode::CreateTable, Opcode::Halt]
        );
        match &program.instructions[1].p4 {
            P4::CreateTable { name, sql: got_sql } => {
                assert_eq!(name, "t");
                assert_eq!(got_sql, sql);
            }
            other => panic!("expected P4::CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn create_view_compiles_to_init_create_view_halt() {
        let sql = "CREATE VIEW v AS SELECT a FROM t";
        let create = match crate::parser::row::error::parse_create_view(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_create_view(&create, sql).unwrap();
        let ops: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert_eq!(ops, vec![Opcode::Init, Opcode::CreateView, Opcode::Halt]);
    }

    fn schema_with_index() -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            root_page: 2,
            columns: vec!["a".to_string()],
            column_types: vec!["INTEGER".to_string()],
            rowid_alias: None,
            indexes: vec![IndexSchema {
                name: "idx_t_a".to_string(),
                root_page: 3,
            }],
        }
    }

    #[test]
    fn drop_table_compiles_to_init_drop_table_halt_carrying_indexes() {
        let drop = match crate::parser::row::error::parse_drop_table("DROP TABLE t") {
            ParseOutcome::Accepted(d) => d,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let schema = schema_with_index();
        let program = compile_drop_table(&drop, &schema);

        let opcodes: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert_eq!(opcodes, vec![Opcode::Init, Opcode::DropTable, Opcode::Halt]);
        match &program.instructions[1].p4 {
            P4::DropTable {
                name,
                root_page,
                indexes,
            } => {
                assert_eq!(name, "t");
                assert_eq!(*root_page, 2);
                assert_eq!(indexes, &vec![("idx_t_a".to_string(), 3)]);
            }
            other => panic!("expected P4::DropTable, got {other:?}"),
        }
    }

    #[test]
    fn drop_index_compiles_to_init_drop_index_halt() {
        let di = match crate::parser::row::error::parse_drop_index("DROP INDEX idx_t_a") {
            ParseOutcome::Accepted(d) => d,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_drop_index(&di, 3);

        let opcodes: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert_eq!(opcodes, vec![Opcode::Init, Opcode::DropIndex, Opcode::Halt]);
        match &program.instructions[1].p4 {
            P4::DropIndex { name, root_page } => {
                assert_eq!(name, "idx_t_a");
                assert_eq!(*root_page, 3);
            }
            other => panic!("expected P4::DropIndex, got {other:?}"),
        }
    }

    #[test]
    fn create_index_resolves_column_and_carries_verbatim_sql() {
        let sql = "CREATE INDEX idx_t_b ON t(b)";
        let ci = match crate::parser::row::error::parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let program = compile_create_index(&ci, &schema(&["a", "b"]), sql).unwrap();

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
    fn create_index_rejects_desc_column() {
        let sql = "CREATE INDEX idx_t_b ON t(b DESC)";
        let ci = match crate::parser::row::error::parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let err = compile_create_index(&ci, &schema(&["a", "b"]), sql).unwrap_err();
        assert!(matches!(err, CodegenError::Unsupported { .. }));
    }

    #[test]
    fn create_index_rejects_expression_column() {
        let sql = "CREATE INDEX idx_t_expr ON t(a + 1)";
        let ci = match crate::parser::row::error::parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let err = compile_create_index(&ci, &schema(&["a"]), sql).unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("indexes an expression"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn create_index_rejects_unresolvable_column() {
        let sql = "CREATE INDEX idx_t_c ON t(c)";
        let ci = match crate::parser::row::error::parse_create_index(sql) {
            ParseOutcome::Accepted(c) => c,
            other => panic!("expected Accepted, got {other:?}"),
        };
        let err = compile_create_index(&ci, &schema(&["a", "b"]), sql).unwrap_err();
        match err {
            CodegenError::Unsupported { reason } => {
                assert!(reason.contains("can't resolve"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
