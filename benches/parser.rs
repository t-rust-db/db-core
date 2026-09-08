//! Parser phase micro-benchmarks (db-core#224): tokenize +
//! `parser::row::parse_select` over a fixed corpus of increasing
//! complexity, so a regression localizes to the parser rather than
//! showing up only as a slower end-to-end query in the separate
//! benchmark repo. **Report only** -- `make perf`, not a CI gate.

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
use db_core::parser::row::parse_select;
use db_core::parser::row::tokenizer::Tokenizer;

const SHORT: &str = "SELECT a FROM t";

const MEDIUM: &str = "SELECT a, b, c FROM t WHERE a > 1 AND b < 10 OR c = 'x' \
                       GROUP BY a, b HAVING COUNT(*) > 1 ORDER BY a DESC LIMIT 10";

/// A deeply left-nested arithmetic expression, close to the grammar's
/// own recursion guard (200) without tripping it.
fn deep_nesting() -> String {
    let mut sql = "SELECT ".to_string();
    sql.push('1');
    for _ in 0..190 {
        sql.push_str(" + 1");
    }
    sql.push_str(" FROM t");
    sql
}

fn bench_tokenize(c: &mut Criterion) {
    let deep = deep_nesting();
    let mut group = c.benchmark_group("parser/tokenize");
    for (label, sql) in [
        ("short", SHORT),
        ("medium", MEDIUM),
        ("deep_nesting", deep.as_str()),
    ] {
        group.bench_function(label, |b| {
            b.iter(|| Tokenizer::tokenize(black_box(sql)));
        });
    }
    group.finish();
}

fn bench_parse_select(c: &mut Criterion) {
    let deep = deep_nesting();
    let mut group = c.benchmark_group("parser/parse_select");
    for (label, sql) in [
        ("short", SHORT),
        ("medium", MEDIUM),
        ("deep_nesting", deep.as_str()),
    ] {
        group.bench_function(label, |b| {
            b.iter(|| parse_select(black_box(sql)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_tokenize, bench_parse_select);
criterion_main!(benches);
