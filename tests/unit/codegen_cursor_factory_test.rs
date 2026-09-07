// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! db-core#182 acceptance test: a `Vm` with only a `CursorFactory`
//! installed -- nothing pre-wired via `Vm::open_cursor` -- must run
//! `compile_statement`'s output for `SELECT` (plain scan, `JOIN`),
//! `INSERT`, `UPDATE`, and `DELETE`. Before #182, every one of these
//! programs relied on the caller pre-wiring its cursor slot ahead of
//! time; a `CursorFactory`-backed consumer (sqlite-rs) never gets a
//! chance to open anything a program doesn't itself ask for via
//! `OpenRead`/`OpenWrite`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use db_core::codegen::row::dispatch::compile_statement;
use db_core::codegen::row::TableSchema;
use db_core::vm::row::{execute, Cursor, CursorFactory, CursorFactoryError, Value, Vm};

type TableStore = Rc<RefCell<Vec<(i64, Vec<Value>)>>>;

/// A [`Cursor`] over a table shared by every cursor a [`TestFactory`]
/// opens against the same root page, so a write from one compiled
/// program (`INSERT`/`UPDATE`/`DELETE`) is visible to the next.
struct SharedTableCursor {
    store: TableStore,
    pos: Option<usize>,
}

impl Cursor for SharedTableCursor {
    fn rewind(&mut self) -> bool {
        self.pos = if self.store.borrow().is_empty() {
            None
        } else {
            Some(0)
        };
        self.pos.is_some()
    }

    fn next(&mut self) -> bool {
        let len = self.store.borrow().len();
        self.pos = match self.pos {
            Some(p) if p + 1 < len => Some(p + 1),
            _ => None,
        };
        self.pos.is_some()
    }

    fn column(&self, col: usize) -> Value {
        self.store.borrow()[self.pos.expect("no current row")].1[col].clone()
    }

    fn rowid(&self) -> i64 {
        self.store.borrow()[self.pos.expect("no current row")].0
    }

    fn seek(&mut self, rowid: i64) -> bool {
        self.pos = self.store.borrow().iter().position(|(r, _)| *r == rowid);
        self.pos.is_some()
    }

    fn insert(&mut self, rowid: i64, values: Vec<Value>) -> bool {
        let mut store = self.store.borrow_mut();
        store.retain(|(r, _)| *r != rowid);
        store.push((rowid, values));
        true
    }

    fn delete(&mut self) -> bool {
        let Some(pos) = self.pos.take() else {
            return false;
        };
        self.store.borrow_mut().remove(pos);
        true
    }

    fn next_rowid(&self) -> i64 {
        self.store
            .borrow()
            .iter()
            .map(|(r, _)| *r)
            .max()
            .unwrap_or(0)
            + 1
    }
}

/// Backs every table root page with its own [`TableStore`], created on
/// first open and reused by every later open against the same root --
/// the mechanism that lets a later `compile_statement` output (a second
/// `Vm::execute` call, same `Vm`) observe an earlier one's writes.
#[derive(Default)]
struct TestFactory {
    tables: HashMap<u32, TableStore>,
}

impl CursorFactory for TestFactory {
    fn open_read(&mut self, root: u32) -> Result<Box<dyn Cursor>, CursorFactoryError> {
        let store = self.tables.entry(root).or_default().clone();
        Ok(Box::new(SharedTableCursor { store, pos: None }))
    }
}

fn schema(name: &str, root_page: u32, columns: &[&str]) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        column_types: columns.iter().map(|_| String::new()).collect(),
        root_page,
        ..Default::default()
    }
}

fn run(vm: &mut Vm, sql: &str, schemas: &[TableSchema]) -> Vec<Vec<Value>> {
    let program = compile_statement(sql, schemas).unwrap();
    execute(vm, &program).unwrap()
}

#[test]
fn select_scan_runs_against_a_cursor_factory_alone() {
    let mut vm = Vm::new();
    vm.set_cursor_factory(Box::new(TestFactory::default()));
    let schemas = [schema("t", 2, &["a"])];

    run(&mut vm, "INSERT INTO t VALUES (1)", &schemas);
    run(&mut vm, "INSERT INTO t VALUES (2)", &schemas);
    let rows = run(&mut vm, "SELECT a FROM t WHERE a > 1", &schemas);

    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn select_join_runs_against_a_cursor_factory_alone() {
    let mut vm = Vm::new();
    vm.set_cursor_factory(Box::new(TestFactory::default()));
    let schemas = [schema("t", 2, &["a"]), schema("u", 3, &["b", "c"])];

    run(&mut vm, "INSERT INTO t VALUES (1)", &schemas);
    run(&mut vm, "INSERT INTO u VALUES (1, 100)", &schemas);
    let rows = run(
        &mut vm,
        "SELECT t.a, u.c FROM t JOIN u ON t.a = u.b",
        &schemas,
    );

    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(100)]]);
}

#[test]
fn update_runs_against_a_cursor_factory_alone() {
    let mut vm = Vm::new();
    vm.set_cursor_factory(Box::new(TestFactory::default()));
    let schemas = [schema("t", 2, &["a", "b"])];

    run(&mut vm, "INSERT INTO t VALUES (1, 10)", &schemas);
    run(&mut vm, "UPDATE t SET b = 99 WHERE a = 1", &schemas);
    let rows = run(&mut vm, "SELECT b FROM t", &schemas);

    assert_eq!(rows, vec![vec![Value::Integer(99)]]);
}

#[test]
fn delete_runs_against_a_cursor_factory_alone() {
    let mut vm = Vm::new();
    vm.set_cursor_factory(Box::new(TestFactory::default()));
    let schemas = [schema("t", 2, &["a"])];

    run(&mut vm, "INSERT INTO t VALUES (1)", &schemas);
    run(&mut vm, "INSERT INTO t VALUES (2)", &schemas);
    run(&mut vm, "DELETE FROM t WHERE a = 1", &schemas);
    let rows = run(&mut vm, "SELECT a FROM t", &schemas);

    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}
