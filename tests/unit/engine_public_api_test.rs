// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `db_core::engine` (#295, ADR 0017): the client
//! seam as a client sees it -- open a file, run SQL, get `Cell`s back,
//! explain, ask for stats -- through the trait, including as
//! `Box<dyn Engine>`, which is how db-studio holds it. The fixture is a
//! temp copy of a committed SQLite file, so writes never touch
//! `tests/corpus/fixtures`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::{Path, PathBuf};

use db_core::engine::row::RowEngine;
use db_core::engine::{
    Cell, Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeSection, PlanRow, QueryResult,
};
use db_core::value::Value;

const FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";

/// A writable copy of the fixture, removed on drop.
struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-engine-{label}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::copy(FIXTURE, &path).expect("copy fixture");
        TempDb(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn open(db: &TempDb) -> RowEngine {
    RowEngine::open(db.path()).expect("open fixture copy")
}

fn ints(rows: &[Vec<Cell>]) -> Vec<Vec<i64>> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|c| match c {
                    Cell::Int(n) => *n,
                    other => panic!("expected Int, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn open_reports_row_mode_and_header_stats() {
    let db = TempDb::new("stats");
    let engine = open(&db);
    assert_eq!(engine.mode(), Mode::Row);
    match engine.stats() {
        FileStats::Row {
            page_size,
            page_count,
            freelist_pages,
        } => {
            assert!(page_size >= 512, "{page_size}");
            assert!(page_count >= 1, "{page_count}");
            assert_eq!(freelist_pages, 0);
        }
        other => panic!("row engine reported {other:?}"),
    }
    assert_eq!(engine.path(), db.path());
}

#[test]
fn open_missing_file_is_an_open_error() {
    let err = RowEngine::open(Path::new("/nonexistent/dir/no-such.db")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Open);
    assert!(!err.message.is_empty());
}

#[test]
fn open_non_sqlite_file_is_an_open_error() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "db-core-engine-notsqlite-{}.db",
        std::process::id()
    ));
    std::fs::write(
        &path,
        b"this is not a database file, not even close to 100 bytes",
    )
    .unwrap();
    let err = RowEngine::open(&path).unwrap_err();
    std::fs::remove_file(&path).ok();
    assert_eq!(err.kind, ErrorKind::Open);
}

#[test]
fn ddl_dml_then_select_round_trips_with_schema_derived_headers() {
    let db = TempDb::new("roundtrip");
    let mut engine = open(&db);

    let created = engine
        .run_query("CREATE TABLE eng_t(a INTEGER, b TEXT, c REAL, d BLOB)")
        .unwrap();
    assert!(created.is_empty());

    let inserted = engine
        .run_query(
            "INSERT INTO eng_t VALUES (2, 'two', 2.5, X'0102'); \
             INSERT INTO eng_t VALUES (1, 'one', NULL, NULL)",
        )
        .unwrap();
    assert!(inserted.is_empty(), "DML yields no rows: {inserted:?}");

    let result = engine
        .run_query("SELECT a, b, c, d FROM eng_t ORDER BY a")
        .unwrap();
    assert_eq!(result.columns, ["a", "b", "c", "d"]);
    assert_eq!(
        result.rows,
        vec![
            vec![
                Cell::Int(1),
                Cell::Text("one".to_string()),
                Cell::Null,
                Cell::Null
            ],
            vec![
                Cell::Int(2),
                Cell::Text("two".to_string()),
                Cell::Real(2.5),
                Cell::Blob(vec![1, 2])
            ],
        ]
    );
}

#[test]
fn multi_statement_script_returns_the_last_result_set() {
    let db = TempDb::new("script");
    let mut engine = open(&db);
    let result = engine
        .run_query(
            "CREATE TABLE s(x INTEGER); INSERT INTO s VALUES (7); \
             SELECT x FROM s; INSERT INTO s VALUES (8); SELECT x FROM s ORDER BY x",
        )
        .unwrap();
    assert_eq!(result.columns, ["x"]);
    assert_eq!(ints(&result.rows), vec![vec![7], vec![8]]);
}

#[test]
fn ddl_invalidates_the_catalog_so_a_new_table_is_visible_next_statement() {
    let db = TempDb::new("catalog");
    let mut engine = open(&db);
    engine.run_query("CREATE TABLE first_t(a INTEGER)").unwrap();
    engine
        .run_query("CREATE TABLE second_t(b INTEGER)")
        .unwrap();
    engine
        .run_query("INSERT INTO second_t VALUES (42)")
        .unwrap();
    let r = engine.run_query("SELECT b FROM second_t").unwrap();
    assert_eq!(ints(&r.rows), vec![vec![42]]);
}

#[test]
fn transaction_state_persists_across_run_query_calls() {
    let db = TempDb::new("txn");
    let mut engine = open(&db);
    engine.run_query("CREATE TABLE tx(v INTEGER)").unwrap();
    engine.run_query("BEGIN").unwrap();
    engine.run_query("INSERT INTO tx VALUES (1)").unwrap();
    engine.run_query("INSERT INTO tx VALUES (2)").unwrap();
    engine.run_query("COMMIT").unwrap();
    let r = engine.run_query("SELECT count(*) FROM tx").unwrap();
    assert_eq!(ints(&r.rows), vec![vec![2]]);

    engine.run_query("BEGIN").unwrap();
    engine.run_query("INSERT INTO tx VALUES (3)").unwrap();
    engine.run_query("ROLLBACK").unwrap();
    let r = engine.run_query("SELECT count(*) FROM tx").unwrap();
    assert_eq!(ints(&r.rows), vec![vec![2]]);
}

#[test]
fn syntax_error_is_parse_kind_and_leaves_the_engine_usable() {
    let db = TempDb::new("parse");
    let mut engine = open(&db);
    let err = engine.run_query("SELECT FROM WHERE").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse, "{err}");
    // Still usable afterwards.
    engine
        .run_query("CREATE TABLE after_err(a INTEGER)")
        .unwrap();
}

#[test]
fn unknown_table_is_compile_kind() {
    let db = TempDb::new("compile");
    let mut engine = open(&db);
    let err = engine
        .run_query("SELECT * FROM no_such_table_here")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile, "{err}");
    assert!(err.message.contains("no_such_table_here"), "{err}");
}

#[test]
fn explain_plan_describes_a_scan_and_a_search() {
    let db = TempDb::new("plan");
    let mut engine = open(&db);
    engine
        .run_query(
            "CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT); CREATE INDEX p_name ON p(name)",
        )
        .unwrap();

    let scan: Vec<PlanRow> = engine.explain_plan("SELECT * FROM p").unwrap();
    assert!(!scan.is_empty());
    assert!(
        scan.iter().any(|r| r.detail.starts_with("SCAN")),
        "{scan:?}"
    );

    // Rowid lookup: the planner's guaranteed SEARCH shape. (Equality on a
    // secondary index still plans as SCAN in db-core today, same as
    // sqlite-rs -- a planner gap, not this seam's.)
    let search = engine.explain_plan("SELECT * FROM p WHERE id = 1").unwrap();
    assert!(
        search
            .iter()
            .any(|r| r.detail.starts_with("SEARCH p USING INTEGER PRIMARY KEY")),
        "{search:?}"
    );
}

#[test]
fn explain_plan_rejects_non_select_and_multiple_statements() {
    let db = TempDb::new("plan-errors");
    let mut engine = open(&db);
    engine.run_query("CREATE TABLE q(a INTEGER)").unwrap();

    let err = engine.explain_plan("INSERT INTO q VALUES (1)").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");

    let err = engine
        .explain_plan("SELECT a FROM q; SELECT a FROM q")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");

    let err = engine.explain_plan("   ").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse, "{err}");
}

#[test]
fn explain_opcodes_is_one_main_section_in_address_order() {
    let db = TempDb::new("opcodes");
    let mut engine = open(&db);
    engine.run_query("CREATE TABLE o(a INTEGER)").unwrap();

    let sections: Vec<OpcodeSection> = engine.explain_opcodes("SELECT a FROM o").unwrap();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].label, "main");
    let rows = &sections[0].rows;
    assert!(rows.len() > 1, "{rows:?}");
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.addr, i, "addresses are dense and ordered: {rows:?}");
        assert!(!r.opcode.is_empty());
    }
    assert!(rows.iter().any(|r| r.opcode == "Halt"), "{rows:?}");

    // Non-SELECT statements have opcodes too.
    let dml = engine.explain_opcodes("INSERT INTO o VALUES (1)").unwrap();
    assert!(!dml[0].rows.is_empty());
}

#[test]
fn engine_is_object_safe_and_usable_through_dyn() {
    let db = TempDb::new("dyn");
    let mut boxed: Box<dyn Engine> = Box::new(open(&db));
    assert_eq!(boxed.mode(), Mode::Row);
    boxed
        .run_query("CREATE TABLE d(a INTEGER); INSERT INTO d VALUES (5)")
        .unwrap();
    let r: QueryResult = boxed.run_query("SELECT a FROM d").unwrap();
    assert_eq!(ints(&r.rows), vec![vec![5]]);
    assert!(matches!(boxed.stats(), FileStats::Row { .. }));
    assert!(!boxed.explain_opcodes("SELECT a FROM d").unwrap().is_empty());
}

#[test]
fn cell_converts_losslessly_from_the_row_value_model() {
    let text: std::rc::Rc<str> = std::rc::Rc::from("hi");
    let blob: std::rc::Rc<[u8]> = std::rc::Rc::from([9u8, 8].as_slice());
    assert_eq!(Cell::from(Value::Null), Cell::Null);
    assert_eq!(Cell::from(Value::Integer(-3)), Cell::Int(-3));
    assert_eq!(Cell::from(Value::Real(1.5)), Cell::Real(1.5));
    assert_eq!(Cell::from(Value::Text(text)), Cell::Text("hi".to_string()));
    assert_eq!(Cell::from(Value::Blob(blob)), Cell::Blob(vec![9, 8]));
    assert_eq!(Cell::from(&Value::Integer(4)), Cell::Int(4));
}

#[cfg(feature = "vm-batch")]
#[test]
fn cell_converts_losslessly_from_the_batch_value_model() {
    use db_core::vm::batch::Value as B;
    use std::borrow::Cow;
    assert_eq!(Cell::from(B::Null), Cell::Null);
    assert_eq!(Cell::from(B::Int(7)), Cell::Int(7));
    assert_eq!(Cell::from(B::Float(0.25)), Cell::Real(0.25));
    assert_eq!(Cell::from(B::Bool(true)), Cell::Bool(true));
    assert_eq!(
        Cell::from(B::Str(Cow::Borrowed("s"))),
        Cell::Text("s".to_string())
    );
}

#[test]
fn cell_display_matches_shell_conventions() {
    assert_eq!(Cell::Null.to_string(), "");
    assert_eq!(Cell::Int(42).to_string(), "42");
    assert_eq!(Cell::Real(2.5).to_string(), "2.5");
    assert_eq!(Cell::Real(3.0).to_string(), "3.0");
    assert_eq!(Cell::Bool(true).to_string(), "1");
    assert_eq!(Cell::Text("x y".to_string()).to_string(), "x y");
    assert_eq!(Cell::Blob(vec![0xAB, 0x01]).to_string(), "X'AB01'");
}

#[test]
fn engine_error_displays_kind_and_message() {
    let e = EngineError::new(ErrorKind::Compile, "no such table: t");
    assert_eq!(e.to_string(), "compile: no such table: t");
    let _: &dyn std::error::Error = &e;
}
