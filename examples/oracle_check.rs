//! One-off validation of #164's IS/BETWEEN/IN/LIKE/CASE/CAST/COLLATE
//! codegen against a real `sqlite3` CLI oracle. Not part of the test
//! suite -- run manually with `cargo run --example oracle_check
//! --all-features`. Requires `sqlite3` on PATH.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::io::Write;
use std::process::{Command, Stdio};

use db_core::codegen::row::dispatch::compile_statement;
use db_core::codegen::row::TableSchema;
use db_core::value::Value;
use db_core::vm::row::{execute, Cursor, EphemeralTableCursor, Vm};

fn schema() -> TableSchema {
    TableSchema {
        name: "t".to_string(),
        columns: vec!["a".into(), "b".into(), "n".into()],
        column_types: vec!["INTEGER".into(), "TEXT".into(), "INTEGER".into()],
        ..Default::default()
    }
}

/// (rowid, a, b, n) -- `n` includes a NULL row to exercise 3-valued
/// logic.
const ROWS: &[(i64, i64, &str, Option<i64>)] = &[
    (1, 1, "abc", Some(10)),
    (2, 2, "ABC", Some(20)),
    (3, 3, "xyz", None),
    (4, 4, "a%b", Some(5)),
];

const QUERIES: &[&str] = &[
    "SELECT a FROM t WHERE a IS 2",
    "SELECT a FROM t WHERE a IS NOT 2",
    "SELECT a FROM t WHERE n IS NULL",
    "SELECT a FROM t WHERE a BETWEEN 2 AND 3",
    "SELECT a FROM t WHERE a NOT BETWEEN 2 AND 3",
    "SELECT a FROM t WHERE n BETWEEN 1 AND 100",
    "SELECT a FROM t WHERE a IN (1, 3)",
    "SELECT a FROM t WHERE a NOT IN (1, 3)",
    "SELECT a FROM t WHERE n IN (10, 20)",
    "SELECT a FROM t WHERE n NOT IN (10, 20)",
    "SELECT a FROM t WHERE b LIKE 'a%'",
    "SELECT a FROM t WHERE b NOT LIKE 'a%'",
    "SELECT a FROM t WHERE b LIKE 'a\\%b' ESCAPE '\\'",
    "SELECT a FROM t WHERE b GLOB 'a?c'",
    "SELECT b FROM t WHERE b = 'ABC'",
    "SELECT b FROM t WHERE b = 'abc' COLLATE NOCASE",
    "SELECT b FROM t WHERE b COLLATE NOCASE = 'ABC'",
    "SELECT a FROM t WHERE (CASE WHEN a = 1 THEN 'one' WHEN a = 2 THEN 'two' ELSE 'other' END) = 'two'",
    "SELECT a FROM t WHERE (CASE a WHEN 1 THEN 'x' WHEN 2 THEN 'y' ELSE 'z' END) = 'y'",
    "SELECT a FROM t WHERE CAST(b AS INTEGER) = a",
    "SELECT a FROM t WHERE CAST(a AS TEXT) = '2'",
    "SELECT a FROM t WHERE UPPER(b) = 'ABC'",
    "SELECT a FROM t WHERE SUBSTR(b, 1, 2) = 'ab'",
    "SELECT a FROM t WHERE COALESCE(n, -1) = -1",
];

fn run_db_core(sql: &str) -> Vec<Vec<Value>> {
    let program = compile_statement(sql, &[schema()], &[]).unwrap_or_else(|e| {
        panic!("db-core failed to compile {sql:?}: {e}");
    });
    let mut vm = Vm::new();
    let mut table = EphemeralTableCursor::new();
    for &(rowid, a, b, n) in ROWS {
        let n_val = n.map_or(Value::Null, Value::Integer);
        table.insert(rowid, vec![Value::Integer(a), Value::Text(b.into()), n_val]);
    }
    vm.open_cursor(0, Box::new(table)).unwrap();
    execute(&mut vm, &program).unwrap_or_else(|e| panic!("db-core failed to run {sql:?}: {e}"))
}

fn run_sqlite3(sql: &str) -> String {
    let setup = "
        CREATE TABLE t(a INTEGER, b TEXT, n INTEGER);
        INSERT INTO t VALUES (1,'abc',10);
        INSERT INTO t VALUES (2,'ABC',20);
        INSERT INTO t VALUES (3,'xyz',NULL);
        INSERT INTO t VALUES (4,'a%b',5);
    ";
    let mut child = Command::new("sqlite3")
        .args([":memory:", "-csv", "-nullvalue", "NULL"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sqlite3 not on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{setup}\n{sql};\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    if !out.stderr.is_empty() {
        panic!(
            "sqlite3 error for {sql:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn db_core_as_csv(rows: &[Vec<Value>]) -> String {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => f.to_string(),
                    Value::Text(s) => s.to_string(),
                    Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn main() {
    let mut pass = 0;
    let mut fail = 0;
    for &sql in QUERIES {
        let ours = db_core_as_csv(&run_db_core(sql));
        let theirs = run_sqlite3(sql);
        if ours == theirs {
            pass += 1;
            println!("PASS  {sql}");
        } else {
            fail += 1;
            println!("FAIL  {sql}\n  db-core: {ours:?}\n  sqlite3: {theirs:?}");
        }
    }
    println!("\n{pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}
