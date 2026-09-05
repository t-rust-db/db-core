//! Statement dispatch (db-core#97, ported from sqlite-rs's
//! `src/codegen/dispatch.rs`, itself moved out of the CLI binary per
//! Lab271/sqlite-rs#695 so it's usable without depending on a binary
//! crate): keyword-sniffs a raw SQL string to pick the right
//! parser/compiler pair for one statement.
//!
//! **Scoped to what this crate has codegen for.** sqlite-rs's dispatcher
//! also routes `INSERT`/`UPDATE`/`DELETE`/`SELECT`; none of those have a
//! `codegen::row` counterpart yet (only #91's expression-compilation
//! slice landed), so [`compile_statement`] only knows
//! `BEGIN`/`COMMIT`/`ROLLBACK`/`PRAGMA`/`ANALYZE`/`CREATE TABLE`/
//! `CREATE INDEX`/`CREATE VIEW`/`DROP TABLE`/`DROP INDEX` -- every
//! statement kind [`super::ddl`], [`super::transaction`], [`super::pragma`],
//! and [`super::analyze`] compile. Routing the DML/SELECT statements is
//! deferred to whichever sub-ticket of #20 ports their codegen.

use crate::parser::row::error::{
    parse_analyze, parse_begin, parse_commit, parse_create_index, parse_create_table,
    parse_create_view, parse_drop_index, parse_drop_table, parse_pragma, parse_rollback,
    ParseOutcome,
};
use crate::vm::row::Program;

use super::{
    compile_analyze, compile_begin, compile_commit, compile_create_index, compile_create_table,
    compile_create_view, compile_drop_index, compile_drop_table, compile_pragma, compile_rollback,
    CodegenError, TableSchema,
};

/// Failure compiling one dispatched statement -- everything
/// [`compile_statement`] can fail with, folded into one error type so
/// callers don't need to know about the per-statement parser/codegen
/// error types individually.
#[derive(Debug)]
pub enum DispatchError {
    /// The statement referenced a table not present in the schema catalog.
    NoSuchTable(String),
    /// The statement referenced an index not present in the schema catalog.
    NoSuchIndex(String),
    /// The leading keyword(s) didn't match any statement kind this
    /// dispatcher knows how to parse/compile.
    Unrecognized(String),
    /// Compilation of the parsed statement failed.
    Codegen(CodegenError),
    /// Parsing the statement failed.
    ParseFailed(String),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::NoSuchTable(name) => write!(f, "no such table: {name}"),
            DispatchError::NoSuchIndex(name) => write!(f, "no such index: {name}"),
            DispatchError::Unrecognized(kw) => {
                write!(f, "unsupported or unrecognized statement: {kw:?} ...")
            }
            DispatchError::Codegen(source) => write!(f, "{source}"),
            DispatchError::ParseFailed(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for DispatchError {}

impl From<CodegenError> for DispatchError {
    fn from(source: CodegenError) -> Self {
        DispatchError::Codegen(source)
    }
}

/// The first one or two whitespace-separated words of `sql`, uppercased.
const DISPATCH_WORDS: &[&str] = &[
    "ANALYZE", "BEGIN", "COMMIT", "CREATE", "DROP", "END", "INDEX", "PRAGMA", "ROLLBACK", "TABLE",
    "UNIQUE", "VIEW",
];

/// `word`'s canonical uppercase spelling if it's one of the statement
/// keywords dispatch branches on, else `""`.
fn canonical(word: &str) -> &'static str {
    DISPATCH_WORDS
        .iter()
        .copied()
        .find(|candidate| candidate.eq_ignore_ascii_case(word))
        .unwrap_or("")
}

fn parse_error<T: std::fmt::Debug>(other: ParseOutcome<T>) -> DispatchError {
    DispatchError::ParseFailed(format!("{other:?}"))
}

/// Parses `sql`, picks the compiler for its leading keyword(s), and
/// compiles it against `schemas` -- the schema catalog every DDL/ANALYZE
/// statement resolves table/index names against.
pub fn compile_statement(sql: &str, schemas: &[TableSchema]) -> Result<Program, DispatchError> {
    let find_schema = |name: &str| -> Result<&TableSchema, DispatchError> {
        schemas
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| DispatchError::NoSuchTable(name.to_string()))
    };
    let find_index_root = |name: &str| -> Result<u32, DispatchError> {
        schemas
            .iter()
            .flat_map(|s| &s.indexes)
            .find(|idx| idx.name.eq_ignore_ascii_case(name))
            .map(|idx| idx.root_page)
            .ok_or_else(|| DispatchError::NoSuchIndex(name.to_string()))
    };

    let mut words = sql.split_whitespace();
    let first_word = words.next().unwrap_or("");
    let head = canonical(first_word);
    let second = canonical(words.next().unwrap_or(""));

    match head {
        "BEGIN" => match parse_begin(sql) {
            ParseOutcome::Accepted(begin) => Ok(compile_begin(&begin)),
            other => Err(parse_error(other)),
        },
        "COMMIT" | "END" => match parse_commit(sql) {
            ParseOutcome::Accepted(commit) => Ok(compile_commit(&commit)),
            other => Err(parse_error(other)),
        },
        "ROLLBACK" => match parse_rollback(sql) {
            ParseOutcome::Accepted(rollback) => Ok(compile_rollback(&rollback)),
            other => Err(parse_error(other)),
        },
        "PRAGMA" => match parse_pragma(sql) {
            ParseOutcome::Accepted(pragma) => Ok(compile_pragma(&pragma)),
            other => Err(parse_error(other)),
        },
        "ANALYZE" => match parse_analyze(sql) {
            ParseOutcome::Accepted(analyze) => {
                let targets: Vec<&TableSchema> = match &analyze.target {
                    None => schemas.iter().collect(),
                    Some(name) => {
                        if let Some(schema) =
                            schemas.iter().find(|s| s.name.eq_ignore_ascii_case(name))
                        {
                            vec![schema]
                        } else if schemas
                            .iter()
                            .flat_map(|s| &s.indexes)
                            .any(|idx| idx.name.eq_ignore_ascii_case(name))
                        {
                            // Real SQLite also accepts `ANALYZE
                            // index-name` -- out of scope here, so
                            // `Unsupported` rather than the
                            // `NoSuchTable` a genuinely unknown name
                            // gets below.
                            return Err(CodegenError::Unsupported {
                                reason: format!(
                                    "ANALYZE of a single index ({name:?}) is not yet supported"
                                ),
                            }
                            .into());
                        } else {
                            return Err(DispatchError::NoSuchTable(name.clone()));
                        }
                    }
                };
                Ok(compile_analyze(&targets))
            }
            other => Err(parse_error(other)),
        },
        "CREATE" if second == "TABLE" => match parse_create_table(sql) {
            ParseOutcome::Accepted(create) => Ok(compile_create_table(&create, sql)?),
            other => Err(parse_error(other)),
        },
        "CREATE" if second == "VIEW" => match parse_create_view(sql) {
            ParseOutcome::Accepted(create) => Ok(compile_create_view(&create, sql)?),
            other => Err(parse_error(other)),
        },
        "CREATE" if second == "INDEX" || second == "UNIQUE" => match parse_create_index(sql) {
            ParseOutcome::Accepted(ci) => {
                let schema = find_schema(&ci.table)?;
                Ok(compile_create_index(&ci, schema, sql)?)
            }
            other => Err(parse_error(other)),
        },
        "DROP" if second == "TABLE" => match parse_drop_table(sql) {
            ParseOutcome::Accepted(drop) => {
                let schema = find_schema(&drop.name)?;
                Ok(compile_drop_table(&drop, schema))
            }
            other => Err(parse_error(other)),
        },
        "DROP" if second == "INDEX" => match parse_drop_index(sql) {
            ParseOutcome::Accepted(di) => {
                let root_page = find_index_root(&di.name)?;
                Ok(compile_drop_index(&di, root_page))
            }
            other => Err(parse_error(other)),
        },
        // Reports the statement's actual leading word (uppercased, as
        // before), not `canonical`'s `""` sentinel -- this is a cold
        // path, so the one allocation is free.
        _ => Err(DispatchError::Unrecognized(first_word.to_ascii_uppercase())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::vm::row::Opcode;

    fn opcodes(program: &Program) -> Vec<Opcode> {
        program.instructions.iter().map(|i| i.opcode).collect()
    }

    #[test]
    fn dispatches_begin() {
        let program = compile_statement("BEGIN", &[]).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::Transaction, Opcode::Halt]
        );
    }

    #[test]
    fn dispatches_commit_and_end() {
        assert_eq!(
            opcodes(&compile_statement("COMMIT", &[]).unwrap()),
            vec![Opcode::Init, Opcode::AutoCommit, Opcode::Halt]
        );
        assert_eq!(
            opcodes(&compile_statement("END", &[]).unwrap()),
            vec![Opcode::Init, Opcode::AutoCommit, Opcode::Halt]
        );
    }

    #[test]
    fn dispatches_pragma() {
        let program = compile_statement("PRAGMA journal_mode = WAL", &[]).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::SetJournalMode, Opcode::Halt]
        );
    }

    #[test]
    fn dispatches_bare_analyze_over_every_schema() {
        let schemas = vec![
            TableSchema {
                name: "t1".to_string(),
                root_page: 2,
                ..Default::default()
            },
            TableSchema {
                name: "t2".to_string(),
                root_page: 3,
                ..Default::default()
            },
        ];
        let program = compile_statement("ANALYZE", &schemas).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::Analyze, Opcode::Halt]
        );
        match &program.instructions[1].p4 {
            crate::vm::row::P4::Analyze { targets } => assert_eq!(targets.len(), 2),
            other => panic!("expected P4::Analyze, got {other:?}"),
        }
    }

    #[test]
    fn dispatches_analyze_unknown_table_error() {
        let err = compile_statement("ANALYZE nope", &[]).unwrap_err();
        assert!(matches!(err, DispatchError::NoSuchTable(name) if name == "nope"));
    }

    #[test]
    fn dispatches_create_table() {
        let sql = "CREATE TABLE t(a INTEGER)";
        let program = compile_statement(sql, &[]).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::CreateTable, Opcode::Halt]
        );
    }

    #[test]
    fn dispatches_create_index_against_known_table() {
        let sql = "CREATE INDEX idx_t_a ON t(a)";
        let schemas = vec![TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string()],
            column_types: vec![String::new()],
            root_page: 2,
            ..Default::default()
        }];
        let program = compile_statement(sql, &schemas).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::CreateIndex, Opcode::Halt]
        );
    }

    #[test]
    fn dispatches_create_index_no_such_table() {
        let sql = "CREATE INDEX idx_t_a ON t(a)";
        let err = compile_statement(sql, &[]).unwrap_err();
        assert!(matches!(err, DispatchError::NoSuchTable(name) if name == "t"));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__dispatch_174__v1_create_index_dispatches_via_index_keyword() {
        let sql = "CREATE INDEX idx_t_a ON t(a)";
        let schemas = vec![TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string()],
            column_types: vec![String::new()],
            root_page: 2,
            ..Default::default()
        }];
        let program = compile_statement(sql, &schemas).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::CreateIndex, Opcode::Halt]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__dispatch_174__v2_create_unique_index_dispatches_via_unique_keyword() {
        let sql = "CREATE UNIQUE INDEX idx_t_a ON t(a)";
        let schemas = vec![TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string()],
            column_types: vec![String::new()],
            root_page: 2,
            ..Default::default()
        }];
        let program = compile_statement(sql, &schemas).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::CreateIndex, Opcode::Halt]
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__dispatch_174__v3_create_with_neither_keyword_is_unrecognized() {
        let sql = "CREATE SEQUENCE s";
        let err = compile_statement(sql, &[]).unwrap_err();
        assert!(matches!(err, DispatchError::Unrecognized(name) if name == "CREATE"));
    }

    #[test]
    fn dispatches_drop_table() {
        let schemas = vec![TableSchema {
            name: "t".to_string(),
            root_page: 2,
            ..Default::default()
        }];
        let program = compile_statement("DROP TABLE t", &schemas).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::DropTable, Opcode::Halt]
        );
    }

    #[test]
    fn dispatches_drop_index() {
        let schemas = vec![TableSchema {
            name: "t".to_string(),
            root_page: 2,
            indexes: vec![super::super::IndexSchema {
                name: "idx_t_a".to_string(),
                root_page: 3,
                columns: vec![],
            }],
            ..Default::default()
        }];
        let program = compile_statement("DROP INDEX idx_t_a", &schemas).unwrap();
        assert_eq!(
            opcodes(&program),
            vec![Opcode::Init, Opcode::DropIndex, Opcode::Halt]
        );
    }

    #[test]
    fn unrecognized_statement_reports_leading_word() {
        let err = compile_statement("FROBNICATE t", &[]).unwrap_err();
        assert!(matches!(err, DispatchError::Unrecognized(word) if word == "FROBNICATE"));
    }
}
