//! A clock per test for the table tests under `src/` (#726). Each test runs
//! on its own thread, so the value is kept per thread: tests that move the
//! clock no longer share one global (and hold a lock around it), and run in
//! parallel. (`thread_local!` is outside the qualified subset, so the values
//! sit in a list keyed by thread id; the lock is held for one lookup only.)

use std::sync::{Mutex, PoisonError};
use std::thread::{self, ThreadId};

static CLOCKS: Mutex<Vec<(ThreadId, i64)>> = Mutex::new(Vec::new());

/// This thread's clock, or `initial` if the test has not set it.
pub(crate) fn now(initial: i64) -> i64 {
    let clocks = CLOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    let me = thread::current().id();
    clocks
        .iter()
        .find(|(id, _)| *id == me)
        .map_or(initial, |(_, ns)| *ns)
}

/// Sets this thread's clock.
pub(crate) fn set(ns: i64) {
    let mut clocks = CLOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    let me = thread::current().id();
    match clocks.iter_mut().find(|(id, _)| *id == me) {
        Some((_, slot)) => *slot = ns,
        None => clocks.push((me, ns)),
    }
}
