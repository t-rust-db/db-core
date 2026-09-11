// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for #309's standing queries:
//! `db_core::engine::stream::StandingQuery` re-evaluating a compiled
//! range-vector query on demand and firing per `EmitMode`. Driven entirely
//! by `db_core::clock::FakeClock` -- `poll`'s `Threshold` state machine
//! ("held for at least `for_duration`") is timed against the engine's own
//! clock, so a hold is simulated by advancing the fake clock between polls,
//! never by a real sleep.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use db_core::clock::FakeClock;
use db_core::engine::stream::{StandingQuery, StreamEngine};
use db_core::parser::ast::BinaryOp;
use db_core::vm::stream::EmitMode;

fn temp_log(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "db-core-stream-standing-{}-{name}.log",
        std::process::id()
    ));
    std::fs::remove_file(&p).ok();
    std::fs::write(&p, b"").unwrap();
    p
}

fn line(secs: u32, n: usize) -> String {
    format!("<134>Sep 10 08:00:{secs:02} h app[{n}]: line {n}\n")
}

fn append(path: &std::path::Path, text: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// A thin `Clock` forwarding to a shared `Arc<FakeClock>`, matching
/// `engine_stream_summaries_test.rs`'s own convention (`Box<dyn Clock>`
/// needs a concrete forwarding type since `FakeClock` itself isn't boxed
/// by the test).
struct FakeClockHandle(Arc<FakeClock>);

impl db_core::clock::Clock for FakeClockHandle {
    fn now_ns(&self) -> i64 {
        self.0.now_ns()
    }
}

fn open_with_fake_clock(path: &std::path::Path) -> (StreamEngine, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new(0));
    let mut e = StreamEngine::open_with_budget(path, 64 * 1024).unwrap();
    e.set_clock(Box::new(FakeClockHandle(clock.clone())));
    (e, clock)
}

/// #309's literal "done when": a threshold query over the synthetic feed
/// fires exactly once per transition, not per refresh/poll. 4 lines land
/// in the same 10-second window, so `count_over_time(message) RANGE 10
/// seconds` reduces to `4`, over the `> 3` threshold, for every poll from
/// the first one onward -- but `for_duration` (5s) is only satisfied
/// starting the third poll, and every poll after that must stay silent.
#[test]
fn threshold_fires_exactly_once_per_transition_not_per_poll() {
    let path = temp_log("threshold-transition");
    let (mut e, clock) = open_with_fake_clock(&path);

    let mut text = String::new();
    for s in [1, 2, 3, 4] {
        text.push_str(&line(s, s as usize));
    }
    append(&path, &text);
    e.refresh().unwrap();

    let mut sq = StandingQuery::new(
        &e,
        "SELECT count_over_time(message) RANGE 10 seconds FROM log",
        EmitMode::Threshold {
            op: BinaryOp::Gt,
            threshold: 3.0,
        },
        Duration::from_secs(1),
        Duration::from_secs(5),
    )
    .unwrap();

    let mut fires = 0usize;

    // t=0: condition starts holding, held_ns=0 < for_duration -> no fire.
    assert!(sq.poll(&mut e).unwrap().is_none());

    // t=3s: still under for_duration (5s) -> no fire.
    clock.advance(3_000_000_000);
    assert!(sq.poll(&mut e).unwrap().is_none());

    // t=6s: held continuously for 6s >= 5s -> fires, exactly once.
    clock.advance(3_000_000_000);
    if sq.poll(&mut e).unwrap().is_some() {
        fires += 1;
    }

    // t=7s, t=8s: condition still holds, already fired for this hold ->
    // must stay silent (this is the literal "not per refresh" criterion).
    clock.advance(1_000_000_000);
    assert!(sq.poll(&mut e).unwrap().is_none());
    clock.advance(1_000_000_000);
    assert!(sq.poll(&mut e).unwrap().is_none());

    assert_eq!(
        fires, 1,
        "threshold must fire exactly once across all polls"
    );
}

/// A condition that holds for less than `for_duration` and then drops
/// never fires at all.
#[test]
fn threshold_never_fires_if_it_drops_before_for_duration_elapses() {
    let path = temp_log("threshold-drops-early");
    let (mut e, clock) = open_with_fake_clock(&path);

    let mut text = String::new();
    for s in [1, 2, 3, 4] {
        text.push_str(&line(s, s as usize));
    }
    append(&path, &text);
    e.refresh().unwrap();

    let mut sq = StandingQuery::new(
        &e,
        "SELECT count_over_time(message) RANGE 10 seconds FROM log",
        EmitMode::Threshold {
            op: BinaryOp::Gt,
            threshold: 3.0,
        },
        Duration::from_secs(1),
        Duration::from_secs(30), // never reached in this test's short span
    )
    .unwrap();

    assert!(sq.poll(&mut e).unwrap().is_none());
    clock.advance(5_000_000_000);
    assert!(sq.poll(&mut e).unwrap().is_none());
    clock.advance(5_000_000_000);
    assert!(
        sq.poll(&mut e).unwrap().is_none(),
        "for_duration (30s) was never reached; must never fire"
    );
}

/// `OnChange` fires only on an actual transition: silent while the result
/// set repeats, fires when new data changes it.
#[test]
fn on_change_fires_only_when_the_result_set_changes() {
    let path = temp_log("on-change");
    let (mut e, _clock) = open_with_fake_clock(&path);

    append(&path, &line(1, 1));
    e.refresh().unwrap();

    let mut sq = StandingQuery::new(
        &e,
        "SELECT count_over_time(message) RANGE 10 seconds FROM log",
        EmitMode::OnChange,
        Duration::from_secs(1),
        Duration::from_secs(0),
    )
    .unwrap();

    // First poll: transition from nothing fired yet -> something -> fires.
    assert!(sq.poll(&mut e).unwrap().is_some());
    // Second poll, no new data: identical result set -> silent.
    assert!(sq.poll(&mut e).unwrap().is_none());

    // New data changes the window's count -> fires again.
    append(&path, &line(2, 2));
    e.refresh().unwrap();
    assert!(sq.poll(&mut e).unwrap().is_some());
    // Polling again with no further change -> silent.
    assert!(sq.poll(&mut e).unwrap().is_none());
}
