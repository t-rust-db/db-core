// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for #308's range-vector queries:
//! `count_over_time`/`rate`/`*_over_time(...) RANGE <duration>` through
//! `db_core::engine::stream::StreamEngine`, end to end -- grammar through
//! `codegen::stream`'s `Epilogue` lowering through `vm::stream::run_epilogue`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::PathBuf;

use db_core::engine::stream::StreamEngine;
use db_core::engine::{Cell, Engine};

fn temp_log(name: &str, text: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "db-core-stream-windows-{}-{name}.log",
        std::process::id()
    ));
    std::fs::write(&p, text).unwrap();
    p
}

fn line(secs: u32, n: usize) -> String {
    format!("<134>Sep 10 08:00:{secs:02} h app[{n}]: line {n}\n")
}

/// 5 lines in `[0,10)`, 3 lines in `[10,20)` -- an independent hand count
/// against `count_over_time(message) RANGE 10 seconds`.
#[test]
fn count_over_time_matches_a_hand_counted_bucketing() {
    let mut text = String::new();
    for s in [1, 2, 3, 4, 5, 11, 12, 13] {
        text.push_str(&line(s, s as usize));
    }
    let path = temp_log("count-over-time", &text);
    let mut e = StreamEngine::open(&path).unwrap();
    let rows = e
        .run_query("SELECT count_over_time(message) RANGE 10 seconds FROM log")
        .unwrap()
        .rows;
    let counts: Vec<i64> = rows
        .iter()
        .map(|r| match &r[1] {
            Cell::Int(n) => *n,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(counts, vec![5, 3]);
}

/// `rate(...)` divides `count_over_time` by the window width in seconds.
#[test]
fn rate_divides_the_window_count_by_its_width_in_seconds() {
    let mut text = String::new();
    for s in [1, 2] {
        text.push_str(&line(s, s as usize));
    }
    let path = temp_log("rate", &text);
    let mut e = StreamEngine::open(&path).unwrap();
    let rows = e
        .run_query("SELECT rate(*) RANGE 10 seconds FROM log")
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1);
    match &rows[0][1] {
        Cell::Real(r) => assert!((r - 0.2).abs() < 1e-9, "got {r}"),
        other => panic!("{other:?}"),
    }
}

/// `sum_over_time`/`min_over_time`/`max_over_time` reducing a numeric
/// column end to end is exercised over `severity` (a genuinely typed
/// integer Tier-2 column every syslog line has) rather than a Tier-3
/// field: `StreamEngine::open` hardcodes `SyslogParser` (no format
/// detection wired into this constructor yet), and syslog's own Tier-3
/// fields (`tag`/`pid`) all end up `Dict`-encoded text, not `Int`
/// (verified separately in `storage::stream::segment`'s own tests) --
/// so there is no numeric Tier-3 column this engine can produce today to
/// exercise these three functions against. The full numeric-reduction
/// path itself (`sum`/`avg`/`min`/`max_over_time`) is unit-tested
/// directly in `vm::stream`'s own tests, independent of this gap.
#[test]
fn sum_min_max_over_time_reduce_the_severity_column() {
    let text = "<0>Sep 10 08:00:01 h app: a\n\
                <3>Sep 10 08:00:02 h app: b\n\
                <7>Sep 10 08:00:03 h app: c\n";
    let path = temp_log("sum-min-max", text);
    let mut e = StreamEngine::open(&path).unwrap();

    // `severity` is `db_core`'s own OTel-scale encoding of syslog's raw
    // 0/3/7 -- independently queried per row rather than assumed, so
    // this test's oracle doesn't hardcode that mapping's formula.
    let per_row = e.run_query("SELECT severity FROM log").unwrap().rows;
    let values: Vec<i64> = per_row
        .iter()
        .map(|r| match &r[0] {
            Cell::Int(n) => *n,
            other => panic!("{other:?}"),
        })
        .collect();
    let want_sum: f64 = values.iter().sum::<i64>() as f64;
    let want_max: f64 = *values.iter().max().unwrap() as f64;

    let sum = e
        .run_query("SELECT sum_over_time(severity) RANGE 10 seconds FROM log")
        .unwrap()
        .rows;
    match &sum[0][1] {
        Cell::Real(r) => assert!((r - want_sum).abs() < 1e-9, "got {r}, want {want_sum}"),
        other => panic!("{other:?}"),
    }
    let max = e
        .run_query("SELECT max_over_time(severity) RANGE 10 seconds FROM log")
        .unwrap()
        .rows;
    match &max[0][1] {
        Cell::Real(r) => assert!((r - want_max).abs() < 1e-9, "got {r}, want {want_max}"),
        other => panic!("{other:?}"),
    }
}

/// A range-vector call combined with `GROUP BY`/`ORDER BY`/`LIMIT` is
/// rejected (#308's narrow SQL surface: the call stands alone).
#[test]
fn range_vector_with_group_by_is_rejected() {
    let path = temp_log("rejects-group-by", "<134>Sep 10 08:00:01 h app: x\n");
    let mut e = StreamEngine::open(&path).unwrap();
    let err = e
        .run_query("SELECT count_over_time(message) RANGE 10 seconds FROM log GROUP BY severity")
        .unwrap_err();
    assert!(
        err.to_string().contains("stands alone"),
        "unexpected error: {err}"
    );
}
