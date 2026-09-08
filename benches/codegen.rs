//! Codegen (planner) phase micro-benchmarks:
//! `codegen::row::select::compile_select_with_catalog` and
//! `codegen::batch::compile`, run over an already-parsed AST so the
//! measurement isolates the planner from tokenizing/parsing (see the
//! `parser` bench for that phase). Also benchmarks
//! `codegen::row::dispatch::compile_statement` (parse+plan together)
//! for comparison against the pre-parsed path. **Report only** --
//! `make perf`, not a CI gate (ADR 0015, tier 6).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    dead_code,
    reason = "benches/ is unconstrained like tests/ (ADR 0015, tier 6)"
)]

mod common;

use std::hint::black_box;

use db_core::codegen::batch::compile as compile_batch;
use db_core::codegen::row::dispatch::compile_statement;
use db_core::codegen::row::select::compile_select_with_catalog;
use db_core::parser::row::ParseOutcome;
use db_core::schema::TableSchema;

const SQL: &str = "SELECT a, b FROM t WHERE a > 1 ORDER BY b LIMIT 10";

fn schema() -> TableSchema {
    TableSchema {
        name: "t".to_string(),
        root_page: 2,
        columns: vec!["a".to_string(), "b".to_string()],
        column_types: vec!["INTEGER".to_string(), "INTEGER".to_string()],
        ..Default::default()
    }
}

fn main() {
    let select = match db_core::parser::row::parse_select(SQL) {
        ParseOutcome::Accepted(select) => *select,
        other => panic!("fixed SQL failed to parse: {other:?}"),
    };
    let batch_select =
        db_core::parser::column::parse(SQL).expect("fixed SQL passes the column validator");
    let schema = schema();
    let schemas = [schema.clone()];

    let mut report = common::Report::new("codegen");
    report.bench("codegen/compile_select_with_catalog", || {
        compile_select_with_catalog(black_box(&select), &schema, &[])
    });
    report.bench("codegen/compile_batch", || {
        compile_batch(black_box(&batch_select))
    });
    report.bench("codegen/compile_statement (parse+plan)", || {
        compile_statement(black_box(SQL), &schemas, &[])
    });
    report.finish();
}
