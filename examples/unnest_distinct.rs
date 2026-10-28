// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! #815's measurement: `UNNEST(string_split(..))` and `count(DISTINCT
//! <Utf8>)` over an ingest-shaped table (events of 1-4 rows, every row of
//! an event repeating a comma-separated `labels`), through the column
//! engine. Writes the fixture, prints the median time of each query, and
//! with `--duckdb` also times DuckDB (single-threaded, same file).
//!
//!     cargo run --release --example unnest_distinct -- [events] [--duckdb] [--only name]

#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    reason = "measurement tool (like tests/): fixed, in-range generated data"
)]

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use db_core::engine::column::BatchEngine;
use db_core::engine::Engine;
use db_core::storage::parquet::writer::{
    ColumnSpec, ColumnType, ColumnValues, ParquetWriter, WriterOptions,
};

const LABELS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

const QUERIES: [(&str, &str); 4] = [
    (
        "baseline",
        "SELECT labels, count(*), count(DISTINCT source) FROM t WHERE seq = 0 GROUP BY labels",
    ),
    (
        "unnest",
        "SELECT label, count(*), count(DISTINCT source) FROM t, UNNEST(string_split(labels, ',')) AS l(label) WHERE seq = 0 AND label <> '' GROUP BY label",
    ),
    (
        "unnest_distinct_id",
        "SELECT label, count(*), count(DISTINCT event_id) FROM t, UNNEST(string_split(labels, ',')) AS l(label) WHERE label <> '' GROUP BY label",
    ),
    (
        "distinct_id",
        "SELECT labels, count(DISTINCT event_id) FROM t GROUP BY labels",
    ),
];

/// A deterministic pseudo-random sequence (xorshift), so runs compare.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// The fixture: `events` events with 1-4 rows each (`seq` 0, 1, ...), the
/// same `labels` (1-2 of 4 values) and `source` on every row of an event.
fn write(path: &Path, events: u64) -> Result<usize, String> {
    let mut rng = Rng(0x5EED);
    let (mut ids, mut seqs, mut sources, mut labels) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for id in 0..events {
        let l = if rng.below(2) == 0 {
            LABELS[rng.below(4) as usize].to_string()
        } else {
            format!(
                "{},{}",
                LABELS[rng.below(4) as usize],
                LABELS[rng.below(4) as usize]
            )
        };
        let source = format!("source-{}", rng.below(50));
        for seq in 0..=rng.below(4).min(3) {
            ids.push(Some(id as i64));
            seqs.push(Some(seq as i32));
            sources.push(Some(source.clone()));
            labels.push(Some(l.clone()));
        }
    }
    let rows = ids.len();
    let file = File::create(path).map_err(|e| e.to_string())?;
    let schema = vec![
        ColumnSpec::new("event_id", ColumnType::Int64, false),
        ColumnSpec::new("seq", ColumnType::Int32, false),
        ColumnSpec::new("source", ColumnType::Utf8, false),
        ColumnSpec::new("labels", ColumnType::Utf8, false),
    ];
    let mut w = ParquetWriter::new(BufWriter::new(file), schema, WriterOptions::default())
        .map_err(|e| e.to_string())?;
    w.write_columns(vec![
        ColumnValues::Int64(ids),
        ColumnValues::Int32(seqs),
        ColumnValues::Utf8(sources),
        ColumnValues::Utf8(labels),
    ])
    .map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    Ok(rows)
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let duckdb = args.iter().any(|a| a == "--duckdb");
    let events: u64 = args
        .iter()
        .find(|a| a.parse::<u64>().is_ok())
        .map(|n| n.parse().map_err(|_| format!("not a count: {n}")))
        .transpose()?
        .unwrap_or(30_000);
    let dir = std::env::temp_dir().join(format!("db-core-unnest-distinct-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("t.parquet");
    let rows = write(&path, events)?;
    println!("{events} events, {rows} rows");

    let mut engine = BatchEngine::open(&path).map_err(|e| e.to_string())?;
    let only = args
        .iter()
        .position(|a| a == "--only")
        .and_then(|i| args.get(i + 1));
    for (name, sql) in QUERIES {
        if only.is_some_and(|o| o != name) {
            continue;
        }
        engine.run_query(sql).map_err(|e| format!("{name}: {e}"))?;
        let mut times = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            engine.run_query(sql).map_err(|e| format!("{name}: {e}"))?;
            times.push(start.elapsed());
        }
        print!("{name:<20} db-core {:>8.2?}", median(times));
        if duckdb {
            print!("  duckdb {:>8.2?}", duckdb_time(&path, sql)?);
        }
        println!();
    }
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// DuckDB's median over five runs in one process (the first is warm-up).
fn duckdb_time(path: &Path, sql: &str) -> Result<Duration, String> {
    let script = format!(
        "PRAGMA threads=1;\nCREATE VIEW t AS SELECT * FROM read_parquet('{}');\n.timer on\n{}",
        path.display(),
        format!("{sql};\n").repeat(6)
    );
    let mut child = Command::new("duckdb")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("duckdb: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("duckdb stdin")?
        .write_all(script.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut times: Vec<Duration> = text
        .lines()
        .filter_map(|l| l.strip_prefix("Run Time (s): real "))
        .filter_map(|l| l.split_whitespace().next()?.parse::<f64>().ok())
        .map(Duration::from_secs_f64)
        .collect();
    if times.len() < 6 {
        return Err(format!("duckdb timings not found:\n{text}"));
    }
    times.remove(0);
    Ok(median(times))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
