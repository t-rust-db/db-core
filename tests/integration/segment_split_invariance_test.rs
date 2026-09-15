// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! ADR-0025 obligation 1 ("segment-split invariance") for `vm::batch` and
//! `vm::stream` (#404): for a given query and dataset, the result must not
//! depend on how rows were divided into segments.
//!
//! `Opcode::Combine`'s Combine/Finalize contract (`src/vm/batch.rs:420-438`)
//! says a segment boundary is never observable in the output. This harness
//! runs the same query over the same rows under several segmentations --
//! including the degenerate ones (a single segment, empty segments
//! interleaved, maximally uneven splits, one row per segment) -- and
//! demands bit-identical output.
//!
//! `COUNT(DISTINCT ...)` is *not* covered here: the column parser rejects
//! `DISTINCT` inside any aggregate call at parse time
//! (`src/parser/column.rs:507-511,790-793`, "DISTINCT inside an
//! aggregate"), so there is no such query to run a segmentation harness
//! over in `vm::batch` today.
//!
//! While building this harness, it caught a real, pre-existing bug: `AVG`
//! never merged across segments at all (`merge_rows`'s `AggPart::Avg`
//! arm was a no-op) -- with more than one segment, `AVG` silently
//! returned the first segment's local average. A second, related bug in
//! `finalize_row` corrupted or dropped whichever aggregate followed an
//! `AVG` in the same query, in any segment count, because it indexed the
//! emitted row by its position in `agg_parts` instead of tracking the
//! row's own (wider, once an `AVG` is present) cursor. Both are fixed in
//! `src/vm/engine.rs` as part of this change; see the `AggPart::Avg` arms
//! of `merge_rows` and `finalize_row` for the mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::io::Write;
use std::path::{Path, PathBuf};

use db_core::codegen::batch::compile;
use db_core::engine::column::BatchEngine;
use db_core::engine::stream::StreamEngine;
use db_core::engine::{Cell, Engine};
use db_core::parser::column::parse;
use db_core::vm::batch::Value;
use db_core::vm::engine::{run, InMemorySegment};

// ---------------------------------------------------------------------
// vm::batch: synthetic in-memory segments
// ---------------------------------------------------------------------

/// Segmentations every batch case is run under; each must produce
/// bit-identical output for `n` total rows. `sizes()` panics (loudly, at
/// test time) if the sizes don't sum to `n` -- a silent mismatch would
/// invalidate the whole comparison.
fn segmentations(n: usize) -> Vec<Vec<usize>> {
    let mut out = vec![
        vec![n],                               // single segment
        (0..n).map(|_| 1).collect::<Vec<_>>(), // one row per segment
    ];

    // Empty segments interleaved: 0, 1, 0, 1, 0, 1, ... (each real row
    // gets its own segment, with an empty segment on either side).
    let mut interleaved = vec![0usize];
    for _ in 0..n {
        interleaved.push(1);
        interleaved.push(0);
    }
    out.push(interleaved);

    // Maximally uneven: 1, 1, 1, remainder (only meaningful for n > 3).
    if n > 3 {
        out.push(vec![1, 1, 1, n - 3]);
    }

    for sizes in &out {
        assert_eq!(
            sizes.iter().sum::<usize>(),
            n,
            "segmentation {sizes:?} does not cover all {n} rows"
        );
    }
    out
}

/// Builds one `InMemorySegment` per size in `sizes`, slicing `col_a`/`col_b`
/// in order (a size-0 entry becomes an empty segment).
fn segments_for(
    sizes: &[usize],
    col_a: (&str, &[Value]),
    col_b: (&str, &[Value]),
) -> Vec<InMemorySegment> {
    let (name_a, values_a) = col_a;
    let (name_b, values_b) = col_b;
    let mut offset = 0;
    sizes
        .iter()
        .map(|&size| {
            let batch = db_core::vm::batch::Batch::new(size)
                .with_column(name_a, values_a[offset..offset + size].to_vec())
                .with_column(name_b, values_b[offset..offset + size].to_vec());
            offset += size;
            InMemorySegment(batch)
        })
        .collect()
}

/// Runs `sql` over `sizes`-segmented `(col_a, col_b)` data and returns the
/// rows, unchanged -- callers that need order-independence add `ORDER BY`
/// to `sql` themselves, since that is exactly what a real caller would do.
fn run_batch(
    sql: &str,
    sizes: &[usize],
    col_a: (&str, &[Value]),
    col_b: (&str, &[Value]),
) -> Vec<Vec<Value>> {
    let program = compile(&parse(sql).unwrap_or_else(|e| panic!("{sql}: parse error {e}")))
        .unwrap_or_else(|e| panic!("{sql}: compile error {e}"));
    let segments = segments_for(sizes, col_a, col_b);
    run(&segments, &program).unwrap_or_else(|e| panic!("{sql} over {sizes:?}: {e}"))
}

/// Asserts every segmentation of `n` rows produces the same rows for `sql`.
fn assert_split_invariant(sql: &str, n: usize, col_a: (&str, &[Value]), col_b: (&str, &[Value])) {
    let sizings = segmentations(n);
    let baseline = run_batch(sql, &sizings[0], col_a, col_b);
    for sizes in &sizings[1..] {
        let got = run_batch(sql, sizes, col_a, col_b);
        assert_eq!(
            got, baseline,
            "{sql}: segmentation {sizes:?} disagrees with {:?}",
            sizings[0]
        );
    }
}

/// 12 rows over 3 groups (`a`, `b`, `c`), 4 rows each, with a group's rows
/// spread across the dataset (not contiguous) so an uneven split still
/// crosses group boundaries -- a merge that only worked when each
/// segment held a whole group wouldn't be caught otherwise.
fn synthetic_dataset() -> (Vec<Value>, Vec<Value>) {
    let groups = ["a", "b", "c"];
    let mut grp = Vec::new();
    let mut amt = Vec::new();
    for i in 0..12i64 {
        grp.push(Value::Str(groups[(i % 3) as usize].into()));
        amt.push(Value::Int((i + 1) * 10));
    }
    (grp, amt)
}

#[test]
fn batch_count_star_is_segment_split_invariant() {
    let (grp, amt) = synthetic_dataset();
    assert_split_invariant(
        "SELECT count(*) FROM t",
        grp.len(),
        ("grp", &grp),
        ("amt", &amt),
    );
}

#[test]
fn batch_sum_is_segment_split_invariant() {
    let (grp, amt) = synthetic_dataset();
    assert_split_invariant(
        "SELECT sum(amt) FROM t",
        grp.len(),
        ("grp", &grp),
        ("amt", &amt),
    );
}

#[test]
fn batch_min_and_max_are_segment_split_invariant() {
    let (grp, amt) = synthetic_dataset();
    assert_split_invariant(
        "SELECT min(amt), max(amt) FROM t",
        grp.len(),
        ("grp", &grp),
        ("amt", &amt),
    );
}

#[test]
fn batch_avg_is_segment_split_invariant() {
    // The ADR's own worked example: `AggPart::Avg` carries `(sum, count)`
    // and divides once at finalize, so uneven segments (which an
    // average-of-per-segment-averages merge would get wrong) are the
    // sensitive case -- see `naive_average_of_segment_averages_disagrees`
    // below for the negative control.
    let (grp, amt) = synthetic_dataset();
    assert_split_invariant(
        "SELECT avg(amt) FROM t",
        grp.len(),
        ("grp", &grp),
        ("amt", &amt),
    );
}

#[test]
fn batch_group_by_one_key_is_segment_split_invariant() {
    let (grp, amt) = synthetic_dataset();
    assert_split_invariant(
        "SELECT grp, count(*), sum(amt), avg(amt), min(amt), max(amt) \
         FROM t GROUP BY grp ORDER BY grp",
        grp.len(),
        ("grp", &grp),
        ("amt", &amt),
    );
}

/// 12 rows over 2 group keys (`grp` x `sub`, 3x2 = 6 groups, 2 rows each).
fn synthetic_two_key_dataset() -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let groups = ["a", "b", "c"];
    let subs = ["x", "y"];
    let mut grp = Vec::new();
    let mut sub = Vec::new();
    let mut amt = Vec::new();
    for i in 0..12i64 {
        grp.push(Value::Str(groups[(i % 3) as usize].into()));
        sub.push(Value::Str(subs[((i / 3) % 2) as usize].into()));
        amt.push(Value::Int((i + 1) * 10));
    }
    (grp, sub, amt)
}

#[test]
fn batch_group_by_two_keys_is_segment_split_invariant() {
    let (grp, sub, amt) = synthetic_two_key_dataset();
    let n = grp.len();

    // `run_batch`/`segments_for` above only carry two columns; a 3-column
    // dataset needs its own segment builder.
    fn segments3(
        sizes: &[usize],
        grp: &[Value],
        sub: &[Value],
        amt: &[Value],
    ) -> Vec<InMemorySegment> {
        let mut offset = 0;
        sizes
            .iter()
            .map(|&size| {
                let batch = db_core::vm::batch::Batch::new(size)
                    .with_column("grp", grp[offset..offset + size].to_vec())
                    .with_column("sub", sub[offset..offset + size].to_vec())
                    .with_column("amt", amt[offset..offset + size].to_vec());
                offset += size;
                InMemorySegment(batch)
            })
            .collect()
    }

    // `vm::batch` only supports a single `ORDER BY` term, so a two-key
    // GROUP BY's output is sorted here instead, by the (grp, sub) group
    // key -- an ordinary caller comparing two runs would do the same.
    fn sort_by_group_key(rows: &mut [Vec<Value>]) {
        rows.sort_by(|a, b| {
            let key = |r: &[Value]| match (&r[0], &r[1]) {
                (Value::Str(g), Value::Str(s)) => (g.clone(), s.clone()),
                other => panic!("{other:?}"),
            };
            key(a).cmp(&key(b))
        });
    }

    let sql = "SELECT grp, sub, count(*), sum(amt) FROM t GROUP BY grp, sub";
    let program = compile(&parse(sql).unwrap()).unwrap();

    let sizings = segmentations(n);
    let mut baseline = run(&segments3(&sizings[0], &grp, &sub, &amt), &program).unwrap();
    sort_by_group_key(&mut baseline);
    for sizes in &sizings[1..] {
        let mut got = run(&segments3(sizes, &grp, &sub, &amt), &program).unwrap();
        sort_by_group_key(&mut got);
        assert_eq!(
            got, baseline,
            "two-key GROUP BY: segmentation {sizes:?} disagrees with {:?}",
            sizings[0]
        );
    }
}

/// Negative control (ADR-0025's own "sensitivity check"): a naive
/// average-of-per-segment-averages merge, reimplemented standalone here
/// (never touching `src/vm/batch.rs`), disagrees with the real merge on
/// an uneven split. This demonstrates the harness above would have
/// caught the bug the ADR names as the worked example, without shipping
/// any broken code.
#[test]
fn naive_average_of_segment_averages_disagrees_on_uneven_splits() {
    fn naive_merge(sizes: &[usize], amt: &[Value]) -> f64 {
        let mut offset = 0;
        let mut per_segment_avgs = Vec::new();
        for &size in sizes {
            if size == 0 {
                continue;
            }
            let sum: i64 = amt[offset..offset + size]
                .iter()
                .map(|v| match v {
                    Value::Int(n) => *n,
                    other => panic!("{other:?}"),
                })
                .sum();
            per_segment_avgs.push(sum as f64 / size as f64);
            offset += size;
        }
        per_segment_avgs.iter().sum::<f64>() / per_segment_avgs.len() as f64
    }

    let (grp, amt) = synthetic_dataset();
    let n = grp.len();
    let true_avg = {
        let rows = run_batch(
            "SELECT avg(amt) FROM t",
            &segmentations(n)[0],
            ("grp", &grp),
            ("amt", &amt),
        );
        match rows[0][0] {
            Value::Float(f) => f,
            ref other => panic!("{other:?}"),
        }
    };

    // The uneven split is the sensitive case: single-segment and
    // one-row-per-segment splits happen to agree with the naive merge
    // too (every segment the same size, or one row each), so only the
    // maximally-uneven segmentation actually distinguishes the two
    // merge strategies.
    let uneven = vec![1, 1, 1, n - 3];
    let naive_avg = naive_merge(&uneven, &amt);

    assert_ne!(
        naive_avg, true_avg,
        "naive average-of-averages should disagree with the real merge \
         on an uneven split -- if it doesn't, this dataset stopped being \
         a sensitive case"
    );
}

// ---------------------------------------------------------------------
// vm::batch: same invariance, over rows read from a real Parquet file
// ---------------------------------------------------------------------

const PARQUET_FIXTURE: &str = "tests/fixtures/parquet/production.parquet";

fn cell_to_value(c: &Cell) -> Value {
    match c {
        Cell::Null => Value::Null,
        Cell::Int(n) => Value::Int(*n),
        Cell::Real(f) => Value::Float(*f),
        Cell::Bool(b) => Value::Bool(*b),
        Cell::Text(s) => Value::Str(s.clone().into()),
        Cell::Blob(_) => panic!("unexpected BLOB in production.parquet"),
    }
}

#[test]
fn batch_group_by_over_parquet_rows_is_segment_split_invariant() {
    // Read 200 real rows once via `BatchEngine` (whose own segmentation
    // is fixed at one segment per Parquet row group -- see
    // `src/engine/column.rs`'s doc comment), then re-segment those exact
    // rows by hand: obligation 1 says the *query*, not the reader, must
    // not depend on segmentation, so the reader's own row-group split is
    // irrelevant here.
    let mut engine = BatchEngine::open(Path::new(PARQUET_FIXTURE)).expect("open parquet fixture");
    let result = engine
        .run_query("SELECT region, amount FROM production ORDER BY id LIMIT 200")
        .expect("read parquet rows");
    let region: Vec<Value> = result.rows.iter().map(|r| cell_to_value(&r[0])).collect();
    let amount: Vec<Value> = result.rows.iter().map(|r| cell_to_value(&r[1])).collect();

    assert_split_invariant(
        "SELECT region, count(*), sum(amount), avg(amount), min(amount), max(amount) \
         FROM t GROUP BY region ORDER BY region",
        region.len(),
        ("region", &region),
        ("amount", &amount),
    );
}

// ---------------------------------------------------------------------
// vm::stream: seal-boundary variation and eviction into `SegmentSummary`
// ---------------------------------------------------------------------

fn temp_log(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "db-core-segment-split-invariance-{}-{name}.log",
        std::process::id()
    ));
    std::fs::remove_file(&p).ok();
    p
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// One RFC 3164 line with a controlled `(facility, severity)` PRI and an
/// index used only to make lines distinct.
fn pri_line(facility: u8, severity: u8, n: usize) -> String {
    let pri = facility * 8 + severity;
    format!("<{pri}>Sep 10 08:00:00 h app[{n}]: line {n}\n")
}

fn rows(e: &mut StreamEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

/// 24 lines (4 facilities x 6 severities), split into `batch_sizes`-sized
/// chunks: the first chunk is written to `path` *before* the engine opens
/// on it -- `StreamEngine::open_with_budget` detects the log format once,
/// from whatever bytes are already there, and never re-detects on
/// `refresh` (`src/engine/stream.rs`'s `open_with_budget`), so opening on
/// a still-empty file would permanently lock in a format that never
/// resolves `facility`/`severity`. Every later chunk is appended and
/// followed by `refresh`, which is what varies where seal boundaries
/// fall.
fn ingest_in_batches(path: &Path, budget: usize, batch_sizes: &[usize]) -> StreamEngine {
    let facilities = [0u8, 4, 9, 16]; // kern, auth, cron, local0
    let severities = [0u8, 1, 2, 3, 4, 5];
    let mut n = 0usize;
    let mut remaining: Vec<(u8, u8)> = facilities
        .iter()
        .flat_map(|&f| severities.iter().map(move |&s| (f, s)))
        .collect();

    let chunk_text = |size: usize, n: &mut usize, remaining: &mut Vec<(u8, u8)>| {
        let mut text = String::new();
        for _ in 0..size {
            let (f, s) = remaining.remove(0);
            text.push_str(&pri_line(f, s, *n));
            *n += 1;
        }
        text
    };

    let (&first, rest) = batch_sizes
        .split_first()
        .expect("batch_sizes must have at least one chunk");
    std::fs::write(path, chunk_text(first, &mut n, &mut remaining)).unwrap();
    let mut e = StreamEngine::open_with_budget(path, budget).unwrap();

    for &size in rest {
        append(path, &chunk_text(size, &mut n, &mut remaining));
        e.refresh().unwrap();
    }
    assert!(
        remaining.is_empty(),
        "batch_sizes did not cover all 24 lines"
    );
    e
}

const STREAM_QUERIES: &[&str] = &[
    "SELECT count(*) FROM log",
    "SELECT sum(severity) FROM log",
    "SELECT avg(severity) FROM log",
    "SELECT min(severity), max(severity) FROM log",
    "SELECT facility, count(*), sum(severity) FROM log GROUP BY facility ORDER BY facility",
];

/// Runs every query in `STREAM_QUERIES` and returns the rows for each.
fn query_all(e: &mut StreamEngine) -> Vec<Vec<Vec<Cell>>> {
    STREAM_QUERIES.iter().map(|sql| rows(e, sql)).collect()
}

#[test]
fn stream_aggregates_are_invariant_to_seal_boundaries() {
    // A "cold" oracle: all 24 lines ingested as one chunk, budget large
    // enough that nothing is ever evicted.
    let oracle_path = temp_log("seal-boundaries-oracle");
    let mut oracle = ingest_in_batches(&oracle_path, 64 * 1024 * 1024, &[24]);
    let expected = query_all(&mut oracle);

    // Same 24 rows, same generous budget (no eviction in play here --
    // that is exercised separately below), but the seal/refresh
    // boundaries fall in different places.
    let variant_seal_boundaries: &[&[usize]] = &[
        &[1; 24],       // seal after every single line
        &[6, 6, 6, 6],  // seal after each facility's block
        &[1, 1, 1, 21], // maximally uneven
        &[3, 9, 0, 12], // a zero-size refresh (no new bytes) in the middle
    ];
    for sizes in variant_seal_boundaries {
        let path = temp_log(&format!("seal-boundaries-{sizes:?}"));
        let mut e = ingest_in_batches(&path, 64 * 1024 * 1024, sizes);
        let got = query_all(&mut e);
        assert_eq!(
            got, expected,
            "seal boundaries {sizes:?} disagree with the single-chunk oracle"
        );
    }
}

/// Queries `merge_retained_summaries` (`src/engine/stream.rs`) actually
/// documents as folding evicted `SegmentSummary` data back in: an
/// ungrouped `COUNT`/`SUM`/`MIN`/`MAX` with an explicit range wide enough
/// to require reaching past what the ring alone holds. `GROUP BY` and
/// `AVG` are excluded on purpose -- that function's own doc comment says
/// neither is folded from a summary at all (a summary has no rows left
/// to bucket by group, and `AVG`'s sum/count are already divided by the
/// time a live answer comes back, so there's nothing left to merge a
/// summary's own sum/count into) -- so obligation 1 does not yet extend
/// to them for evicted stream data; that gap is real but is a feature
/// gap in `merge_retained_summaries`, not a segment-split-invariance bug.
const EVICTION_QUERIES: &[&str] = &[
    "SELECT count(*) FROM log SINCE 1 DAY",
    "SELECT sum(severity) FROM log SINCE 1 DAY",
    "SELECT min(severity), max(severity) FROM log SINCE 1 DAY",
];

fn query_eviction_all(e: &mut StreamEngine) -> Vec<Vec<Vec<Cell>>> {
    EVICTION_QUERIES.iter().map(|sql| rows(e, sql)).collect()
}

/// A thin `Clock` forwarding to a shared `Arc<FakeClock>`, so a test can
/// keep advancing the same clock the engine reads `now_ns()` from --
/// lifted from `tests/unit/engine_stream_summaries_test.rs`'s own helper
/// of the same shape.
struct FakeClockHandle(std::sync::Arc<db_core::clock::FakeClock>);

impl db_core::clock::Clock for FakeClockHandle {
    fn now_ns(&self) -> i64 {
        self.0.now_ns()
    }
}

#[test]
fn stream_aggregates_are_invariant_to_ring_eviction() {
    // ADR 0018's own example, mirrored from
    // `engine_stream_summaries_test.rs`: a live `SINCE 1 DAY` query over
    // a ring that has evicted most of that day into summaries must match
    // a fresh, fully-hot scan of the same file. `FakeClock` makes "1 day"
    // and "one facility per hour" real simulated units, not a scaled-down
    // stand-in, and keeps both engines' idea of "now" identical -- a real
    // wall clock would let the two queries race apart by however long
    // this test takes to run.
    let path = temp_log("eviction");
    let clock = std::sync::Arc::new(db_core::clock::FakeClock::new(0));
    let mut hot = StreamEngine::open_with_budget(&path, 64).unwrap();
    hot.set_clock(Box::new(FakeClockHandle(clock.clone())));

    let facilities = [0u8, 4, 9, 16]; // kern, auth, cron, local0
    let mut n = 0usize;
    for &f in &facilities {
        let mut text = String::new();
        for s in 0u8..6 {
            text.push_str(&pri_line(f, s, n));
            n += 1;
        }
        append(&path, &text);
        clock.advance(60 * 60 * 1_000_000_000); // 1 simulated hour
        assert_eq!(hot.refresh().unwrap(), 6);
    }
    assert!(
        hot.ring().rows() < 24,
        "test is only meaningful if the tiny-budget ring evicted something \
         (ring holds {} of 24 rows)",
        hot.ring().rows()
    );

    // The cold, fully-hot oracle: a fresh scan of the whole file, sharing
    // the same (now fully advanced) clock so `SINCE 1 DAY` covers the
    // same window on both sides.
    let mut cold = StreamEngine::open_with_budget(&path, 64 * 1024 * 1024).unwrap();
    cold.set_clock(Box::new(FakeClockHandle(clock)));

    let expected = query_eviction_all(&mut cold);
    let got = query_eviction_all(&mut hot);
    assert_eq!(
        got, expected,
        "evicted-segment answers (from SegmentSummary) disagree with the \
         fully-resident-ring oracle"
    );
}

#[test]
fn stream_aggregates_are_invariant_over_a_real_syslog_fixture() {
    // The generated-data test above gives full control over facility and
    // severity with simulated time; this repeats the same idea (tiny
    // budget forcing eviction vs a huge, never-evicting budget) over the
    // real seeded fixture used elsewhere in the suite, so the property is
    // also checked against data nobody hand-crafted for this test. A
    // 100-year `SINCE` and the real system clock are wide enough that
    // this fixture's own (unknown, but certainly not centuries-old)
    // embedded dates fall inside the window regardless.
    //
    // The fixture alone (70 KB) is smaller than one ring block (256 KiB,
    // `storage::stream::file::BLOCK_SIZE`), so it becomes exactly one
    // segment at open -- and `Ring::evict_over_budget` never evicts the
    // last segment regardless of budget ("the ring always holds the
    // head"). Forcing this fixture's own data to be evicted needs a
    // second segment: append several synthetic batches after opening so
    // each becomes its own segment via `refresh`, so a small budget then
    // evicts the original fixture segment first.
    const FIXTURE: &str = "tests/fixtures/stream/syslog-1k.log";
    let fixture_bytes = std::fs::read(FIXTURE).unwrap();
    const WIDE_EVICTION_QUERIES: &[&str] = &[
        "SELECT count(*) FROM log SINCE 36500 DAY",
        "SELECT sum(severity) FROM log SINCE 36500 DAY",
        "SELECT min(severity), max(severity) FROM log SINCE 36500 DAY",
    ];

    let make = |name: &str, budget: usize| -> StreamEngine {
        let path = temp_log(name);
        std::fs::write(&path, &fixture_bytes).unwrap();
        let mut e = StreamEngine::open_with_budget(&path, budget).unwrap();
        for batch in 0..8usize {
            let mut text = String::new();
            for i in 0..50usize {
                text.push_str(&pri_line(4, (i % 6) as u8, 100_000 + batch * 50 + i));
            }
            append(&path, &text);
            e.refresh().unwrap();
        }
        e
    };

    let mut resident = make("real-fixture-resident", 64 * 1024 * 1024);
    let mut evicting = make("real-fixture-evicting", 4096);
    assert!(
        evicting.ring().rows() < resident.ring().rows(),
        "test is only meaningful if the tiny-budget ring evicted something \
         (evicting holds {}, resident holds {})",
        evicting.ring().rows(),
        resident.ring().rows()
    );

    let query_all = |e: &mut StreamEngine| -> Vec<Vec<Vec<Cell>>> {
        WIDE_EVICTION_QUERIES
            .iter()
            .map(|sql| rows(e, sql))
            .collect()
    };
    let expected = query_all(&mut resident);
    let got = query_all(&mut evicting);
    assert_eq!(
        got, expected,
        "evicted-segment answers over the real syslog fixture disagree \
         with the fully-resident-ring oracle"
    );
}
