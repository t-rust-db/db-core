//! Stream-engine materialization benchmark (#305, ADR 0018 §Consequences):
//! the number the "batch performance" claim rests on. Rows/s for sealing a
//! block, materializing a segment into a `vm::batch::Batch` (2 columns vs
//! all), and an end-to-end `SELECT count(*) FROM log WHERE severity >= 13`
//! over a ring of segments. **Report only** -- `make perf` (ADR 0015, tier 6).

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
use std::sync::Arc;
use std::time::Instant;

use db_core::codegen::batch as planner;
use db_core::storage::stream::{
    Block, ColumnRequest, Segment, Source, SourceKind, StreamSegment, SyslogParser,
    PREDEFINED_COLUMNS,
};
use db_core::vm::batch::Segment as _;
use db_core::vm::engine;

const LINES_PER_BLOCK: usize = 4096;
const SEGMENTS: usize = 64; // ~262k rows, about an hour at 70 lines/s

fn block() -> Block {
    let services = ["nginx", "sshd", "postgres", "cron", "systemd", "kernel"];
    let hosts = ["web01", "web02", "db01", "cache01", "gateway"];
    let pris = [134u8, 38, 131, 30, 2, 4, 27, 29];
    let mut text = String::new();
    for i in 0..LINES_PER_BLOCK {
        text.push_str(&format!(
            "<{}>Sep 10 08:{:02}:{:02} {} {}[{}]: request {} completed in {} ms\n",
            pris[i % pris.len()],
            (i / 60) % 60,
            i % 60,
            hosts[i % hosts.len()],
            services[i % services.len()],
            1000 + i,
            i,
            i % 900
        ));
    }
    Block {
        file_off: 0,
        bytes: Arc::from(text.as_bytes()),
    }
}

fn seal(b: &Block) -> Arc<Segment> {
    let mut v = Segment::seal_block(
        b,
        &Source::new(SourceKind::File, "bench.log"),
        &SyslogParser::with_year(2026),
        0,
    );
    Arc::new(v.remove(0))
}

fn rows_per_s(rows: usize, iters: u32, f: impl Fn()) -> f64 {
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    (rows as f64 * f64::from(iters)) / t.elapsed().as_secs_f64()
}

fn main() {
    let mut r = common::Report::new("stream_materialize");
    let b = block();
    let seg = seal(&b);
    let two = vec![
        ColumnRequest::bare("severity"),
        ColumnRequest::bare("facility"),
    ];
    let all: Vec<ColumnRequest> = PREDEFINED_COLUMNS
        .iter()
        .map(|c| ColumnRequest::bare(c))
        .chain(
            ["tag", "pid", "hostname"]
                .iter()
                .map(|c| ColumnRequest::bare(c)),
        )
        .collect();

    r.bench("stream/seal_block_4096", || black_box(seal(&b)));
    let s2 = StreamSegment::new(Arc::clone(&seg), two.clone());
    r.bench("stream/load_2_cols", || black_box(s2.load().unwrap()));
    let sa = StreamSegment::new(Arc::clone(&seg), all.clone());
    r.bench("stream/load_all_cols", || black_box(sa.load().unwrap()));

    // End to end over a ring of SEGMENTS segments.
    let select = planner::expand_star(
        &db_core::parser::parse("SELECT count(*) FROM log WHERE severity >= 13").unwrap(),
        &[],
    )
    .unwrap();
    let program = planner::compile(&select).unwrap();
    let cols: Vec<ColumnRequest> = program
        .columns_to_load()
        .iter()
        .map(|c| ColumnRequest::bare(c))
        .collect();
    let ring: Vec<StreamSegment> = (0..SEGMENTS)
        .map(|_| StreamSegment::new(Arc::clone(&seg), cols.clone()))
        .collect();
    r.bench("stream/count_where_severity_64x4096", || {
        black_box(engine::run(&ring, &program).unwrap())
    });

    // Rows/s summary, the unit the ADR talks in.
    let n = LINES_PER_BLOCK;
    eprintln!("\nrows/s (median-free, {} iters):", 20);
    eprintln!(
        "  seal_block        {:>12.0}",
        rows_per_s(n, 20, || {
            black_box(seal(&b));
        })
    );
    eprintln!(
        "  load 2 cols       {:>12.0}",
        rows_per_s(n, 20, || {
            black_box(s2.load().unwrap());
        })
    );
    eprintln!(
        "  load all cols     {:>12.0}",
        rows_per_s(n, 20, || {
            black_box(sa.load().unwrap());
        })
    );
    eprintln!(
        "  count where sev   {:>12.0}  ({} rows/query)",
        rows_per_s(n * SEGMENTS, 5, || {
            black_box(engine::run(&ring, &program).unwrap());
        }),
        n * SEGMENTS
    );
    r.finish();
}
