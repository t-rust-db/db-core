// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The column engine's sqllogictest suite (#680): every `.test` file under
//! `tests/sqllogictest/column/` runs through `BatchEngine::run_query` and
//! is compared with the expected results committed in the file. Those
//! results are DuckDB's answers on the same Parquet fixtures (ADR 0000
//! (f), ADR 0030), written by `make refresh-column-sqllogictest` -- never
//! typed by hand -- so CI checks the column engine against DuckDB without
//! running DuckDB.
//!
//! The format is sqllogictest's, as in sqlite-rs's `tests/sqllogictest/`:
//!
//! - `query <types> <rowsort|valuesort|nosort>`, the SQL, `----`, then one
//!   value per line, row-major. Types are `I` (integer, boolean as 0/1),
//!   `R` (float) and `T` (anything else, compared as text); the refresh
//!   derives them from DuckDB's result types.
//! - `statement ok` / `statement error`: the SQL must succeed / fail in
//!   the column engine. `statement error` records a shape the engine
//!   refuses on purpose (the refresh leaves it alone).
//! - `skipif db-core` above a record: DuckDB's answer is kept, but the
//!   column engine does not give it yet; counted as a skip.
//!
//! Values render as NULL `NULL`, the empty string `(empty)`, anything else
//! as `Cell`'s `Display`. `R` values match within a relative 1e-12 (the
//! fuzz-sql comparator's `FLOAT_REL_EPS`). There is no DDL: a table is a
//! fixture in `tests/fixtures/parquet/`, named by its file stem, and the
//! runner opens the one each query's `FROM` names.
//!
//! The per-file tally is written to `tools/column-sqllogictest-status.json`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use db_core::engine::column::BatchEngine;
use db_core::engine::{Cell, Engine};

/// The fuzz-sql comparator's tolerance (`fuzz/run/src/compare.rs`).
const FLOAT_REL_EPS: f64 = 1e-12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::enum_variant_names,
    reason = "matches sqllogictest's own sort-mode vocabulary"
)]
enum SortMode {
    NoSort,
    RowSort,
    ValueSort,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Statement {
        expect_ok: bool,
    },
    Query {
        types: String,
        sort: SortMode,
        expected: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Record {
    line: usize,
    skip: bool,
    sql: String,
    kind: Kind,
}

fn parse_sort_mode(s: &str) -> Option<SortMode> {
    match s {
        "nosort" => Some(SortMode::NoSort),
        "rowsort" => Some(SortMode::RowSort),
        "valuesort" => Some(SortMode::ValueSort),
        _ => None,
    }
}

/// Splits `text` into blank-line-separated blocks with the 1-based line
/// of each block's first line; comment lines (`# ...`) are dropped.
fn blocks(text: &str) -> Vec<(usize, Vec<&str>)> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut start = 0;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push((start, std::mem::take(&mut current)));
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if current.is_empty() {
            start = i + 1;
        }
        current.push(line);
    }
    if !current.is_empty() {
        out.push((start, current));
    }
    out
}

/// Parses one `.test` file. A malformed record is an error, not a silent
/// drop: a typo must not turn a check into nothing.
fn parse_script(text: &str) -> Result<Vec<Record>, String> {
    let mut records = Vec::new();
    for (start, block) in blocks(text) {
        let mut skip = false;
        let mut at = 0;
        while let Some(line) = block.get(at) {
            match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["skipif", "db-core", ..] => skip = true,
                ["skipif" | "onlyif", ..] => {}
                _ => break,
            }
            at += 1;
        }
        let line = start + at;
        let Some((header, rest)) = block[at..].split_first() else {
            return Err(format!("line {line}: a directive without a record"));
        };
        let words: Vec<&str> = header.split_whitespace().collect();
        let kind = match words.as_slice() {
            ["statement", "ok"] => Kind::Statement { expect_ok: true },
            ["statement", "error"] => Kind::Statement { expect_ok: false },
            ["query", types, sort] => {
                let sort = parse_sort_mode(sort)
                    .ok_or_else(|| format!("line {line}: unknown sort mode {sort}"))?;
                let sep = rest
                    .iter()
                    .position(|l| *l == "----")
                    .ok_or_else(|| format!("line {line}: query without ----"))?;
                let expected = rest[sep + 1..].iter().map(|s| (*s).to_string()).collect();
                records.push(Record {
                    line,
                    skip,
                    sql: rest[..sep].join("\n"),
                    kind: Kind::Query {
                        types: (*types).to_string(),
                        sort,
                        expected,
                    },
                });
                continue;
            }
            _ => return Err(format!("line {line}: unknown record: {header}")),
        };
        records.push(Record {
            line,
            skip,
            sql: rest.join("\n"),
            kind,
        });
    }
    Ok(records)
}

/// The fixture a query reads: the word after its first `FROM`.
fn table_of(sql: &str) -> Option<String> {
    let words: Vec<&str> = sql.split_whitespace().collect();
    let at = words.iter().position(|w| w.eq_ignore_ascii_case("from"))?;
    let name = words.get(at + 1)?;
    let name: String = name
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    Some(name)
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parquet")
}

/// Runs `sql` against the fixture its `FROM` names, catching a panic so
/// one bad query does not hide the rest of the suite.
fn run(sql: &str) -> Result<(Vec<String>, Vec<Vec<Cell>>), String> {
    let table = table_of(sql).ok_or("no FROM <table> in the query")?;
    let path = fixture_dir().join(format!("{table}.parquet"));
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut engine = BatchEngine::open(&path).map_err(|e| e.to_string())?;
        engine
            .run_query(sql)
            .map(|r| (r.columns, r.rows))
            .map_err(|e| e.to_string())
    }));
    outcome.unwrap_or_else(|_| Err("panicked".to_string()))
}

fn render(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) if s.is_empty() => "(empty)".to_string(),
        other => other.to_string(),
    }
}

/// Puts `values` (row-major, `width` per row) in the order `sort` asks
/// for. Expected and actual both go through this, so their order agrees
/// even when the file was hand-edited.
fn sorted(values: Vec<String>, width: usize, sort: SortMode) -> Vec<String> {
    match sort {
        SortMode::NoSort => values,
        SortMode::ValueSort => {
            let mut v = values;
            v.sort();
            v
        }
        SortMode::RowSort => {
            let mut rows: Vec<Vec<String>> = values
                .chunks(width.max(1))
                .map(<[String]>::to_vec)
                .collect();
            rows.sort();
            rows.concat()
        }
    }
}

fn floats_close(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x == y || (x - y).abs() <= FLOAT_REL_EPS * x.abs().max(y.abs()),
        _ => false,
    }
}

/// Compares value lists of a `types`-wide result: `R` columns within the
/// float tolerance, the rest exactly. `Err` names the first difference.
fn compare(types: &str, want: &[String], got: &[String]) -> Result<(), String> {
    if want.len() != got.len() {
        return Err(format!("{} values, expected {}", got.len(), want.len()));
    }
    let kinds: Vec<char> = types.chars().collect();
    for (i, (w, g)) in want.iter().zip(got).enumerate() {
        let float = kinds.get(i % kinds.len().max(1)) == Some(&'R');
        if w != g && !(float && floats_close(w, g)) {
            return Err(format!("value {i}: got {g}, expected {w}"));
        }
    }
    Ok(())
}

/// `Ok(())` when the column engine gives the record's expected outcome.
fn check(record: &Record) -> Result<(), String> {
    let result = run(&record.sql);
    match &record.kind {
        Kind::Statement { expect_ok: true } => result.map(|_| ()),
        Kind::Statement { expect_ok: false } => match result {
            Ok(_) => Err("succeeded, expected an error".to_string()),
            Err(e) if e == "panicked" => Err(e),
            Err(_) => Ok(()),
        },
        Kind::Query {
            types,
            sort,
            expected,
        } => {
            let (columns, rows) = result?;
            if columns.len() != types.len() {
                return Err(format!(
                    "{} columns, expected {}",
                    columns.len(),
                    types.len()
                ));
            }
            let got = rows.iter().flatten().map(render).collect();
            compare(
                types,
                &sorted(expected.clone(), types.len(), *sort),
                &sorted(got, types.len(), *sort),
            )
        }
    }
}

#[derive(Debug, Default)]
struct Tally {
    file: String,
    pass: usize,
    skip: usize,
    fail: usize,
}

fn run_file(path: &Path, failures: &mut Vec<String>) -> Tally {
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    let mut tally = Tally {
        file: file.clone(),
        ..Tally::default()
    };
    let text = std::fs::read_to_string(path).unwrap();
    let records = match parse_script(&text) {
        Ok(records) => records,
        Err(e) => {
            failures.push(format!("{file}: {e}"));
            tally.fail += 1;
            return tally;
        }
    };
    for record in &records {
        if record.skip {
            tally.skip += 1;
            continue;
        }
        match check(record) {
            Ok(()) => tally.pass += 1,
            Err(e) => {
                tally.fail += 1;
                failures.push(format!("{file}:{}: {e}\n    {}", record.line, record.sql));
            }
        }
    }
    tally
}

/// Hand-rolled (no serde in the dependency closure), one file per line so
/// a diff shows which file moved.
fn status_json(tallies: &[Tally]) -> String {
    let mut out = String::from("{\n  \"files\": [\n");
    for (i, t) in tallies.iter().enumerate() {
        let comma = if i + 1 < tallies.len() { "," } else { "" };
        out.push_str(&format!(
            "    {{\"file\": \"{}\", \"pass\": {}, \"skip\": {}, \"fail\": {}}}{comma}\n",
            t.file, t.pass, t.skip, t.fail
        ));
    }
    let sum = |f: fn(&Tally) -> usize| tallies.iter().map(f).sum::<usize>();
    out.push_str(&format!(
        "  ],\n  \"total\": {{\"pass\": {}, \"skip\": {}, \"fail\": {}}}\n}}\n",
        sum(|t| t.pass),
        sum(|t| t.skip),
        sum(|t| t.fail)
    ));
    out
}

#[test]
fn column_engine_agrees_with_duckdb_on_every_test_file() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sqllogictest/column");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "test"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no .test files in {}", dir.display());

    let mut failures = Vec::new();
    let tallies: Vec<Tally> = paths.iter().map(|p| run_file(p, &mut failures)).collect();
    let status =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/column-sqllogictest-status.json");
    std::fs::write(&status, status_json(&tallies)).unwrap();

    assert!(
        failures.is_empty(),
        "{} column sqllogictest failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn parses_queries_statements_and_skips() {
    let text = "# c\nquery IT rowsort\nSELECT a, b\nFROM t\n----\n1\nfoo\n\n\
                statement error\nSELECT x FROM t\n\nskipif db-core\nquery R nosort\nSELECT f FROM t\n----\n";
    let records = parse_script(text).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].line, 2);
    assert_eq!(records[0].sql, "SELECT a, b\nFROM t");
    assert_eq!(
        records[0].kind,
        Kind::Query {
            types: "IT".to_string(),
            sort: SortMode::RowSort,
            expected: vec!["1".to_string(), "foo".to_string()],
        }
    );
    assert_eq!(records[1].kind, Kind::Statement { expect_ok: false });
    assert!(!records[1].skip);
    assert!(records[2].skip);
    assert_eq!(records[2].line, 13);
}

#[test]
fn malformed_records_are_errors_not_dropped() {
    assert!(parse_script("query I bogus\nSELECT 1\n----\n1\n").is_err());
    assert!(parse_script("query I nosort\nSELECT 1\n").is_err());
    assert!(parse_script("statment ok\nSELECT 1\n").is_err());
    assert!(parse_script("skipif db-core\n").is_err());
}

#[test]
fn table_is_the_word_after_from() {
    assert_eq!(table_of("SELECT a FROM t WHERE x").as_deref(), Some("t"));
    assert_eq!(
        table_of("select count(*)\nfrom typed_nulls;").as_deref(),
        Some("typed_nulls")
    );
    assert_eq!(table_of("SELECT 1"), None);
}

#[test]
fn sort_modes_order_rows_and_values() {
    let v = |s: &[&str]| s.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
    assert_eq!(
        sorted(v(&["b", "1", "a", "2"]), 2, SortMode::RowSort),
        v(&["a", "2", "b", "1"])
    );
    assert_eq!(
        sorted(v(&["b", "1", "a", "2"]), 2, SortMode::ValueSort),
        v(&["1", "2", "a", "b"])
    );
    assert_eq!(sorted(v(&["b", "a"]), 1, SortMode::NoSort), v(&["b", "a"]));
}

#[test]
fn floats_match_within_tolerance_and_other_types_exactly() {
    let v = |s: &[&str]| s.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
    assert!(compare("R", &v(&["0.30000000000000004"]), &v(&["0.3"])).is_ok());
    assert!(compare("R", &v(&["0.31"]), &v(&["0.3"])).is_err());
    assert!(compare("RT", &v(&["NULL", "x"]), &v(&["NULL", "x"])).is_ok());
    assert!(compare("T", &v(&["1.50"]), &v(&["1.5"])).is_err());
    assert!(compare("I", &v(&["1"]), &v(&["1", "2"])).is_err());
}
