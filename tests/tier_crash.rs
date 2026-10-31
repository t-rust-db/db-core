// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! Tiered-writer crash safety (#648): SIGKILL a writer
//! (`tests/helpers/tier_probe.rs`) that appends, closes hours, spills the
//! open hour and merges days under a fast clock, at many points, and after
//! each kill simulate a power loss before checking that
//!
//! - a `Snapshot` reader opens the table as it was left, and every file
//!   the manifest names is a complete Parquet file with the manifest's row
//!   count;
//! - every row the writer printed as durable-acked is present, and no row
//!   is present twice;
//! - the next writer recovers the table, and the same holds after it.
//!
//! The power loss: anything a durable manifest does not name may still be
//! in the page cache only, so every such file is deleted or cut short, a
//! directory holding no named file may lose its entry, and a pending
//! `manifest.json.tmp` (the rename the crash beat) is deleted or cut short.
//! Everything the manifest names was fsynced, file and every directory
//! entry on its path, before the manifest was, so none of it may be lost.
//!
//! `TIER_TORTURE_ITERATIONS` overrides the default of 25 kills.

#![cfg(all(feature = "ingest", feature = "engine-column"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230)"
)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[path = "helpers/tier_clock.rs"]
mod tier_clock;

use db_core::engine::column::table::TableEngine;
use db_core::engine::{Cell, Engine};
use db_core::storage::ingest::{Manifest, Snapshot, TableWriter, MANIFEST_FILE};
use db_core::storage::parquet::ParquetFile;

const PROBE: &str = env!("CARGO_BIN_EXE_tier_probe");
const PHASES: [&str; 3] = ["flush", "spill", "compact"];

fn iterations() -> u32 {
    std::env::var("TIER_TORTURE_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25)
}

/// What one probe run printed before the kill.
struct Run {
    acked: Vec<i64>,
    /// The phase that had begun and not ended, if any.
    in_flight: Option<String>,
    last_id: Option<i64>,
}

fn run_and_kill(table: &Path, origin: i64, first_id: i64, delay: Duration) -> Run {
    let mut child = Command::new(PROBE)
        .arg(table)
        .arg(origin.to_string())
        .arg(first_id.to_string())
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
    let mut run = Run {
        acked: Vec::new(),
        in_flight: None,
        last_id: None,
    };
    for line in out.lines() {
        let Some((word, rest)) = line.split_once(' ') else {
            continue; // cut by the kill
        };
        match word {
            "ack" => {
                let Some((lo, hi)) = rest.split_once("..=") else {
                    continue;
                };
                let (Ok(lo), Ok(hi)) = (lo.parse::<i64>(), hi.parse::<i64>()) else {
                    continue;
                };
                run.acked.extend(lo..=hi);
                run.last_id = Some(hi);
            }
            "begin" => run.in_flight = Some(rest.to_string()),
            "end" => run.in_flight = None,
            _ => {}
        }
    }
    run
}

/// A small deterministic generator for the power-loss choices.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Every file under `dir`, relative to `root`, with `/` separators.
fn walk(root: &Path, dir: &Path, files: &mut Vec<String>, dirs: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            dirs.push(rel);
            walk(root, &path, files, dirs);
        } else {
            files.push(rel);
        }
    }
}

/// Deletes or truncates `path` at random.
fn lose(path: &Path, rng: &mut Rng) {
    let len = std::fs::metadata(path).map_or(0, |m| m.len());
    if len == 0 || rng.below(2) == 0 {
        std::fs::remove_file(path).ok();
    } else {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_len(rng.below(len)).unwrap();
    }
}

/// What a power loss may take, given what the durable manifest names (see
/// the module docs). Returns how many files and directories it took.
fn lose_power(table: &Path, rng: &mut Rng) -> (usize, usize) {
    let named: HashSet<String> = Manifest::read(table)
        .unwrap()
        .map(|m| m.files.iter().map(|f| f.path.clone()).collect())
        .unwrap_or_default();
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    walk(table, table, &mut files, &mut dirs);
    let mut lost_files = 0;
    for file in &files {
        // The WAL's own tearing is `tests/wal_crash.rs`'s; the lock and the
        // manifest are synced in place.
        let kept = named.contains(file)
            || file.starts_with("wal/")
            || file == "LOCK"
            || file == MANIFEST_FILE;
        if !kept && rng.below(3) != 0 {
            lose(&table.join(file), rng);
            lost_files += 1;
        }
    }
    // Deepest first, so a parent goes after its children are decided.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
    let mut lost_dirs = 0;
    for dir in &dirs {
        let holds_named = named.iter().any(|f| f.starts_with(&format!("{dir}/")));
        if !holds_named && dir != "wal" && rng.below(3) == 0 {
            std::fs::remove_dir_all(table.join(dir)).ok();
            lost_dirs += 1;
        }
    }
    (lost_files, lost_dirs)
}

/// Every id in the table, in order, read by a fresh reader; each named
/// file checked complete first.
fn read_ids(table: &Path, label: &str) -> Vec<i64> {
    let snapshot =
        Snapshot::open(table).unwrap_or_else(|e| panic!("{label}: snapshot failed: {e}"));
    for entry in &snapshot.manifest().files {
        let bytes = std::fs::read(table.join(&entry.path))
            .unwrap_or_else(|e| panic!("{label}: {} unreadable: {e}", entry.path));
        let file = ParquetFile::open(&bytes)
            .unwrap_or_else(|e| panic!("{label}: {} is not a whole file: {e}", entry.path));
        assert_eq!(
            file.num_rows() as u64,
            entry.rows,
            "{label}: {} row count",
            entry.path
        );
    }
    let mut engine =
        TableEngine::open(table).unwrap_or_else(|e| panic!("{label}: engine open failed: {e}"));
    let name = table.file_name().unwrap().to_string_lossy().to_string();
    engine
        .run_query(&format!("SELECT id FROM {name} ORDER BY id"))
        .unwrap_or_else(|e| panic!("{label}: query failed: {e}"))
        .rows
        .into_iter()
        .map(|row| match row.into_iter().next() {
            Some(Cell::Int(id)) => id,
            other => panic!("{label}: id is {other:?}"),
        })
        .collect()
}

fn check(ids: &[i64], acked: &BTreeSet<i64>, label: &str) {
    for pair in ids.windows(2) {
        assert_ne!(pair[0], pair[1], "{label}: id {} present twice", pair[0]);
    }
    let present: HashSet<i64> = ids.iter().copied().collect();
    let lost: Vec<i64> = acked
        .iter()
        .filter(|id| !present.contains(id))
        .take(5)
        .copied()
        .collect();
    assert!(lost.is_empty(), "{label}: durable-acked ids lost: {lost:?}");
}

#[test]
fn sigkill_and_power_loss_during_flush_spill_and_compact_lose_no_durable_row() {
    let root: PathBuf =
        std::env::temp_dir().join(format!("db-core-tier-crash-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    let table = root.join("events");
    let origin = tier_clock::real_now();
    tier_clock::set_origin(origin);

    let mut acked: BTreeSet<i64> = BTreeSet::new();
    let mut by_phase: BTreeMap<String, u32> = BTreeMap::new();
    let mut lost = (0usize, 0usize);
    let mut rng = Rng(0x648);
    let base = iterations();
    let mut kills = 0u32;
    let covered =
        |by_phase: &BTreeMap<String, u32>| PHASES.iter().all(|p| by_phase.contains_key(*p));
    while kills < base || (!covered(&by_phase) && kills < base.saturating_mul(8)) {
        let i = kills;
        kills += 1;
        // Ids never repeat across runs, acked or not.
        let first_id = i64::from(i) * 10_000_000;
        let delay = Duration::from_millis(40 + rng.below(400));
        let run = run_and_kill(&table, origin, first_id, delay);
        acked.extend(&run.acked);
        let phase = run.in_flight.unwrap_or_else(|| "append".to_string());
        *by_phase.entry(phase.clone()).or_default() += 1;
        let label = format!("kill {i} during {phase} (last acked {:?})", run.last_id);

        if !table.join(MANIFEST_FILE).exists() && acked.is_empty() {
            continue; // killed before the table existed
        }
        let (files, dirs) = lose_power(&table, &mut rng);
        lost.0 += files;
        lost.1 += dirs;

        check(&read_ids(&table, &label), &acked, &label);
        // Recovery by the next writer, then the same checks.
        let writer = TableWriter::open(&table, tier_clock::options())
            .unwrap_or_else(|e| panic!("{label}: recovery failed: {e}"));
        writer.close().unwrap();
        let after = format!("{label}, after recovery");
        check(&read_ids(&table, &after), &acked, &after);
    }
    eprintln!(
        "{kills} kills by phase {by_phase:?}; {} durable acks checked; power loss took {} files and {} directories",
        acked.len(),
        lost.0,
        lost.1
    );
    for phase in PHASES {
        assert!(
            by_phase.contains_key(phase),
            "no kill landed during {phase}: {by_phase:?}"
        );
    }
    assert!(!acked.is_empty(), "the probe never acked anything");
    assert!(lost.0 > 0, "the power loss never took a file");
    std::fs::remove_dir_all(&root).ok();
}
