// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for #308's ring summaries and autoscaling:
//! `db_core::engine::stream::StreamEngine` merging retained per-segment
//! summaries into an ungrouped `COUNT`/`SUM`/`MIN`/`MAX` when the ring
//! alone no longer covers the requested scope, and ring autoscaling
//! staying within its clamp. Driven entirely by `db_core::clock::FakeClock`
//! -- no real sleeping, so a "42-minute ring" or a sustained-rate burst is
//! simulated by advancing the fake clock, not by waiting.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use db_core::clock::FakeClock;
use db_core::engine::stream::{StreamEngine, HARD_CAP_RING_BUDGET, MIN_RING_BUDGET};
use db_core::engine::{Cell, Engine};

fn temp_log(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "db-core-stream-summaries-{}-{name}.log",
        std::process::id()
    ));
    std::fs::remove_file(&p).ok();
    std::fs::write(&p, b"").unwrap();
    p
}

fn line(n: usize) -> String {
    format!("<134>Sep 10 08:00:00 h app[{n}]: line {n}\n")
}

fn append(path: &std::path::Path, text: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn count_star(e: &mut StreamEngine) -> i64 {
    let rows = e.run_query("SELECT count(*) FROM log").unwrap().rows;
    match &rows[0][0] {
        Cell::Int(n) => *n,
        other => panic!("{other:?}"),
    }
}

/// ADR 0018's own example, literally: "a live `count(*) ... since 1d`
/// over a 42-minute ring merges a day of summaries, re-aggregates the
/// hot ring, and adds the head." The clock is simulated (`FakeClock`,
/// advanced by real minutes -- no wall-clock cost, this test runs in
/// milliseconds), so "42 minutes" and "1 day" are the ADR's own numbers,
/// not a scaled-down stand-in.
#[test]
fn count_star_since_1d_over_a_42_minute_ring_matches_the_cold_bounded_total() {
    let path = temp_log("cold-oracle-1d");
    let clock = std::sync::Arc::new(FakeClock::new(0));
    // One line's worth of budget: byte eviction, not time, drives the
    // ring here -- only the newest batch or two stay hot regardless of
    // how much simulated time has passed, which is exactly what makes
    // "since 1d over a 42-minute ring" a meaningful merge-summaries case
    // rather than a query the ring alone could already answer.
    let mut e = StreamEngine::open_with_budget(&path, 64).unwrap();
    e.set_clock(Box::new(FakeClockHandle(clock.clone())));

    // 24 batches, one simulated hour apart: a full day of ingestion.
    let mut total = 0i64;
    for batch in 0..24 {
        let mut text = String::new();
        for n in 0..5 {
            text.push_str(&line(batch * 5 + n));
            total += 1;
        }
        append(&path, &text);
        clock.advance(60 * 60 * 1_000_000_000); // 1 simulated hour
        assert_eq!(e.refresh().unwrap(), 5);
    }

    // At this point the ring holds only the last ~42 minutes' worth of
    // *data volume* (a handful of the newest lines) -- everything older
    // survives only as a retained summary, not as live rows.
    assert!(
        e.ring().rows() < total as usize,
        "test is only meaningful if the ring evicted something (ring holds {}, total {total})",
        e.ring().rows()
    );

    // The cold, fully-hot oracle: what a fresh scan of the whole file
    // would report, independent of any ring/summary machinery.
    let mut cold = StreamEngine::open_with_budget(&path, 64 * 1024 * 1024).unwrap();
    let cold_total = count_star(&mut cold);
    assert_eq!(cold_total, total);

    // `SINCE 1 DAY` widens the query well past the ring's own 42
    // minutes of hot data; retained summaries must close the gap.
    let got = match e
        .run_query("SELECT count(*) FROM log SINCE 1 DAY")
        .unwrap()
        .rows[0][0]
    {
        Cell::Int(n) => n,
        ref other => panic!("{other:?}"),
    };
    assert_eq!(got, total);
    assert_eq!(got, cold_total);
}

/// Ring autoscaling (ADR 0018 §Storage): `target = clamp(rate_ewma *
/// default_scope, min, hard_cap)`. ADR 0018's own "Done when": "memory
/// stays under `hard_cap` while tailing a synthetic 50 MB/min feed" --
/// simulated literally (real MB/min, real 10-second ticks via
/// `FakeClock`, no wall-clock cost) rather than scaled down.
#[test]
fn autoscaling_stays_under_the_hard_cap_tailing_a_synthetic_50mb_per_minute_feed() {
    const MB: usize = 1024 * 1024;
    let path = temp_log("autoscale-50mb-per-min");
    let clock = std::sync::Arc::new(FakeClock::new(0));
    let mut e = StreamEngine::open_with_budget(&path, MIN_RING_BUDGET).unwrap();
    e.set_clock(Box::new(FakeClockHandle(clock.clone())));
    e.enable_autoscaling(Duration::from_secs(60)); // target: hold ~1 minute hot

    // 50 MB/min = ~8.33 MB per simulated 10-second tick, sustained for
    // one simulated minute (6 ticks) -- long enough for the EWMA to
    // react and the clamp to be exercised, short enough (~50 MB total
    // parsed) to keep this a fast unit test.
    let bytes_per_tick = 50 * MB / 6;
    let line_bytes = b"<134>Sep 10 08:00:00 h app[0]: x\n".len();
    let lines_per_tick = bytes_per_tick / line_bytes;
    let one_line = "<134>Sep 10 08:00:00 h app[0]: x\n".to_string();

    for _ in 0..6 {
        let text = one_line.repeat(lines_per_tick);
        append(&path, &text);
        clock.advance(10 * 1_000_000_000); // 10 simulated seconds
        e.refresh().unwrap();
        assert!(
            e.ring().bytes() <= HARD_CAP_RING_BUDGET,
            "ring bytes {} exceeded the hard cap {HARD_CAP_RING_BUDGET} \
             while tailing a synthetic 50 MB/min feed",
            e.ring().bytes()
        );
        assert!(
            e.ring().budget() <= HARD_CAP_RING_BUDGET,
            "ring budget {} exceeded the hard cap {HARD_CAP_RING_BUDGET}",
            e.ring().budget()
        );
    }
    // A sustained 50 MB/min feed with a 1-minute target scope implies a
    // ~50 MB ring -- comfortably past the floor, otherwise autoscaling
    // never actually engaged.
    assert!(
        e.ring().budget() > MIN_RING_BUDGET,
        "expected the ring to have grown past the floor, got {}",
        e.ring().budget()
    );
}

/// A thin `Clock` forwarding to a shared `Arc<FakeClock>` so the test can
/// keep advancing the same clock the engine reads from.
struct FakeClockHandle(std::sync::Arc<FakeClock>);

impl db_core::clock::Clock for FakeClockHandle {
    fn now_ns(&self) -> i64 {
        self.0.now_ns()
    }
}
