//! Cross-mode join lookup materialization benchmark (ADR 0023, #373):
//! whole-table materialization (`engine::cross_mode::scan_table_as_batch`,
//! the SQLite lookup side of a cross-mode join, ADR-0019) against a
//! synthetic one-seek-per-key baseline, across lookup table sizes and
//! probe-side selectivity -- the measurement ADR-0019 deferred rather
//! than assumed. **Report only** -- `make perf` (ADR 0015, tier 6).
//!
//! The one-seek-per-key baseline uses `RowEngine::run_query`'s public
//! `SELECT ... WHERE id = ?` per key, not a raw `TableCursor::seek` (that
//! API is `pub(crate)`, unreachable from a bench binary, which -- like
//! `tests/` -- only sees the public surface). This makes the baseline
//! pessimistic: it pays full parse+compile per key, which a real
//! key-restricted materialization would not. If whole-table scanning
//! already beats this inflated baseline, that's strong evidence for
//! ADR-0023's "measured, not assumed" standard; if the inflated baseline
//! still wins, a real (cheaper) seek would win by more.
//!
//! Table sizes: 1K/10K/100K rows (ADR-0023 also names 1M; omitted here to
//! keep `make perf` fast -- the 1K-100K trend already shows whether cost
//! scales with N, which is what matters for extrapolating to 1M).

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
use std::path::PathBuf;

use db_core::engine::cross_mode::scan_table_as_batch;
use db_core::engine::row::RowEngine;
use db_core::engine::Engine;

const ROW_FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";
const SIZES: [usize; 3] = [1_000, 10_000, 100_000];
const SELECTIVITIES: [f64; 3] = [0.01, 0.10, 1.00];
const INSERT_CHUNK: usize = 500;

struct TempDb(PathBuf);

impl TempDb {
    fn seeded(label: &str, n: usize) -> (Self, RowEngine) {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-bench-cross-mode-lookup-{label}-{n}-{}.db",
            std::process::id()
        ));
        std::fs::copy(ROW_FIXTURE, &path).expect("copy fixture");
        let mut engine = RowEngine::open(&path).expect("open fixture copy");
        engine
            .run_query("CREATE TABLE hosts(id INTEGER PRIMARY KEY, region TEXT);")
            .expect("create hosts table");

        let regions = ["eu", "us", "apac"];
        let mut id = 1usize;
        while id <= n {
            let end = (id + INSERT_CHUNK).min(n + 1);
            let values: Vec<String> = (id..end)
                .map(|i| format!("({i}, '{}')", regions[i % regions.len()]))
                .collect();
            engine
                .run_query(&format!("INSERT INTO hosts VALUES {};", values.join(", ")))
                .expect("seed hosts chunk");
            id = end;
        }
        (TempDb(path), engine)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn main() {
    let mut r = common::Report::new("cross_mode_lookup");

    // scan_ns[i] / seek_ns[i] correspond to SIZES[i], filled in as we go so
    // the summary table below doesn't re-run anything.
    let mut scan_ns = [0.0; SIZES.len()];
    let mut seek_ns = [0.0; SIZES.len()];

    for (i, &n) in SIZES.iter().enumerate() {
        let (_db, engine) = TempDb::seeded("scan", n);
        let cols = vec!["id".to_string(), "region".to_string()];

        scan_ns[i] = r.bench(&format!("whole_table_scan/n={n}"), || {
            black_box(scan_table_as_batch(&engine, "hosts", &cols).unwrap())
        });

        // A fresh key each call (round-robin over the table) so the b-tree
        // cache/branch predictor can't settle on one hot path -- same
        // spirit as a probe side whose keys aren't all identical.
        let mut key = 1i64;
        let mut probe_engine = engine;
        seek_ns[i] = r.bench(&format!("seek_one_key/n={n}"), || {
            let sql = format!("SELECT region FROM hosts WHERE id = {key}");
            key = (key % n as i64) + 1;
            black_box(probe_engine.run_query(&sql).unwrap())
        });
    }

    eprintln!("\nbreak-even: whole-table scan vs. K one-key seeks (median ns)");
    eprintln!(
        "{:>10}  {:>14}  {:>14}  {:>12}  {:>10}",
        "n", "scan_ns", "seek_ns/key", "break-even K", "break-even %"
    );
    for (i, &n) in SIZES.iter().enumerate() {
        let break_even_k = scan_ns[i] / seek_ns[i];
        eprintln!(
            "{:>10}  {:>14.0}  {:>14.1}  {:>12.0}  {:>9.2}%",
            n,
            scan_ns[i],
            seek_ns[i],
            break_even_k,
            100.0 * break_even_k / n as f64
        );
    }

    eprintln!("\nwhole-table scan vs. seek-per-key at each (n, selectivity):");
    eprintln!(
        "{:>10}  {:>12}  {:>14}  {:>14}  {:>10}",
        "n", "selectivity", "scan_ns", "k_seeks_ns", "winner"
    );
    for (i, &n) in SIZES.iter().enumerate() {
        for &sel in &SELECTIVITIES {
            let k = ((n as f64) * sel).max(1.0);
            let k_seeks_ns = k * seek_ns[i];
            let winner = if scan_ns[i] < k_seeks_ns {
                "scan"
            } else {
                "seek"
            };
            eprintln!(
                "{:>10}  {:>11.0}%  {:>14.0}  {:>14.0}  {:>10}",
                n,
                sel * 100.0,
                scan_ns[i],
                k_seeks_ns,
                winner
            );
        }
    }

    r.finish();
}
