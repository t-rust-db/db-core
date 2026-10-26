// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `CASE`, `COALESCE`, `IFNULL` and `NULLIF` (#725) through the stream
//! engine, which shares the column dialect's parser and the batch VM. The
//! stream-specific part is the severity-name rewrite (`severity >= 'WARN'`),
//! which has to reach a condition inside a `CASE`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::Path;

use db_core::engine::stream::StreamEngine;
use db_core::engine::{Cell, Engine};

const FIXTURE: &str = "tests/fixtures/stream/syslog-1k.log";

fn open() -> StreamEngine {
    StreamEngine::open(Path::new(FIXTURE)).expect("open fixture")
}

fn rows(e: &mut StreamEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn int(e: &mut StreamEngine, sql: &str) -> i64 {
    match &rows(e, sql)[0][0] {
        Cell::Int(n) => *n,
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_severity_name_inside_a_case_condition_is_rewritten() {
    let mut e = open();
    let warn = rows(&mut e, "SELECT * FROM log WHERE severity >= 'WARN'").len() as i64;
    assert!(warn > 0 && warn < 1000);
    assert_eq!(
        int(
            &mut e,
            "SELECT count(*) FROM log WHERE CASE WHEN severity >= 'WARN' THEN true ELSE false END"
        ),
        warn
    );
    assert_eq!(
        int(
            &mut e,
            "SELECT sum(CASE WHEN severity >= 'WARN' THEN 1 ELSE 0 END) FROM log"
        ),
        warn
    );
    // A simple CASE on the severity code compares with a name as `=` does.
    assert_eq!(
        int(
            &mut e,
            "SELECT count(CASE severity WHEN 'ERROR' THEN 1 END) FROM log"
        ),
        int(&mut e, "SELECT count(*) FROM log WHERE severity = 'ERROR'")
    );
}

#[test]
fn conditional_aggregation_groups_like_filtered_counts() {
    let mut e = open();
    let per_facility = rows(
        &mut e,
        "SELECT facility, count(CASE WHEN severity >= 'WARN' THEN 1 END) FROM log GROUP BY facility ORDER BY facility",
    );
    assert!(per_facility.len() > 1);
    for row in per_facility {
        let (Cell::Text(facility), Cell::Int(n)) = (&row[0], &row[1]) else {
            panic!("{row:?}");
        };
        assert_eq!(
            *n,
            int(
                &mut e,
                &format!(
                    "SELECT count(*) FROM log WHERE facility = '{facility}' AND severity >= 'WARN'"
                )
            ),
            "{facility}"
        );
    }
}

#[test]
fn coalesce_ifnull_and_nullif_over_a_stream() {
    let mut e = open();
    let total = int(&mut e, "SELECT count(*) FROM log");
    // Nothing in a syslog facility is NULL, so COALESCE is the column.
    assert_eq!(
        int(
            &mut e,
            "SELECT count(*) FROM log WHERE coalesce(facility, 'none') = facility"
        ),
        total
    );
    assert_eq!(
        int(
            &mut e,
            "SELECT count(*) FROM log WHERE ifnull(facility, 'none') = facility"
        ),
        total
    );
    // NULLIF turns one facility into NULL, which count() skips.
    let first = match &rows(&mut e, "SELECT facility FROM log LIMIT 1")[0][0] {
        Cell::Text(f) => f.clone(),
        other => panic!("{other:?}"),
    };
    let same = int(
        &mut e,
        &format!("SELECT count(*) FROM log WHERE facility = '{first}'"),
    );
    assert_eq!(
        int(
            &mut e,
            &format!("SELECT count(nullif(facility, '{first}')) FROM log")
        ),
        total - same
    );
}
