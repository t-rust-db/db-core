// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `Engine::compile_predicate`/`CompiledPredicate::eval`
//! (#369): compiling a bare boolean expression -- `WHERE`'s own grammar --
//! against a schema, then evaluating it repeatedly against rows of `Cell`s
//! without any file I/O or engine re-query.
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
use db_core::engine::{Cell, Engine, ErrorKind};

const FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";

/// A writable copy of the fixture, removed on drop.
struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-engine-predicate-{label}-{}-{}.db",
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

/// `TempDb` must outlive the returned `RowEngine` so its `Drop` doesn't
/// remove the file out from under an engine still holding it open.
fn engine_with_events_table(label: &str) -> (RowEngine, TempDb) {
    let db = TempDb::new(label);
    let mut engine = RowEngine::open(db.path()).expect("open fixture copy");
    engine
        .run_query("CREATE TABLE events(device INTEGER, event TEXT, severity_text TEXT, raw TEXT)")
        .unwrap();
    (engine, db)
}

#[test]
fn compiles_and_evaluates_a_comparison_against_a_matching_row() {
    let (engine, _db) = engine_with_events_table("comparison");
    let predicate = engine.compile_predicate("severity_text = 'ERROR'").unwrap();

    let columns = vec!["severity_text".to_string()];
    let row = vec![Cell::Text("ERROR".to_string())];
    assert!(predicate.eval(&row, &columns).unwrap());

    let row = vec![Cell::Text("INFO".to_string())];
    assert!(!predicate.eval(&row, &columns).unwrap());
}

#[test]
fn compiles_and_evaluates_like_against_a_row() {
    let (engine, _db) = engine_with_events_table("like");
    let predicate = engine.compile_predicate("raw LIKE '%err%'").unwrap();

    let columns = vec!["raw".to_string()];
    assert!(predicate
        .eval(&[Cell::Text("an err occurred".to_string())], &columns)
        .unwrap());
    assert!(!predicate
        .eval(&[Cell::Text("all fine".to_string())], &columns)
        .unwrap());
}

#[test]
fn compiles_and_evaluates_an_and_expression_across_two_columns() {
    let (engine, _db) = engine_with_events_table("and-expr");
    let predicate = engine
        .compile_predicate("device = 2 AND event = 'battery'")
        .unwrap();

    let columns = vec!["device".to_string(), "event".to_string()];
    let row = vec![Cell::Int(2), Cell::Text("battery".to_string())];
    assert!(predicate.eval(&row, &columns).unwrap());

    let row = vec![Cell::Int(3), Cell::Text("battery".to_string())];
    assert!(!predicate.eval(&row, &columns).unwrap());
}

#[test]
fn a_column_the_expression_references_but_the_schema_lacks_is_a_compile_error() {
    let (engine, _db) = engine_with_events_table("unknown-schema-col");
    let err = engine.compile_predicate("no_such_column = 1").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn a_column_the_expression_references_but_the_row_lacks_is_an_eval_error() {
    let (engine, _db) = engine_with_events_table("unknown-row-col");
    let predicate = engine.compile_predicate("severity_text = 'ERROR'").unwrap();

    // `columns` doesn't carry `severity_text` at all -- a clear error,
    // not a silent `false`.
    let err = predicate
        .eval(&[Cell::Text("x".to_string())], &["raw".to_string()])
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn eval_does_not_touch_the_file() {
    let (engine, _db) = engine_with_events_table("no-file-io");
    let predicate = engine.compile_predicate("device = 1").unwrap();

    // Drop the engine (closing the file) before evaluating -- `eval`
    // must not need it.
    drop(engine);

    let columns = vec!["device".to_string()];
    assert!(predicate.eval(&[Cell::Int(1)], &columns).unwrap());
}
