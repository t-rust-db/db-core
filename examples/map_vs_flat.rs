// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! #621's measurement: request headers stored as a `MAP<UTF8, UTF8>`
//! column (one row per request) against a flat key/value table (one row
//! per header), both written by the Parquet writer with its defaults
//! (Snappy, dictionary encoding), then the same lookup through the column
//! engine. Prints bytes per request and the median time of each lookup.
//!
//!     cargo run --release --example map_vs_flat -- [requests]

#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "measurement tool (like tests/): fixed, in-range generated data"
)]

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use db_core::engine::column::BatchEngine;
use db_core::engine::Engine;
use db_core::storage::parquet::writer::{
    ColumnSpec, ColumnType, ColumnValues, MapEntries, ParquetWriter, WriterOptions,
};

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

/// One request's headers: always Host, User-Agent, Accept,
/// Accept-Encoding, Accept-Language, X-Request-Id; often Cookie, Referer,
/// X-Forwarded-For; sometimes Authorization, Content-Type. About ten.
fn headers(rng: &mut Rng, i: u64) -> MapEntries {
    let mut h: MapEntries = vec![
        (
            "Host".into(),
            Some(format!("api{}.example.com", rng.below(10))),
        ),
        (
            "User-Agent".into(),
            Some(format!(
                "Mozilla/5.0 (agent {}) Gecko/20100101",
                rng.below(50)
            )),
        ),
        (
            "Accept".into(),
            Some(["*/*", "application/json", "text/html"][rng.below(3) as usize].into()),
        ),
        ("Accept-Encoding".into(), Some("gzip, deflate, br".into())),
        (
            "Accept-Language".into(),
            Some(format!("en-US,en;q=0.{}", rng.below(9) + 1)),
        ),
        (
            "X-Request-Id".into(),
            Some(format!("{:016x}", i.wrapping_mul(0x9E37_79B9_7F4A_7C15))),
        ),
    ];
    if rng.below(10) < 7 {
        h.push((
            "Cookie".into(),
            Some(format!("session={:x}; theme=dark", rng.below(1 << 40))),
        ));
    }
    if rng.below(10) < 6 {
        h.push((
            "Referer".into(),
            Some(format!("https://example.com/page/{}", rng.below(100))),
        ));
    }
    if rng.below(10) < 5 {
        h.push((
            "X-Forwarded-For".into(),
            Some(format!(
                "10.{}.{}.{}",
                rng.below(4),
                rng.below(256),
                rng.below(256)
            )),
        ));
    }
    if rng.below(10) < 3 {
        h.push((
            "Authorization".into(),
            Some(format!("Bearer {:x}", rng.below(1 << 48))),
        ));
    }
    if rng.below(10) < 4 {
        h.push(("Content-Type".into(), Some("application/json".into())));
    }
    h
}

fn write(path: &Path, schema: Vec<ColumnSpec>, columns: Vec<ColumnValues>) -> Result<u64, String> {
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut w = ParquetWriter::new(BufWriter::new(file), schema, WriterOptions::default())
        .map_err(|e| e.to_string())?;
    w.write_columns(columns).map_err(|e| e.to_string())?;
    Ok(w.finish().map_err(|e| e.to_string())?.1.bytes)
}

/// The median wall time of `runs` runs of `sql`, and its answer.
fn time(engine: &mut BatchEngine, sql: &str, runs: usize) -> Result<(Duration, String), String> {
    let mut times = Vec::new();
    let mut answer = String::new();
    for _ in 0..runs {
        let start = Instant::now();
        let result = engine.run_query(sql).map_err(|e| format!("{sql}: {e}"))?;
        times.push(start.elapsed());
        answer = result
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join(";");
    }
    times.sort();
    Ok((times[times.len() / 2], answer))
}

fn run() -> Result<(), String> {
    let requests: u64 = std::env::args()
        .nth(1)
        .map(|n| n.parse().map_err(|_| format!("not a count: {n}")))
        .transpose()?
        .unwrap_or(200_000);
    let dir = std::env::temp_dir().join(format!("db-core-map-vs-flat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut rng = Rng(0x5EED);
    let rows: Vec<MapEntries> = (0..requests).map(|i| headers(&mut rng, i)).collect();
    let entries: usize = rows.iter().map(Vec::len).sum();

    let map_path = dir.join("map_t.parquet");
    let map_bytes = write(
        &map_path,
        vec![
            ColumnSpec::new("request_id", ColumnType::UInt64, false),
            ColumnSpec::new("headers", ColumnType::MapUtf8, false),
        ],
        vec![
            ColumnValues::UInt64((0..requests).map(Some).collect()),
            ColumnValues::Map(rows.iter().cloned().map(Some).collect()),
        ],
    )?;

    let flat_path = dir.join("flat_t.parquet");
    let (mut ids, mut names, mut values) = (Vec::new(), Vec::new(), Vec::new());
    for (i, row) in rows.iter().enumerate() {
        for (k, v) in row {
            ids.push(Some(i as u64));
            names.push(Some(k.clone()));
            values.push(v.clone());
        }
    }
    let flat_bytes = write(
        &flat_path,
        vec![
            ColumnSpec::new("request_id", ColumnType::UInt64, false),
            ColumnSpec::new("name", ColumnType::Utf8, false),
            ColumnSpec::new("value", ColumnType::Utf8, true),
        ],
        vec![
            ColumnValues::UInt64(ids),
            ColumnValues::Utf8(names),
            ColumnValues::Utf8(values),
        ],
    )?;

    let per = |bytes: u64| bytes as f64 / requests as f64;
    println!(
        "{requests} requests, {entries} headers ({:.1} per request)",
        entries as f64 / requests as f64
    );
    println!();
    println!("| storage | bytes | bytes/request |");
    println!("|---|---|---|");
    println!(
        "| MAP column (one row per request) | {map_bytes} | {:.1} |",
        per(map_bytes)
    );
    println!(
        "| flat key/value (one row per header) | {flat_bytes} | {:.1} |",
        per(flat_bytes)
    );

    let agent = "Mozilla/5.0 (agent 7) Gecko/20100101";
    let mut map_engine = BatchEngine::open(&map_path).map_err(|e| e.to_string())?;
    let mut flat_engine = BatchEngine::open(&flat_path).map_err(|e| e.to_string())?;
    let queries = [
        (
            true,
            "MAP: headers['User-Agent'] = ...",
            format!("SELECT count(*) FROM map_t WHERE headers['User-Agent'] = '{agent}'"),
        ),
        (
            true,
            "MAP: map_get_ci(headers, 'user-agent') = ...",
            format!(
                "SELECT count(*) FROM map_t WHERE map_get_ci(headers, 'user-agent') = '{agent}'"
            ),
        ),
        (
            false,
            "flat: name = 'User-Agent' AND value = ...",
            format!("SELECT count(*) FROM flat_t WHERE name = 'User-Agent' AND value = '{agent}'"),
        ),
    ];
    println!();
    println!("| lookup (column engine, median of 5) | time | matches |");
    println!("|---|---|---|");
    let mut answers = Vec::new();
    for (is_map, label, sql) in queries {
        let engine = if is_map {
            &mut map_engine
        } else {
            &mut flat_engine
        };
        let (t, answer) = time(engine, &sql, 5)?;
        println!("| {label} | {:.1} ms | {answer} |", t.as_secs_f64() * 1e3);
        answers.push(answer);
    }
    std::fs::remove_dir_all(&dir).ok();
    if answers.windows(2).any(|w| w[0] != w[1]) {
        return Err(format!("the lookups disagree: {answers:?}"));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("map_vs_flat: {e}");
            ExitCode::FAILURE
        }
    }
}
