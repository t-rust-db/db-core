//! Parser phase micro-benchmarks: tokenize + `parser::row::parse_select`
//! over a fixed corpus of increasing complexity, so a regression
//! localizes to the parser rather than showing up only as a slower
//! end-to-end query in the separate benchmark repo. **Report only** --
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

fn main() {
    let deep = deep_nesting();
    let corpus = [
        ("short", SHORT),
        ("medium", MEDIUM),
        ("deep_nesting", deep.as_str()),
    ];
    let mut report = common::Report::new("parser");
    for (label, sql) in corpus {
        report.bench(&format!("parser/tokenize/{label}"), || {
            Tokenizer::tokenize(black_box(sql))
        });
    }
    for (label, sql) in corpus {
        report.bench(&format!("parser/parse_select/{label}"), || {
            parse_select(black_box(sql))
        });
    }
    report.finish();
}
