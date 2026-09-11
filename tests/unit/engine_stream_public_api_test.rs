// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `db_core::engine::stream::StreamEngine` (#305): one
//! syslog file as table `log` through the `Engine` seam, including as
//! `Box<dyn Engine>`. The fixture is `tests/fixtures/stream/syslog-1k.log`
//! (1000 RFC 3164 lines from loglume's `gen_syslog.py --seed 304`). The
//! oracle for every count is an independent re-parse of the PRI field in
//! this file, so the test does not trust the engine's own parser.
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

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use db_core::engine::stream::{StreamEngine, TABLE};
use db_core::engine::{Cell, Engine, ErrorKind, FileStats, Mode};
use db_core::storage::stream::ColumnRequest;
use db_core::vm::batch::Source;

const FIXTURE: &str = "tests/fixtures/stream/syslog-1k.log";

fn open() -> StreamEngine {
    StreamEngine::open(Path::new(FIXTURE)).expect("open fixture")
}

fn rows(e: &mut StreamEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

/// Independent oracle: `(facility_name, syslog_severity)` per line.
fn oracle() -> Vec<(&'static str, u8)> {
    const FAC: [&str; 24] = [
        "kern", "user", "mail", "daemon", "auth", "syslog", "lpr", "news", "uucp", "cron",
        "authpriv", "ftp", "", "", "", "", "local0", "local1", "local2", "local3", "local4",
        "local5", "local6", "local7",
    ];
    std::fs::read_to_string(FIXTURE)
        .unwrap()
        .lines()
        .map(|l| {
            let end = l.find('>').unwrap();
            let pri: u8 = l[1..end].parse().unwrap();
            (FAC[(pri >> 3) as usize], pri & 7)
        })
        .collect()
}

/// syslog severity 0..=4 (emerg..warning) is WARN or worse in the OTel scale.
fn is_warn_or_worse(sev: u8) -> bool {
    sev <= 4
}

#[test]
fn opens_as_stream_mode_with_the_whole_fixture_hot() {
    let e = open();
    assert_eq!(e.mode(), Mode::Stream);
    match e.stats() {
        FileStats::Stream {
            bytes_parsed,
            lines,
        } => {
            assert_eq!(lines, 1000);
            assert!(bytes_parsed >= 70_000, "read {bytes_parsed} bytes");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn group_by_facility_matches_the_oracle() {
    let mut e = open();
    let got: BTreeMap<String, i64> = rows(
        &mut e,
        "SELECT facility, count(*) FROM log GROUP BY facility ORDER BY facility",
    )
    .into_iter()
    .map(|r| match (&r[0], &r[1]) {
        (Cell::Text(f), Cell::Int(n)) => (f.clone(), *n),
        other => panic!("{other:?}"),
    })
    .collect();
    let mut want: BTreeMap<String, i64> = BTreeMap::new();
    for (f, _) in oracle() {
        *want.entry(f.to_string()).or_default() += 1;
    }
    assert_eq!(got, want);
}

#[test]
fn severity_name_literal_orders_correctly() {
    let mut e = open();
    let want = oracle()
        .iter()
        .filter(|(_, s)| is_warn_or_worse(*s))
        .count();
    let got = rows(&mut e, "SELECT * FROM log WHERE severity >= 'WARN'").len();
    assert_eq!(got, want);
    assert!(got > 0 && got < 1000);
    // ERROR sorts before WARN as text; as a code it must be included.
    let errors = rows(&mut e, "SELECT * FROM log WHERE severity = 'ERROR'").len();
    let want_err = oracle().iter().filter(|(_, s)| *s == 3).count();
    assert_eq!(errors, want_err);
    // The numeric form is the same predicate.
    assert_eq!(
        rows(&mut e, "SELECT * FROM log WHERE severity >= 13").len(),
        want
    );
}

#[test]
fn severity_and_facility_conjunction() {
    let mut e = open();
    let want = oracle()
        .iter()
        .filter(|(f, s)| *f == "kern" && is_warn_or_worse(*s))
        .count();
    let got = rows(
        &mut e,
        "SELECT * FROM log WHERE severity >= 'WARN' AND facility = 'kern'",
    );
    assert_eq!(got.len(), want);
    // `SELECT *` yields the declared column order.
    let cols = e
        .run_query("SELECT * FROM log WHERE severity >= 'FATAL'")
        .unwrap()
        .columns;
    assert_eq!(
        &cols[..7],
        &[
            "timestamp",
            "observed_ts",
            "severity",
            "severity_text",
            "facility",
            "message",
            "raw"
        ]
    );
    assert!(cols.iter().any(|c| c == "tag"), "{cols:?}");
}

#[test]
fn tier3_fields_are_queryable() {
    let mut e = open();
    let r = rows(
        &mut e,
        // No `AS`: the batch grammar rejects column aliases (ADR 0002).
        "SELECT tag, count(*) FROM log GROUP BY tag ORDER BY count(*) DESC",
    );
    assert!(r.len() >= 5, "{r:?}");
    let total: i64 = r
        .iter()
        .map(|x| match &x[1] {
            Cell::Int(n) => *n,
            other => panic!("{other:?}"),
        })
        .sum();
    assert_eq!(total, 1000);
}

#[test]
fn tables_and_types() {
    let e = open();
    let t = e.tables().unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].name, TABLE);
    let ty = |n: &str| {
        t[0].columns
            .iter()
            .find(|c| c.name == n)
            .unwrap()
            .type_name
            .clone()
    };
    assert_eq!(ty("severity"), "INTEGER");
    assert_eq!(ty("facility"), "TEXT");
    assert_eq!(ty("tag"), "TEXT");
}

#[test]
fn errors_have_the_right_kind() {
    let mut e = open();
    let k = |sql: &str, e: &mut StreamEngine| e.run_query(sql).unwrap_err().kind;
    assert_eq!(k("SELECT * FROM syslog", &mut e), ErrorKind::Compile);
    assert_eq!(
        k("SELECT * FROM log WHERE severity > 'LOUD'", &mut e),
        ErrorKind::Compile
    );
    assert_eq!(
        k("SELECT * FROM log WHERE nosuch = 1", &mut e),
        ErrorKind::Compile
    );
    assert_eq!(
        k("SELECT * FROM log JOIN log AS b ON log.tag = b.tag", &mut e),
        ErrorKind::Unsupported
    );
    assert_eq!(k("SELECT FROM", &mut e), ErrorKind::Parse);
}

#[test]
fn works_through_box_dyn_engine() {
    let mut e: Box<dyn Engine> = Box::new(open());
    let r = e.run_query("SELECT count(*) FROM log").unwrap();
    assert_eq!(r.rows[0][0], Cell::Int(1000));
    assert!(!e
        .explain_plan("SELECT * FROM log WHERE severity >= 'WARN'")
        .unwrap()
        .is_empty());
    assert!(!e
        .explain_opcodes("SELECT count(*) FROM log")
        .unwrap()
        .is_empty());
}

#[test]
fn refresh_admits_appended_lines_and_tail_source_streams_them() {
    let mut p = std::env::temp_dir();
    p.push(format!("db-core-engine-stream-{}.log", std::process::id()));
    std::fs::copy(FIXTURE, &p).unwrap();
    let mut e = StreamEngine::open(&p).unwrap();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM log")[0][0],
        Cell::Int(1000)
    );

    let mut src = e
        .tail_source(
            &[ColumnRequest::bare("severity")],
            Duration::from_millis(5),
            Some(3),
        )
        .unwrap();

    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(b"<2>Sep 10 09:00:00 web01 kernel[1]: oom-killer: Kill process\n<134>Sep 10 09:00:01 web01 nginx[2]: GET / 200\n").unwrap();

    assert_eq!(e.refresh().unwrap(), 2);
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM log")[0][0],
        Cell::Int(1002)
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM log WHERE severity >= 'FATAL' AND facility = 'kern'"
        )[0][0],
        Cell::Int(
            oracle()
                .iter()
                .filter(|(f, s)| *f == "kern" && *s <= 2)
                .count() as i64
                + 1
        )
    );

    let b = src.next_batch().expect("tail source sees the append");
    assert_eq!(b.num_rows, 2);
    assert!(src.next_batch().is_none());
    std::fs::remove_file(&p).ok();
}

/// ADR 0018 §Planner/§Scope: every stream result carries its effective
/// range, and a freshly opened engine's default scope (`SINCE` absent)
/// still sees the whole fixture -- `SINCE`/`UNTIL` bound *observed* time
/// ("since I opened this"), not the fixture's own (old, synthetic) event
/// timestamps.
#[test]
fn query_result_carries_a_scope_report() {
    let mut e = open();
    let result = e.run_query("SELECT count(*) FROM log").unwrap();
    let report = result.scope_report.expect("stream queries report scope");
    assert_eq!(report.lines, 1000);
    // ADR 0018 §Scope and retention: "past the file start the range is
    // clamped and reported" -- the default 1h scope reaches earlier than
    // this fixture's single load instant, so `capped` is correctly true
    // rather than a sign anything was missed (every line is still seen,
    // as `lines` above confirms).
    assert!(report.capped);
}

/// A `SINCE` bound wide enough to cover "just opened" still returns the
/// whole fixture (#306 "Done when": `since 1h` touches only the segments
/// whose minmax overlaps -- here every segment's `observed_ts` is "now",
/// so every segment overlaps a 1-hour-wide window).
#[test]
fn since_covers_the_whole_freshly_opened_fixture() {
    let mut e = open();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM log SINCE 1 h")[0][0],
        Cell::Int(1000)
    );
}

/// #306 "Done when": `facility = 'kern'` skips segments whose dictionary
/// lacks the value -- exercised end to end (Prune + residual filter both
/// apply; a segment surviving Prune isn't automatically "all kern rows").
#[test]
fn dict_eq_prune_still_filters_exact_rows() {
    let mut e = open();
    let want = oracle().iter().filter(|(f, _)| *f == "kern").count();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM log WHERE facility = 'kern'")[0][0],
        Cell::Int(want as i64)
    );
}

/// #306 "Done when": rejection tests with exact spans -- a stream JOIN
/// has no plan (ADR 0002 reject-after-parse), and an aggregate with no
/// boundary over `Scope::All` is rejected naming the construct.
#[test]
fn join_and_unbounded_aggregate_are_rejected() {
    let mut e = open();
    let err = e
        .run_query("SELECT * FROM log JOIN log AS b ON log.tag = b.tag")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);

    // `SINCE 0 lines`/`Scope::All` has no meaning through the public
    // engine yet (no CLI/config layer wires a bare `Scope::All` request
    // here) -- covered directly against `codegen::stream::compile` in
    // `src/codegen/stream.rs`'s own tests instead.
}

const LOGFMT_FIXTURE: &str = "tests/fixtures/stream/logfmt-1k.log";
const JSONL_FIXTURE: &str = "tests/fixtures/stream/jsonl-1k.log";

/// #348: `StreamEngine` must detect and dispatch per-format, not hard-code
/// `SyslogParser` -- a logfmt file's `message` must be just the `msg=`
/// value, never the whole raw line (the syslog no-match fallback).
#[test]
fn detects_and_parses_a_logfmt_file_not_as_syslog() {
    let mut e = StreamEngine::open(Path::new(LOGFMT_FIXTURE)).expect("open logfmt fixture");
    let got = rows(&mut e, "SELECT message FROM log LIMIT 3");
    let raw_lines: Vec<String> = std::fs::read_to_string(LOGFMT_FIXTURE)
        .unwrap()
        .lines()
        .take(3)
        .map(String::from)
        .collect();
    for (row, raw) in got.iter().zip(raw_lines.iter()) {
        match &row[0] {
            Cell::Text(msg) => {
                assert_ne!(msg, raw, "message must not be the whole raw line");
                assert!(
                    raw.contains(&format!("msg=\"{msg}\"")),
                    "{msg} not found in {raw}"
                );
            }
            other => panic!("expected Text message, got {other:?}"),
        }
    }
}

/// #348: same guarantee for a JSON-Lines file.
#[test]
fn detects_and_parses_a_jsonl_file_not_as_syslog() {
    let mut e = StreamEngine::open(Path::new(JSONL_FIXTURE)).expect("open jsonl fixture");
    let got = rows(&mut e, "SELECT message FROM log LIMIT 3");
    let raw_lines: Vec<String> = std::fs::read_to_string(JSONL_FIXTURE)
        .unwrap()
        .lines()
        .take(3)
        .map(String::from)
        .collect();
    for (row, raw) in got.iter().zip(raw_lines.iter()) {
        match &row[0] {
            Cell::Text(msg) => {
                assert_ne!(msg, raw, "message must not be the whole raw JSON line");
                assert!(
                    raw.contains(&format!("\"msg\": \"{msg}\"")),
                    "{msg} not found in {raw}"
                );
            }
            other => panic!("expected Text message, got {other:?}"),
        }
    }
}

/// #348: a real RFC 3164 syslog file's behavior is unchanged.
#[test]
fn still_detects_and_parses_a_real_syslog_file() {
    let mut e = open();
    let got = rows(&mut e, "SELECT severity_text FROM log LIMIT 3");
    for row in got {
        assert_ne!(
            row[0],
            Cell::Null,
            "syslog severity_text must still resolve"
        );
    }
}

const JSON_FIXTURE: &str = "tests/fixtures/stream/syslog-json-1k.log";

fn open_json() -> StreamEngine {
    StreamEngine::open(Path::new(JSON_FIXTURE)).expect("open JSON fixture")
}

/// Independent oracle: whether each line's `message` (the text after the
/// `PROC[PID]: ` prefix) contains a `"status"` key, and what its value is
/// -- a hand rolled substring search, not `json_extract` itself, so the
/// test doesn't validate the function against its own output.
fn json_status_oracle() -> Vec<Option<&'static str>> {
    STATUS_LINES
        .lines()
        .map(|l| {
            let msg = l.split_once(": ").map_or(l, |(_, m)| m);
            // The fixture's one malformed-JSON case (unterminated object,
            // `i % 17 == 0`) also contains the `"status":"..."` substring
            // textually -- excluded here by requiring the closing brace,
            // so this oracle doesn't count a line `json_extract` correctly
            // treats as unparseable.
            if !msg.ends_with('}') {
                return None;
            }
            let key = "\"status\":\"";
            let start = msg.find(key)? + key.len();
            let end = msg[start..].find('"')? + start;
            Some(&msg[start..end])
        })
        .collect()
}

// A `'static` copy of the fixture's contents so the oracle's borrows
// outlive the function that built them (read once at first use).
static STATUS_LINES: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| std::fs::read_to_string(JSON_FIXTURE).unwrap());

/// #307 "Done when": `json_extract` over a JSON-in-syslog `message`,
/// grouped by the extracted value via its `SELECT`-list alias. The
/// issue's own acceptance query adds `WHERE message LIKE '%status%'` as
/// a cheap pre-filter before the late parse -- `LIKE` is not part of
/// this validated subset's `WHERE` grammar at all (pre-existing,
/// unrelated to `json_extract`; `parser::column::validate_expr`'s doc
/// comment already calls this out), so that clause is dropped here.
/// Adding `LIKE`/`GLOB` (or a general `FunctionCall`) to the `WHERE`
/// subset is a separate, unscoped decision.
#[test]
fn json_extract_with_alias_and_group_by_matches_the_oracle() {
    let mut e = open_json();
    let got: BTreeMap<String, i64> = rows(
        &mut e,
        "SELECT json_extract(message,'$.status') AS s, count(*) FROM log \
         GROUP BY s ORDER BY s",
    )
    .into_iter()
    // The NULL group (rows with no `"status"` key, or malformed JSON) is
    // covered by its own test below -- filtered out here so this test
    // compares only the present-and-parseable groups against the oracle.
    .filter_map(|r| match (&r[0], &r[1]) {
        (Cell::Text(s), Cell::Int(n)) => Some((s.clone(), *n)),
        (Cell::Null, Cell::Int(_)) => None,
        other => panic!("{other:?}"),
    })
    .collect();

    let mut want: BTreeMap<String, i64> = BTreeMap::new();
    for status in json_status_oracle().into_iter().flatten() {
        *want.entry(status.to_string()).or_default() += 1;
    }
    assert_eq!(got, want);
}

/// A missing key (or malformed JSON) returns `NULL`, not an error -- the
/// fixture seeds both on purpose (see the generator's `i % 10`/`i % 17`
/// cases), so at least one row must produce it.
#[test]
fn json_extract_returns_null_for_missing_key_or_malformed_json() {
    let mut e = open_json();
    let got = rows(&mut e, "SELECT json_extract(message,'$.status') FROM log");
    assert!(got.iter().any(|r| r[0] == Cell::Null));
    assert_eq!(got.len(), 1000);
}
