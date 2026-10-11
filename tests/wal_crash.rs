// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! WAL crash safety (#571): SIGKILL a writer (`tests/helpers/wal_probe.rs`)
//! at many points and check, after each recovery, that
//!
//! - every record the writer printed as `Durable`-acked is present, with
//!   its exact payload;
//! - the recovered log is the contiguous run of LSNs `1..=n` (recovery
//!   refuses anything else), and every recovered payload is well formed,
//!   so nothing is half-present;
//! - the next writer continues the same log.
//!
//! SIGKILL cannot tear a `write` (the bytes are already in the page
//! cache), so after each kill the test also simulates losing power: it
//! cuts the last segment file at a random point after the last
//! durable-acked frame, as an OS crash may lose unsynced bytes, so
//! recovery's torn-tail path runs end to end too.
//!
//! The pattern is trigrep's `tests/crash.rs`. `WAL_TORTURE_ITERATIONS`
//! overrides the default of 25 kills.

#![cfg(feature = "ingest")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "test code fails fast (db-core#230)"
)]

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use db_core::storage::ingest::{Wal, WalOptions};

const PROBE: &str = env!("CARGO_BIN_EXE_wal_probe");

fn iterations() -> u32 {
    std::env::var("WAL_TORTURE_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25)
}

/// `<thread>:<n>:` followed by `n % 300` copies of the thread's letter.
fn well_formed(payload: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(payload) else {
        return false;
    };
    let mut parts = text.splitn(3, ':');
    let (Some(t), Some(n), Some(fill)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(t), Ok(n)) = (t.parse::<u8>(), n.parse::<u64>()) else {
        return false;
    };
    let letter = char::from(b'a' + t);
    fill.len() as u64 == n % 300 && fill.chars().all(|c| c == letter)
}

/// Runs the probe for `delay`, kills it, and returns what it printed as
/// durable: LSN -> payload. A line cut by the kill is ignored.
fn run_and_kill(table: &Path, delay: Duration) -> HashMap<u64, String> {
    let mut child = Command::new(PROBE)
        .arg(table)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(delay);
    child.kill().unwrap();
    child.wait().unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    out.lines()
        .filter_map(|line| {
            let (lsn, payload) = line.split_once('\t')?;
            Some((lsn.parse().ok()?, payload.to_string()))
        })
        .collect()
}

/// Cuts the last segment file somewhere after the end of the frame for
/// `max_acked` (or anywhere, if that frame is in an earlier file).
/// Returns whether anything was cut.
fn tear_unacked_tail(table: &Path, max_acked: u64, seed: u64) -> bool {
    let Ok(entries) = std::fs::read_dir(table.join("wal")) else {
        return false; // killed before the WAL directory existed
    };
    let mut files: Vec<_> = entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    files.sort();
    let Some(last) = files.last() else {
        return false;
    };
    let bytes = std::fs::read(last).unwrap();
    let mut protected = 0usize;
    let mut at = 0usize;
    while at + 16 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let lsn = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap());
        let end = at + 16 + len;
        if end > bytes.len() {
            break;
        }
        if lsn <= max_acked {
            protected = end;
        }
        at = end;
    }
    if protected >= bytes.len() {
        return false;
    }
    let span = bytes.len() - protected;
    let cut = protected + (seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 33) as usize % span;
    let file = std::fs::OpenOptions::new().write(true).open(last).unwrap();
    file.set_len(cut as u64).unwrap();
    true
}

#[test]
fn sigkill_never_loses_a_durable_ack_or_leaves_half_a_record() {
    let table = std::env::temp_dir().join(format!("db-core-wal-crash-{}", std::process::id()));
    std::fs::remove_dir_all(&table).ok();
    let mut acked: HashMap<u64, String> = HashMap::new();
    let mut recovered_total = 0;
    let mut tears = 0;
    for i in 0..iterations() {
        // Spread kills from just after start-up to well into the run.
        let delay = Duration::from_millis(5 + u64::from(i * 37 % 150));
        acked.extend(run_and_kill(&table, delay));
        let max_acked = acked.keys().copied().max().unwrap_or(0);
        if tear_unacked_tail(&table, max_acked, u64::from(i)) {
            tears += 1;
        }

        let (wal, records) = Wal::open(&table, 0, WalOptions::default())
            .unwrap_or_else(|e| panic!("iteration {i}: recovery failed: {e}"));
        for (n, (lsn, payload)) in records.iter().enumerate() {
            assert_eq!(*lsn, n as u64 + 1, "iteration {i}: LSNs not contiguous");
            assert!(
                well_formed(payload),
                "iteration {i}: LSN {lsn} is half-present: {payload:?}"
            );
        }
        let by_lsn: HashMap<u64, &[u8]> = records.iter().map(|(l, p)| (*l, p.as_slice())).collect();
        for (lsn, payload) in &acked {
            let got = by_lsn
                .get(lsn)
                .unwrap_or_else(|| panic!("iteration {i}: durable-acked LSN {lsn} lost"));
            assert_eq!(
                *got,
                payload.as_bytes(),
                "iteration {i}: LSN {lsn} payload differs"
            );
        }
        assert!(
            records.len() as u64 >= max_acked,
            "iteration {i}: lost acked records"
        );
        recovered_total = records.len();
        wal.close().unwrap();
    }
    assert!(
        !acked.is_empty(),
        "the probe never acked anything: kills too early"
    );
    assert!(tears > 0, "no unacked tail was ever cut");
    eprintln!(
        "{} kills, {tears} simulated power losses, {} durable acks checked, {recovered_total} records recovered",
        iterations(),
        acked.len()
    );
    std::fs::remove_dir_all(&table).ok();
}
