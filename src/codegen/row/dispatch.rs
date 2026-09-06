//! Statement dispatch (db-core#97, ported from sqlite-rs's
//! `src/codegen/dispatch.rs`, itself moved out of the CLI binary per
//! Lab271/sqlite-rs#695 so it's usable without depending on a binary
//! crate): keyword-sniffs a raw SQL string to pick the right
//! parser/compiler pair for one statement.
//!
//! **Routes every statement kind `codegen::row` compiles** (#148):
//! `BEGIN`/`COMMIT`/`ROLLBACK`/`PRAGMA`/`ANALYZE`/`CREATE TABLE`/
//! `CREATE INDEX`/`CREATE VIEW`/`DROP TABLE`/`DROP INDEX` (unchanged
//! since #97), plus `SELECT`/`INSERT`/`UPDATE`/`DELETE` now that #147
//! retargeted [`super::select`]/[`super::stmt`] onto [`crate::parser::ast`].
//! A `SELECT` with a single `JOIN` resolves its right-hand table from
//! `schemas` and compiles via [`super::compile_select_join`]; anything
//! else (no `JOIN`, an optional `FROM`-subquery) goes through
//! [`super::compile_select_with_catalog`], which wires its own cursors.
//! N-way joins, `WITH`, compound `SELECT`, and everything else
//! [`super::select`] doesn't implement yet surface as
//! [`CodegenError::Unsupported`] from within it, not from here.

use crate::parser::ast::TableRefKind;
use crate::parser::row::error::{
    parse_analyze, parse_begin, parse_commit, parse_create_index, parse_create_table,
    parse_create_view, parse_delete, parse_drop_index, parse_drop_table, parse_insert,
    parse_pragma, parse_rollback, parse_select, parse_update, ParseOutcome,
};
use crate::vm::row::Program;

use super::{
    compile_analyze, compile_begin, compile_commit, compile_create_index, compile_create_table,
    compile_create_view, compile_delete, compile_drop_index, compile_drop_table, compile_insert,
    compile_pragma, compile_rollback, compile_select_join, compile_select_with_catalog,
    compile_update, CodegenError, TableSchema,
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
    "ANALYZE", "BEGIN", "COMMIT", "CREATE", "DELETE", "DROP", "END", "INDEX", "INSERT", "PRAGMA",
    "ROLLBACK", "SELECT", "TABLE", "UNIQUE", "UPDATE", "VIEW", "WITH",
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
        // `WITH ...` is a `SELECT`'s CTE prefix -- the AST's
        // `with_clause` field lives only on `Select` (#153's table
        // confirms `Insert`/`Update`/`Delete` have none), so a leading
        // `WITH` unambiguously means "parse and compile as a SELECT".
        "SELECT" | "WITH" => match parse_select(sql) {
            ParseOutcome::Accepted(select) => {
                // A single JOIN needs both cursors pre-wired, which only
                // `compile_select_join` does; anything else (no JOIN, an
                // optional FROM-subquery) goes through
                // `compile_select_with_catalog`, which wires cursor 0
                // itself. N-way joins are rejected inside `codegen::row`
                // regardless of which entry point reaches them.
                let joins = select.from.as_ref().map_or(&[][..], |f| f.joins.as_slice());
                match joins {
                    [] => {
                        // `compile_select_with_catalog` resolves the FROM
                        // table itself, but reports an unknown name as
                        // `CodegenError::Unsupported` (a generic message),
                        // not `DispatchError::NoSuchTable` -- the specific
                        // variant every other statement kind uses here.
                        // Pre-check a plain-table FROM so the two error
                        // shapes agree; a FROM-subquery has no name to
                        // check and is left to that call.
                        // Skip the pre-check for `WITH`: the FROM
                        // table may well be a CTE name, which isn't in
                        // the catalog at all -- that's not a "no such
                        // table" error, it's `codegen::row`'s own
                        // "WITH is not supported yet" rejection, and
                        // only `compile_select_with_catalog` knows to
                        // raise that one.
                        if select.with_clause.is_none() {
                            if let Some(name) = select.from.as_ref().and_then(|f| f.first.name()) {
                                find_schema(name)?;
                            }
                        }
                        Ok(compile_select_with_catalog(schemas, &select)?)
                    }
                    [join] => {
                        let left_name = select
                            .from
                            .as_ref()
                            .and_then(|f| f.first.name())
                            .ok_or_else(|| CodegenError::Unsupported {
                                reason: "a JOIN whose left side is a FROM-subquery is not yet \
                                             supported"
                                    .to_string(),
                            })?;
                        let left = find_schema(left_name)?;
                        let TableRefKind::Name(right_name) = &join.table.kind else {
                            return Err(CodegenError::Unsupported {
                                reason: "a JOIN against a FROM-subquery is not yet supported"
                                    .to_string(),
                            }
                            .into());
                        };
                        let right = find_schema(right_name)?;
                        Ok(compile_select_join(left, 0, right, 1, &select)?)
                    }
                    // More than one JOIN: let `codegen::row`'s own
                    // "only a single JOIN is supported" check produce the
                    // error, rather than duplicating that message here.
                    [join, ..] => {
                        let left_name = select.from.as_ref().and_then(|f| f.first.name());
                        let left = left_name.map(find_schema).transpose()?;
                        let right = match &join.table.kind {
                            TableRefKind::Name(name) => find_schema(name).ok(),
                            TableRefKind::Subquery(_) => None,
                        };
                        match (left, right) {
                            (Some(left), Some(right)) => {
                                Ok(compile_select_join(left, 0, right, 1, &select)?)
                            }
                            _ => Err(CodegenError::Unsupported {
                                reason: "N-way joins are not yet supported".to_string(),
                            }
                            .into()),
                        }
                    }
                }
            }
            other => Err(parse_error(other)),
        },
        "INSERT" => match parse_insert(sql) {
            ParseOutcome::Accepted(insert) => {
                let schema = find_schema(&insert.table)?;
                Ok(compile_insert(schema, &insert)?)
            }
            other => Err(parse_error(other)),
        },
        "UPDATE" => match parse_update(sql) {
            ParseOutcome::Accepted(update) => {
                let schema = find_schema(&update.table)?;
                Ok(compile_update(schema, &update)?)
            }
            other => Err(parse_error(other)),
        },
        "DELETE" => match parse_delete(sql) {
            ParseOutcome::Accepted(delete) => {
                let schema = find_schema(&delete.table)?;
                Ok(compile_delete(schema, &delete)?)
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

    /// #148: `SELECT`/`INSERT`/`UPDATE`/`DELETE` end-to-end through
    /// `compile_statement` -- SQL text -> `parser::ast` -> `codegen::row`
    /// -> `Program`, executed against a real cursor. Before this, these
    /// planners were reachable only from unit tests that hand-built
    /// `ast::Select`/`Insert`/`Update`/`Delete` and called the per-kind
    /// `compile_*` functions directly; the dispatcher itself never routed
    /// to them.
    mod end_to_end {
        use super::*;
        use crate::vm::row::{execute, Cursor, EphemeralTableCursor, Value, Vm};

        fn schema(columns: &[&str]) -> TableSchema {
            TableSchema {
                name: "t".to_string(),
                columns: columns.iter().map(|c| (*c).to_string()).collect(),
                column_types: columns.iter().map(|_| String::new()).collect(),
                ..Default::default()
            }
        }

        fn run(
            schemas: &[TableSchema],
            sql: &str,
            seed: Vec<(i64, Vec<Value>)>,
        ) -> Vec<Vec<Value>> {
            let program = compile_statement(sql, schemas).unwrap();
            let mut vm = Vm::new();
            let mut table = EphemeralTableCursor::new();
            for (rowid, values) in seed {
                table.insert(rowid, values);
            }
            vm.open_cursor(0, Box::new(table)).unwrap();
            execute(&mut vm, &program).unwrap()
        }

        #[test]
        fn dispatches_select() {
            let rows = run(
                &[schema(&["a"])],
                "SELECT a FROM t WHERE a > 1",
                vec![(1, vec![Value::Integer(1)]), (2, vec![Value::Integer(2)])],
            );
            assert_eq!(rows, vec![vec![Value::Integer(2)]]);
        }

        #[test]
        fn dispatches_select_with_a_join() {
            let left = schema(&["a"]);
            let mut right = schema(&["b", "c"]);
            right.name = "u".to_string();

            let program =
                compile_statement("SELECT a, u.c FROM t JOIN u ON t.a = u.b", &[left, right])
                    .unwrap();
            let mut vm = Vm::new();
            let mut left_table = EphemeralTableCursor::new();
            left_table.insert(1, vec![Value::Integer(1)]);
            vm.open_cursor(0, Box::new(left_table)).unwrap();
            let mut right_table = EphemeralTableCursor::new();
            right_table.insert(1, vec![Value::Integer(1), Value::Integer(100)]);
            vm.open_cursor(1, Box::new(right_table)).unwrap();
            let rows = execute(&mut vm, &program).unwrap();
            assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
        }

        #[test]
        fn dispatches_insert_then_select_sees_it() {
            let schemas = [schema(&["a"])];
            let insert_program = compile_statement("INSERT INTO t VALUES (1)", &schemas).unwrap();
            let mut vm = Vm::new();
            vm.open_cursor(0, Box::new(EphemeralTableCursor::new()))
                .unwrap();
            execute(&mut vm, &insert_program).unwrap();

            let select_program = compile_statement("SELECT a FROM t", &schemas).unwrap();
            let rows = execute(&mut vm, &select_program).unwrap();
            assert_eq!(rows, vec![vec![Value::Integer(1)]]);
        }

        #[test]
        fn dispatches_update() {
            let rows = run(
                &[schema(&["a", "b"])],
                "UPDATE t SET b = 99 WHERE a = 1",
                vec![(1, vec![Value::Integer(1), Value::Integer(10)])],
            );
            assert!(rows.is_empty(), "UPDATE has no result rows: {rows:?}");
        }

        #[test]
        fn dispatches_delete() {
            let program =
                compile_statement("DELETE FROM t WHERE a = 1", &[schema(&["a"])]).unwrap();
            let mut vm = Vm::new();
            let mut table = EphemeralTableCursor::new();
            table.insert(1, vec![Value::Integer(1)]);
            table.insert(2, vec![Value::Integer(2)]);
            vm.open_cursor(0, Box::new(table)).unwrap();
            execute(&mut vm, &program).unwrap();
        }

        #[test]
        fn select_unknown_table_is_reported() {
            let err = compile_statement("SELECT a FROM nope", &[]).unwrap_err();
            assert!(matches!(err, DispatchError::NoSuchTable(name) if name == "nope"));
        }

        #[test]
        fn insert_unknown_table_is_reported() {
            let err = compile_statement("INSERT INTO nope VALUES (1)", &[]).unwrap_err();
            assert!(matches!(err, DispatchError::NoSuchTable(name) if name == "nope"));
        }

        /// A construct the AST can express but `codegen::row` can't
        /// compile yet must fail with a clear error naming it, not a
        /// panic -- the same contract #147's `Unsupported` stubs promise,
        /// now proven from the dispatcher entry point a real caller uses.
        /// `DISTINCT` stands in for this today; a `WITH`-clause query
        /// used to (until #143 added CTE support), so this test can't
        /// use that construct as its example anymore.
        #[test]
        fn unsupported_select_construct_fails_clearly_through_dispatch() {
            let err = compile_statement("SELECT DISTINCT a FROM t", &[schema(&["a"])]).unwrap_err();
            assert!(matches!(
                err,
                DispatchError::Codegen(CodegenError::Unsupported { .. })
            ));
        }
    }
}
