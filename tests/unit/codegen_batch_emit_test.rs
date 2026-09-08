// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `codegen::batch::emit`'s public entry points --
//! the ahead-of-time Rust-source emitter -- which had zero `tests/unit`
//! references before this (db-core#223): `generate`, `render_flat`,
//! `render_joined`, `render_semi_join`, `render_windowed`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use db_core::codegen::batch::emit::{
    generate, render_flat, render_joined, render_semi_join, render_windowed,
};
use db_core::codegen::batch::{compile, output_column_names};
use db_core::parser::column::parse;

#[test]
fn generate_renders_a_flat_query_to_a_const_program() {
    let src = generate("column_rs", "SELECT a FROM t").unwrap();
    assert!(src.contains("const PROGRAM"));
    assert!(src.contains("const COLUMNS"));
    assert!(src.contains("fn main"));
}

#[test]
fn generate_renders_a_join_query_via_execute_joined() {
    let src = generate("column_rs", "SELECT a.x, b.y FROM a JOIN b ON a.id = b.fk").unwrap();
    assert!(src.contains("execute_joined"));
}

#[test]
fn generate_renders_a_window_query_via_execute_windowed() {
    let src = generate(
        "column_rs",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t",
    )
    .unwrap();
    assert!(src.contains("execute_windowed"));
}

#[test]
fn render_flat_embeds_the_program_and_column_names() {
    let select = parse("SELECT a, b FROM t").unwrap();
    let program = compile(&select);
    let columns = output_column_names(&select);
    let src = render_flat("column_rs", "SELECT a, b FROM t", "t", &program, &columns);
    assert!(src.contains("const PROGRAM"));
    assert!(src.contains("\"a\""));
    assert!(src.contains("\"b\""));
    assert!(src.contains("Table: t"));
}

#[test]
fn render_joined_embeds_a_reconstructed_select_and_execute_joined_call() {
    let select = parse("SELECT a.x, b.y FROM a JOIN b ON a.id = b.fk").unwrap();
    let src = render_joined(
        "column_rs",
        "SELECT a.x, b.y FROM a JOIN b ON a.id = b.fk",
        &select,
    );
    assert!(src.contains("execute_joined"));
    assert!(src.contains("Select"));
}

#[test]
fn render_semi_join_embeds_the_subquery_table_and_execute_semi_join_call() {
    let select = parse("SELECT x FROM a WHERE id IN (SELECT id FROM b)").unwrap();
    let src = render_semi_join(
        "column_rs",
        "SELECT x FROM a WHERE id IN (SELECT id FROM b)",
        &select,
        "b",
    );
    assert!(src.contains("execute_semi_join"));
}

#[test]
fn render_windowed_embeds_a_reconstructed_select_and_execute_windowed_call() {
    let select = parse("SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t").unwrap();
    let src = render_windowed(
        "column_rs",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t",
        &select,
    );
    assert!(src.contains("execute_windowed"));
    assert!(src.contains("Table: t"));
}
