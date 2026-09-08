// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box compile-then-execute round trips through `codegen::row::
//! dispatch::compile_statement`, the entry point a downstream consumer
//! (sqlite-rs) actually calls: SQL string in, `Program` out, then run
//! against a real cursor and check the resulting rows -- not the
//! opcodes. `dispatch.rs`'s own inline `end_to_end` module already
//! covers a plain `SELECT`/`JOIN`/`INSERT`/`UPDATE`/`DELETE`; this suite
//! fills the gap around it: `GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`+
//! `OFFSET`, and the richer `WHERE`/`SELECT`-list expression shapes
//! (`CASE`, `IN`, `BETWEEN`, a function call, a scalar subquery) that
//! `codegen::row::mod`'s `walk_columns`/`walk_subexprs` only cover
//! exhaustively when a query actually contains them -- both were among
//! the weakest-covered files in the crate (71.5%/79.3% lines) before
//! this suite existed.

#![allow(clippy::unwrap_used, clippy::panic)]

use db_core::codegen::row::dispatch::compile_statement;
use db_core::codegen::row::TableSchema;
use db_core::vm::row::{execute, Cursor, EphemeralTableCursor, Opcode, Program, Value, Vm};

fn schema(name: &str, columns: &[&str]) -> TableSchema {
    schema_with_root(name, columns, 2)
}

fn schema_with_root(name: &str, columns: &[&str], root_page: u32) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        column_types: columns.iter().map(|_| String::new()).collect(),
        root_page,
        sql: format!("CREATE TABLE {name} ({})", columns.join(", ")),
        ..Default::default()
    }
}

/// The cursor slot the compiler assigned to the table rooted at
/// `root_page` -- a query with more than one scan (a `JOIN`, a
/// subquery, ...) doesn't allocate cursor slots in catalog order, so
/// this reads it back from the compiled program's own `OpenRead`
/// instead of assuming a slot number.
fn cursor_slot_for_root(program: &Program, root_page: u32) -> i32 {
    program
        .instructions
        .iter()
        .find(|i| i.opcode == Opcode::OpenRead && i.p2 == i32::try_from(root_page).unwrap())
        .map(|i| i.p1)
        .unwrap_or_else(|| panic!("no OpenRead for root page {root_page} in {program:?}"))
}

fn run(schemas: &[TableSchema], sql: &str, seed: Vec<(i64, Vec<Value>)>) -> Vec<Vec<Value>> {
    let program = compile_statement(sql, schemas, &[]).unwrap();
    let mut vm = Vm::new();
    let mut table = EphemeralTableCursor::new();
    for (rowid, values) in seed {
        table.insert(rowid, values);
    }
    vm.open_cursor(0, Box::new(table)).unwrap();
    execute(&mut vm, &program).unwrap()
}

#[test]
fn group_by_with_having_and_an_aggregate() {
    let rows = run(
        &[schema("t", &["k", "v"])],
        "SELECT k, SUM(v) FROM t GROUP BY k HAVING SUM(v) > 5",
        vec![
            (1, vec![Value::Integer(1), Value::Integer(10)]),
            (2, vec![Value::Integer(2), Value::Integer(1)]),
            (3, vec![Value::Integer(2), Value::Integer(2)]),
        ],
    );
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(10)]]);
}

#[test]
fn order_by_desc_with_limit_and_offset() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT a FROM t ORDER BY a DESC LIMIT 2 OFFSET 1",
        vec![
            (1, vec![Value::Integer(1)]),
            (2, vec![Value::Integer(4)]),
            (3, vec![Value::Integer(2)]),
            (4, vec![Value::Integer(3)]),
        ],
    );
    assert_eq!(rows, vec![vec![Value::Integer(3)], vec![Value::Integer(2)]]);
}

#[test]
fn where_clause_with_between_and_in() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT a FROM t WHERE a BETWEEN 2 AND 4 AND a IN (2, 3)",
        vec![
            (1, vec![Value::Integer(1)]),
            (2, vec![Value::Integer(2)]),
            (3, vec![Value::Integer(3)]),
            (4, vec![Value::Integer(5)]),
        ],
    );
    assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
}

#[test]
fn select_list_case_expression_and_function_call() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT CASE WHEN a > 1 THEN 'big' ELSE 'small' END, abs(a) FROM t",
        vec![(1, vec![Value::Integer(-1)]), (2, vec![Value::Integer(2)])],
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("small".into()), Value::Integer(1)],
            vec![Value::Text("big".into()), Value::Integer(2)],
        ]
    );
}

#[test]
fn where_clause_scalar_subquery() {
    let schemas = [
        schema_with_root("t", &["a"], 2),
        schema_with_root("bound", &["n"], 3),
    ];
    let program = compile_statement(
        "SELECT a FROM t WHERE a > (SELECT n FROM bound)",
        &schemas,
        &[],
    )
    .unwrap();

    // The outer table is the caller's pre-wired cursor 0 (no `OpenRead`
    // of its own -- codegen's compiled-ahead-of-time path); only the
    // nested subquery scan gets an explicit `OpenRead`.
    let mut vm = Vm::new();
    let mut t = EphemeralTableCursor::new();
    t.insert(1, vec![Value::Integer(5)]);
    t.insert(2, vec![Value::Integer(15)]);
    vm.open_cursor(0, Box::new(t)).unwrap();
    let mut bound = EphemeralTableCursor::new();
    bound.insert(1, vec![Value::Integer(10)]);
    vm.open_cursor(cursor_slot_for_root(&program, 3), Box::new(bound))
        .unwrap();

    let rows = execute(&mut vm, &program).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(15)]]);
}

#[test]
fn insert_then_group_by_select_sees_the_new_row() {
    let schemas = [schema("t", &["k", "v"])];
    let insert = compile_statement("INSERT INTO t VALUES (1, 7)", &schemas, &[]).unwrap();
    let mut vm = Vm::new();
    vm.open_cursor(0, Box::new(EphemeralTableCursor::new()))
        .unwrap();
    execute(&mut vm, &insert).unwrap();

    let select = compile_statement("SELECT k, SUM(v) FROM t GROUP BY k", &schemas, &[]).unwrap();
    let rows = execute(&mut vm, &select).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(7)]]);
}

#[test]
fn compile_statement_reports_an_unknown_table() {
    let err = compile_statement("SELECT a FROM missing", &[], &[]).unwrap_err();
    assert!(format!("{err:?}").contains("missing"));
}
