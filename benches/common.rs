//! The `std`-only micro-benchmark harness behind `make perf` (ADR 0015,
//! tier 6). Report only: it prints one table per bench binary and writes
//! the same numbers as JSON under `target/perf/<bench>.json`, and it
//! never asserts a number.
//!
//! Method: a short warm-up, then timed samples until both a minimum
//! sample count and a minimum wall budget are met. Each sample times a
//! batch of `k` calls (sized so one batch takes about a millisecond,
//! which keeps `Instant` overhead out of nanosecond-scale results) and
//! records the per-call time. Reported: min, median and p95 in
//! nanoseconds per call, plus the total call count.

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const WARMUP: Duration = Duration::from_millis(50);
const BUDGET: Duration = Duration::from_millis(300);
const MIN_SAMPLES: usize = 30;
const TARGET_BATCH: Duration = Duration::from_millis(1);

pub struct Row {
    pub name: String,
    pub calls: u64,
    pub min_ns: f64,
    pub median_ns: f64,
    pub p95_ns: f64,
}

pub struct Report {
    bench: &'static str,
    rows: Vec<Row>,
}

impl Report {
    pub fn new(bench: &'static str) -> Self {
        Self {
            bench,
            rows: Vec::new(),
        }
    }

    /// Times `f` and records one row named `name`.
    pub fn bench<R>(&mut self, name: &str, mut f: impl FnMut() -> R) {
        // Warm-up: run until the warm-up budget is spent, estimating the
        // per-call cost as we go.
        let start = Instant::now();
        let mut warm_calls: u64 = 0;
        while start.elapsed() < WARMUP {
            black_box(f());
            warm_calls += 1;
        }
        let per_call = start.elapsed().as_secs_f64() / warm_calls.max(1) as f64;
        let k = ((TARGET_BATCH.as_secs_f64() / per_call.max(1e-9)) as u64).clamp(1, 1_000_000);

        let mut samples: Vec<f64> = Vec::new();
        let mut calls: u64 = 0;
        let run = Instant::now();
        while samples.len() < MIN_SAMPLES || run.elapsed() < BUDGET {
            let t = Instant::now();
            for _ in 0..k {
                black_box(f());
            }
            let ns = t.elapsed().as_secs_f64() * 1e9 / k as f64;
            samples.push(ns);
            calls += k;
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pick = |q: f64| {
            let idx = ((samples.len() - 1) as f64 * q).round() as usize;
            samples[idx.min(samples.len() - 1)]
        };
        self.rows.push(Row {
            name: name.to_string(),
            calls,
            min_ns: samples[0],
            median_ns: pick(0.5),
            p95_ns: pick(0.95),
        });
    }

    /// Prints the table and writes `target/perf/<bench>.json`.
    pub fn finish(self) {
        let width = self
            .rows
            .iter()
            .map(|r| r.name.len())
            .max()
            .unwrap_or(4)
            .max(4);
        println!("\n{} -- ns/call (min / median / p95), calls", self.bench);
        println!("{}", "-".repeat(width + 44));
        for r in &self.rows {
            println!(
                "{:<width$}  {:>10.1}  {:>10.1}  {:>10.1}  {:>8}",
                r.name, r.min_ns, r.median_ns, r.p95_ns, r.calls
            );
        }
        let mut json = format!(
            "{{\n  \"bench\": \"{}\",\n  \"results\": [\n",
            escape(self.bench)
        );
        for (i, r) in self.rows.iter().enumerate() {
            json.push_str(&format!(
                "    {{\"name\": \"{}\", \"calls\": {}, \"min_ns\": {:.1}, \"median_ns\": {:.1}, \"p95_ns\": {:.1}}}{}\n",
                escape(&r.name),
                r.calls,
                r.min_ns,
                r.median_ns,
                r.p95_ns,
                if i + 1 < self.rows.len() { "," } else { "" }
            ));
        }
        json.push_str("  ]\n}\n");
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/perf");
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("perf: cannot create {}: {e}", dir.display());
            return;
        }
        let path = dir.join(format!("{}.json", self.bench));
        match fs::write(&path, json) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(e) => eprintln!("perf: cannot write {}: {e}", path.display()),
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
