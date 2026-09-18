//! Per-commit cost of the row pager's two journaling modes (#487): one
//! dirty page, `flush()`, repeated -- the `BEGIN; INSERT; COMMIT` shape
//! `benchmark/perf/sqlite-rs`'s `insert_single_tx` measures, minus the
//! SQL layer, so the number is the pager/VFS/syscall cost alone. Four
//! variants: rollback journal and WAL, each at `synchronous=FULL` (the
//! default, fsync per commit) and `NORMAL` (WAL: no per-commit fsync),
//! so FULL minus NORMAL is exactly what the commit's fsync costs on this
//! machine's filesystem. **Report only** -- `make perf`, not a CI gate
//! (ADR 0015, tier 6).
//!
//! Real files under a temp dir (a `MemoryVfs` would hide every syscall
//! this bench exists to measure); the `-wal` grows across iterations
//! without a checkpoint, as it does across the benchmark's scripted
//! commits.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "benches/ is unconstrained like tests/ (ADR 0015, tier 6)"
)]

mod common;

use std::hint::black_box;
use std::path::{Path, PathBuf};

use db_core::storage::row::header::{JournalMode, SynchronousMode};
use db_core::storage::row::pager::Pager;
use db_core::storage::row::vfs::UnixVfs;

const PAGE_SIZE: u32 = 4096;
/// 1 MB fixture, like the tier-1 benchmark's.
const PAGES: u32 = 256;
/// SQLite header: database size in pages, big-endian u32.
const PAGE_COUNT_OFFSET: usize = 28;

struct TempDb {
    dir: PathBuf,
    path: PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "db-core-wal-commit-bench-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bench.sqlite");
        let mut contents = vec![0u8; (PAGE_SIZE * PAGES) as usize];
        // Legacy (rollback-journal) file format version bytes at 18/19;
        // `set_journal_mode(Wal)` flips them to 2/2 on the WAL variants.
        contents[18] = 1;
        contents[19] = 1;
        contents[PAGE_COUNT_OFFSET..PAGE_COUNT_OFFSET + 4].copy_from_slice(&PAGES.to_be_bytes());
        std::fs::write(&path, &contents).unwrap();
        TempDb { dir, path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn open(path: &Path, mode: JournalMode, sync: SynchronousMode) -> Pager {
    let mut pager = Pager::open(&UnixVfs, path, PAGE_SIZE).unwrap();
    pager.set_journal_mode(mode).unwrap();
    pager.set_synchronous(sync);
    pager
}

fn bench_commit(r: &mut common::Report, name: &str, mode: JournalMode, sync: SynchronousMode) {
    let db = TempDb::new(name);
    let mut pager = open(&db.path, mode, sync);
    let mut n: u8 = 0;
    r.bench(&format!("wal_commit/{name}"), || {
        n = n.wrapping_add(1);
        // One dirty page per commit: the single-row-insert shape.
        pager.get_page_mut(2 + u32::from(n % 200)).unwrap().fill(n);
        pager.flush().unwrap();
        black_box(n)
    });
}

/// The tier-1 benchmark's actual shape: every iteration starts from a
/// fresh copy of the fixture, opens it, switches journal mode (as the
/// scripted `PRAGMA journal_mode=…` does), and commits once. Whatever
/// the mode switch and first-commit file creation cost lands here and
/// *not* in the steady-state numbers above -- which is how a flat
/// per-script cost can masquerade as a per-commit one in a benchmark
/// that runs one short script per iteration.
fn bench_script(r: &mut common::Report, name: &str, mode: JournalMode, sync: SynchronousMode) {
    let template = TempDb::new(&format!("{name}-template"));
    let run_dir = TempDb::new(&format!("{name}-runs"));
    let mut n: u32 = 0;
    r.bench(&format!("wal_commit/script: {name}"), || {
        n = n.wrapping_add(1);
        let path = run_dir.dir.join(format!("run-{n}.sqlite"));
        std::fs::copy(&template.path, &path).unwrap();
        let mut pager = open(&path, mode, sync);
        pager.get_page_mut(2).unwrap().fill(7);
        pager.flush().unwrap();
        drop(pager);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
        }
        black_box(n)
    });
}

fn main() {
    let mut report = common::Report::new("wal_commit");
    bench_script(
        &mut report,
        "fresh fixture, open, 1 commit (journal, FULL)",
        JournalMode::Legacy,
        SynchronousMode::Full,
    );
    bench_script(
        &mut report,
        "fresh fixture, open, switch to WAL, 1 commit (FULL)",
        JournalMode::Wal,
        SynchronousMode::Full,
    );
    bench_commit(
        &mut report,
        "journal, synchronous=FULL (1 dirty page)",
        JournalMode::Legacy,
        SynchronousMode::Full,
    );
    bench_commit(
        &mut report,
        "journal, synchronous=NORMAL (1 dirty page)",
        JournalMode::Legacy,
        SynchronousMode::Normal,
    );
    bench_commit(
        &mut report,
        "wal, synchronous=FULL (1 dirty page)",
        JournalMode::Wal,
        SynchronousMode::Full,
    );
    bench_commit(
        &mut report,
        "wal, synchronous=NORMAL (1 dirty page)",
        JournalMode::Wal,
        SynchronousMode::Normal,
    );
    report.finish();
}
