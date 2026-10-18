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
    generate, render_flat, render_joined, render_semi_join, render_windowed, EmitError,
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
    let program = compile(&select).unwrap();
    let columns = output_column_names(&select);
    let src = render_flat("column_rs", "SELECT a, b FROM t", "t", &program, &columns);
    assert!(src.contains("const PROGRAM"));
    assert!(src.contains("\"a\""));
    assert!(src.contains("\"b\""));
    assert!(src.contains("Table: t"));
}

#[test]
fn render_flat_embeds_the_post_aggregation_projection() {
    // #623: `SUM(x) + COUNT(*)` merges a plain `Sum` and `Count`, and
    // `Combine`'s `project` program adds them -- the AOT emitter renders
    // that nested program inside the `Combine` literal.
    let select = parse("SELECT SUM(amount) + COUNT(*) FROM t").unwrap();
    let program = compile(&select).unwrap();
    let columns = output_column_names(&select);
    let src = render_flat(
        "column_rs",
        "SELECT SUM(amount) + COUNT(*) FROM t",
        "t",
        &program,
        &columns,
    );
    assert!(src.contains("AggPart::Sum, AggPart::Count"), "{src}");
    assert!(
        src.contains("project: std::borrow::Cow::Borrowed(&[Opcode::LoadColumn"),
        "{src}"
    );
    assert!(src.contains("op: MapOp::Add"), "{src}");
    assert!(src.contains("Opcode::Emit"), "{src}");
}

#[test]
fn render_flat_embeds_an_integer_literal_in_the_projection() {
    // #623: `AVG(x) * 2` merges an `Avg`; the literal is a typed constant
    // in the projection, not an `f64` operand.
    let select = parse("SELECT AVG(amount) * 2 FROM t").unwrap();
    let program = compile(&select).unwrap();
    let columns = output_column_names(&select);
    let src = render_flat(
        "column_rs",
        "SELECT AVG(amount) * 2 FROM t",
        "t",
        &program,
        &columns,
    );
    assert!(src.contains("AggPart::Avg("), "{src}");
    assert!(src.contains("value: Value::Int(2)"), "{src}");
}

#[test]
fn render_flat_embeds_hidden_and_expr_agg_parts_of_a_hand_built_program() {
    // The planner no longer emits `Hidden`/`Expr` parts (#623), but the VM
    // still runs them, so a hand-built program must still render.
    use db_core::vm::batch::{AggOperand, AggPart, HiddenPart, MapOp, Opcode, Program};
    let program = Program::from_opcodes([Opcode::Combine {
        agg_parts: vec![
            AggPart::Hidden(HiddenPart::Min),
            AggPart::Hidden(HiddenPart::Max),
            AggPart::Expr(MapOp::Add, AggOperand::Slot(0), AggOperand::Literal(2.0)),
            AggPart::Expr(MapOp::Mul, AggOperand::Avg(0, 1), AggOperand::Slot(1)),
        ]
        .into(),
        num_group_keys: 0,
        distinct: false,
        output_order: Vec::new().into(),
        project: Vec::new().into(),
    }]);
    let src = render_flat("column_rs", "-- hand-built", "t", &program, &[]);
    assert!(src.contains("AggPart::Hidden(HiddenPart::Min)"), "{src}");
    assert!(src.contains("AggPart::Hidden(HiddenPart::Max)"), "{src}");
    assert!(src.contains("AggPart::Expr(MapOp::Add"), "{src}");
    assert!(src.contains("AggOperand::Slot("), "{src}");
    assert!(src.contains("AggOperand::Literal(2"), "{src}");
    assert!(src.contains("AggOperand::Avg("), "{src}");
    assert!(
        src.contains("project: std::borrow::Cow::Borrowed(&[])"),
        "{src}"
    );
}

#[test]
fn generate_refuses_group_by_an_alias() {
    // #623: without a table schema an alias cannot be told from a column.
    let err = generate(
        "column_rs",
        "SELECT amount * 10 AS k, COUNT(*) FROM t GROUP BY k",
    )
    .unwrap_err();
    assert!(matches!(err, EmitError::Unsupported(m) if m.contains("GROUP BY a SELECT alias")));
    // Grouping by a column with computed select items still emits.
    assert!(generate(
        "column_rs",
        "SELECT region || '!', COUNT(*) FROM t GROUP BY region"
    )
    .is_ok());
}

#[test]
fn render_joined_embeds_a_reconstructed_select_and_execute_joined_call() {
    let select = parse("SELECT a.x, b.y FROM a JOIN b ON a.id = b.fk").unwrap();
    let src = render_joined(
        "column_rs",
        "SELECT a.x, b.y FROM a JOIN b ON a.id = b.fk",
        &select,
    )
    .unwrap();
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
    )
    .unwrap();
    assert!(src.contains("execute_semi_join"));
}

#[test]
fn render_windowed_embeds_a_reconstructed_select_and_execute_windowed_call() {
    let select = parse("SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t").unwrap();
    let src = render_windowed(
        "column_rs",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t",
        &select,
    )
    .unwrap();
    assert!(src.contains("execute_windowed"));
    assert!(src.contains("Table: t"));
}

/// db-core#232: a query with no table to open is refused instead of being
/// rendered as `TABLE = ""` (a generated program that could never bind a
/// file).
#[test]
fn generate_refuses_a_select_without_a_from_clause() {
    assert!(generate("demo", "SELECT 1").is_err());
}

#[test]
fn a_plan_error_surfaces_through_emit_error() {
    let e = EmitError::Plan(db_core::codegen::batch::PlanError::NoJoinClause);
    assert_eq!(e.to_string(), "compile_join requires a JOIN clause");
}

#[test]
fn an_unsupported_emit_error_names_what_is_missing() {
    let e = EmitError::Unsupported("window frames");
    assert_eq!(e.to_string(), "codegen does not support window frames yet");
}
