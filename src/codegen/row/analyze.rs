//! `Analyze` AST -> `Program` compilation (db-core#97, ported from
//! sqlite-rs's `src/codegen/analyze.rs`). Like [`super::ddl`]'s
//! `CreateTable`/`CreateIndex`, a single `Opcode::Analyze` instruction
//! does the whole job procedurally at exec time (scan each target
//! table/index, replace its `sqlite_stat1` rows) rather than a
//! decomposed cursor-driven sequence -- every target table's root page
//! and its indexes' names/root pages are baked into `P4::Analyze` here,
//! at codegen time, from the schema catalog. Which table(s) `targets`
//! names (bare `ANALYZE` vs `ANALYZE table-name`) is the caller's job --
//! [`super::dispatch`] resolves the AST's `target: Option<String>`
//! against the schema catalog before calling this function.

use crate::vm::row::{AnalyzeIndexTarget, AnalyzeTarget, Instruction, Opcode, Program, P4};

use super::{Emitter, TableSchema};

/// Compiles `ANALYZE` (or `ANALYZE table-name`) into a single-instruction
/// `Program` that replaces `sqlite_stat1` rows for every table in
/// `targets` (and their indexes) at exec time. See the module doc for why
/// this bakes root pages/names in at codegen time instead of a
/// cursor-driven sequence.
pub fn compile_analyze(targets: &[&TableSchema]) -> Program {
    let targets: Vec<AnalyzeTarget> = targets
        .iter()
        .map(|schema| AnalyzeTarget {
            table_name: schema.name.clone(),
            table_root_page: schema.root_page,
            indexes: schema
                .indexes
                .iter()
                .map(|idx| AnalyzeIndexTarget {
                    index_name: idx.name.clone(),
                    root_page: idx.root_page,
                })
                .collect(),
        })
        .collect();

    let mut em = Emitter::new();
    let init_addr = em.emit(Instruction::new(Opcode::Init, 0, 0, 0));
    let body_start = em.new_label();
    em.place(body_start);
    em.patch_p2(init_addr, body_start);

    em.emit(Instruction::with_p4(
        Opcode::Analyze,
        0,
        0,
        0,
        P4::Analyze { targets },
    ));
    em.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    em.finish()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::codegen::row::IndexSchema;
    use crate::vm::row::Opcode;

    fn table(name: &str, root_page: u32, indexes: Vec<IndexSchema>) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            root_page,
            columns: vec!["a".to_string()],
            column_types: vec![String::new()],
            rowid_alias: None,
            indexes,
        }
    }

    #[test]
    fn compiles_to_init_analyze_halt() {
        let t = table("t", 2, vec![]);
        let program = compile_analyze(&[&t]);

        let opcodes: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        assert_eq!(opcodes, vec![Opcode::Init, Opcode::Analyze, Opcode::Halt]);
        match &program.instructions[1].p4 {
            P4::Analyze { targets } => {
                assert_eq!(targets.len(), 1);
                assert_eq!(targets[0].table_name, "t");
                assert_eq!(targets[0].table_root_page, 2);
            }
            other => panic!("expected P4::Analyze, got {other:?}"),
        }
    }

    #[test]
    fn bakes_every_index_on_the_target_table() {
        let idx = IndexSchema {
            name: "idx_a".to_string(),
            root_page: 3,
            columns: vec![],
        };
        let t = table("t", 2, vec![idx]);
        let program = compile_analyze(&[&t]);

        match &program.instructions[1].p4 {
            P4::Analyze { targets } => {
                assert_eq!(targets[0].indexes.len(), 1);
                assert_eq!(targets[0].indexes[0].index_name, "idx_a");
                assert_eq!(targets[0].indexes[0].root_page, 3);
            }
            other => panic!("expected P4::Analyze, got {other:?}"),
        }
    }
}
