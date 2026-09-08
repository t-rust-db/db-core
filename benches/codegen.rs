//! Codegen (planner) phase micro-benchmarks (db-core#224):
//! `codegen::row::select::compile_select_with_catalog` and
//! `codegen::batch::compile`, run over an already-parsed AST so the
//! measurement isolates the planner from tokenizing/parsing (see
//! `parser` bench for that phase). Also benchmarks
//! `codegen::row::dispatch::compile_statement` (parse+plan together)
//! for comparison against the pre-parsed path. **Report only** --
//! `make perf`, not a CI gate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "benches/ is unconstrained like tests/ (db-core#224); criterion's own timing loop needs unwrap/index freely"
)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
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

fn bench_compile_select_with_catalog(c: &mut Criterion) {
    let select = match db_core::parser::row::parse_select(SQL) {
        ParseOutcome::Accepted(select) => *select,
        other => panic!("fixed SQL failed to parse: {other:?}"),
    };
    let schema = schema();
    c.bench_function("codegen/compile_select_with_catalog", |b| {
        b.iter(|| compile_select_with_catalog(black_box(&select), &schema, &[]));
    });
}

fn bench_compile_batch(c: &mut Criterion) {
    let select =
        db_core::parser::column::parse(SQL).expect("fixed SQL passes the column validator");
    c.bench_function("codegen/compile_batch", |b| {
        b.iter(|| compile_batch(black_box(&select)));
    });
}

fn bench_compile_statement_full_pipeline(c: &mut Criterion) {
    let schema = schema();
    let schemas = [schema];
    c.bench_function("codegen/compile_statement (parse+plan)", |b| {
        b.iter(|| compile_statement(black_box(SQL), &schemas, &[]));
    });
}

criterion_group!(
    benches,
    bench_compile_select_with_catalog,
    bench_compile_batch,
    bench_compile_statement_full_pipeline
);
criterion_main!(benches);
