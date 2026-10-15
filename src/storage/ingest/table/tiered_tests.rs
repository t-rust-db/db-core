//! Tiered layout (ADR 0028, #573): hours into `live/`, days into
//! `archive/`, under a clock the test sets.

use super::*;
use crate::storage::ingest::layout::{TierOptions, DAY_NS, HOUR_NS};
use crate::storage::parquet::ParquetFile;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The tests' clock: shared, so tests that use it hold [`serial`].
static NOW: AtomicI64 = AtomicI64::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn clock() -> i64 {
    NOW.load(Ordering::SeqCst)
}

/// Sets the clock to `day` days and `hour` hours past 2026-10-11, plus
/// `ns`.
fn set_clock(day: i64, hour: i64, ns: i64) {
    NOW.store(
        (20_737 + day) * DAY_NS + hour * HOUR_NS + ns,
        Ordering::SeqCst,
    );
}

struct TempTable(PathBuf);

impl TempTable {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("db-core-tiered-{}-{n}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        TempTable(dir)
    }
}

impl Drop for TempTable {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn tier() -> TierOptions {
    TierOptions {
        sort_key: vec!["kind".to_string()],
        row_group_max_rows: 3,
        max_file_size: 1 << 30,
        delete_grace_ns: 60_000_000_000,
    }
}

fn options(tier: TierOptions) -> TableOptions {
    TableOptions {
        layout: Layout::Tiered(tier),
        clock,
        ..TableOptions::default()
    }
}

fn open(t: &TempTable) -> TableWriter {
    TableWriter::open(&t.0, options(tier())).unwrap()
}

/// Record `i`: `kind` cycles c, a, b so sorting changes the order.
fn line(i: usize) -> String {
    let kind = ["c", "a", "b"][i % 3];
    format!(r#"{{"i":{i},"kind":"{kind}"}}"#)
}

fn append(w: &mut TableWriter, range: std::ops::Range<usize>) -> Vec<String> {
    range
        .map(|i| {
            let l = line(i);
            w.append(l.as_bytes(), Ack::Durable).unwrap();
            l
        })
        .collect()
}

/// Every record's `raw`: the manifest's files, then the pages, sorted (a
/// multiset, order-independent).
fn all_rows(w: &TableWriter) -> Vec<String> {
    let s = w.snapshot().unwrap();
    let mut rows = Vec::new();
    for (_, bytes) in s.files() {
        let file = ParquetFile::open(bytes).unwrap();
        let raw = file.metadata().schema[1..]
            .iter()
            .position(|e| e.name == "raw")
            .unwrap();
        for rg in file.row_groups() {
            rows.extend(rg.read_string_column(raw).unwrap().into_iter().flatten());
        }
    }
    for page in s.pages() {
        for i in 0..page.lines().unwrap().len() {
            rows.push(String::from_utf8(page.lines().unwrap().raw_line(i).to_vec()).unwrap());
        }
    }
    rows.sort();
    rows
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Every Parquet file under the table's areas, relative to it.
fn files_on_disk(t: &TempTable) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries {
            let e = e.unwrap();
            let rel = format!("{rel}/{}", e.file_name().to_string_lossy());
            if e.file_type().unwrap().is_dir() {
                walk(&e.path(), &rel, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    for area in ["data", "live", "archive"] {
        walk(&t.0.join(area), area, &mut out);
    }
    out.sort();
    out
}

fn listed(w: &TableWriter) -> Vec<String> {
    let mut v: Vec<String> = w.manifest().files.iter().map(|f| f.path.clone()).collect();
    v.sort();
    v
}

#[test]
fn hours_close_into_live_and_days_merge_into_archive() {
    let _serial = serial();
    let t = TempTable::new();
    set_clock(0, 10, 0);
    let mut w = open(&t);
    let mut lines = append(&mut w, 0..7);
    assert!(w.flush().unwrap().is_empty(), "hour 10 is still open");
    set_clock(0, 11, 5);
    lines.extend(append(&mut w, 7..9));
    let added = w.flush().unwrap();
    // Seven rows of hour 10 in row groups of 3: one file, LSNs 1-7.
    assert_eq!(added.len(), 1);
    assert_eq!(
        added[0].path,
        format!("live/date=2026-10-11/hour=10/{:020}-{:020}.parquet", 1, 7)
    );
    assert_eq!(added[0].area, Area::Live);
    assert_eq!(w.manifest().flushed_lsn, 7);
    assert_eq!(all_rows(&w), sorted(lines.clone()));

    // The hour's rows sorted by kind, then cut into row groups of 3: each
    // row group a range of kinds.
    let bytes = fs::read(t.0.join(&added[0].path)).unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let kind = file.metadata().schema[1..]
        .iter()
        .position(|e| e.name == "kind")
        .unwrap();
    let raw = file.metadata().schema[1..]
        .iter()
        .position(|e| e.name == "raw")
        .unwrap();
    let groups: Vec<Vec<String>> = file
        .row_groups()
        .map(|rg| {
            rg.read_string_column(kind)
                .unwrap()
                .into_iter()
                .flatten()
                .collect()
        })
        .collect();
    assert_eq!(
        groups,
        [vec!["a", "a", "b"], vec!["b", "c", "c"], vec!["c"]]
    );
    let first_group: Vec<String> = file
        .row_group(0)
        .unwrap()
        .read_string_column(raw)
        .unwrap()
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        first_group,
        vec![line(1), line(4), line(2)],
        "ties keep arrival order"
    );

    // The next day: hour 11 closes, and day 0 merges into the archive.
    set_clock(1, 0, 0);
    lines.extend(append(&mut w, 9..10));
    w.flush().unwrap();
    assert_eq!(w.manifest().files.len(), 2);
    let merged = w.compact().unwrap();
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].path,
        format!("archive/date=2026-10-11/{:020}-{:020}.parquet", 1, 9)
    );
    assert_eq!(
        (merged[0].lsn_lo, merged[0].lsn_hi, merged[0].rows),
        (1, 9, 9)
    );
    assert_eq!(w.manifest().files.len(), 1);
    assert_eq!(all_rows(&w), sorted(lines.clone()));
    let archived = fs::read(t.0.join(&merged[0].path)).unwrap();
    let file = ParquetFile::open(&archived).unwrap();
    assert_eq!(
        file.num_row_groups(),
        4,
        "row groups copied, never spanning an hour"
    );

    // The replaced hours wait out the grace period.
    assert_eq!(w.pending_deletes().count(), 2);
    assert_eq!(files_on_disk(&t).len(), 3);
    assert!(w.compact().unwrap().is_empty(), "nothing more to merge");
    set_clock(1, 0, 61_000_000_000);
    w.compact().unwrap();
    assert_eq!(w.pending_deletes().count(), 0);
    assert_eq!(files_on_disk(&t), listed(&w));
    assert!(
        !t.0.join("live/date=2026-10-11").exists(),
        "empty day directory removed"
    );

    // A reopened table has the same rows.
    drop(w);
    let w = open(&t);
    assert_eq!(all_rows(&w), sorted(lines));
}

#[test]
fn an_idle_hour_closes_when_the_clock_moves_on() {
    let _serial = serial();
    let t = TempTable::new();
    set_clock(0, 10, 0);
    let mut w = open(&t);
    let lines = append(&mut w, 0..2);
    set_clock(0, 12, 0);
    let added = w.flush().unwrap();
    assert_eq!(added.len(), 1, "the head of hour 10 is sealed and written");
    assert!(w.head_page().is_none());
    assert_eq!(all_rows(&w), sorted(lines));
}

#[test]
fn reopening_seals_replayed_records_at_hour_boundaries() {
    let _serial = serial();
    let t = TempTable::new();
    set_clock(0, 10, 0);
    let mut w = open(&t);
    let mut lines = append(&mut w, 0..2);
    set_clock(0, 11, 0);
    lines.extend(append(&mut w, 2..4));
    set_clock(0, 13, 0);
    lines.extend(append(&mut w, 4..5));
    w.close().unwrap();

    let mut w = open(&t);
    assert_eq!(w.sealed_pages().len(), 2, "hours 10 and 11 sealed apart");
    let added = w.flush().unwrap();
    let dirs: Vec<&str> = added
        .iter()
        .map(|e| e.path.rsplit_once('/').unwrap().0)
        .collect();
    assert_eq!(
        dirs,
        [
            "live/date=2026-10-11/hour=10",
            "live/date=2026-10-11/hour=11"
        ]
    );
    assert_eq!(all_rows(&w), sorted(lines));
}

#[test]
fn files_split_at_the_size_limit() {
    let _serial = serial();
    let t = TempTable::new();
    set_clock(0, 10, 0);
    let options = options(TierOptions {
        max_file_size: 1,
        ..tier()
    });
    let mut w = TableWriter::open(&t.0, options.clone()).unwrap();
    let mut lines = append(&mut w, 0..7);
    set_clock(0, 11, 0);
    lines.extend(append(&mut w, 7..8));
    let added = w.flush().unwrap();
    let ranges: Vec<(u64, u64)> = added.iter().map(|e| (e.lsn_lo, e.lsn_hi)).collect();
    assert_eq!(ranges, [(1, 7)], "an hour is one file, whatever its size");
    set_clock(1, 0, 0);
    w.flush().unwrap();
    let merged = w.compact().unwrap();
    let ranges: Vec<(u64, u64)> = merged.iter().map(|e| (e.lsn_lo, e.lsn_hi)).collect();
    assert_eq!(
        ranges,
        [(1, 7), (8, 8)],
        "a new file at each hour once over the limit"
    );
    w.manifest().check().unwrap();
    assert_eq!(all_rows(&w), sorted(lines));
}

#[test]
fn a_crash_at_every_flush_and_compaction_step_recovers_every_record_once() {
    let _serial = serial();
    for step in [1u8, 2, 3, 5, 11, 12, 13] {
        let t = TempTable::new();
        set_clock(0, 10, 0);
        let mut w = open(&t);
        let mut lines = append(&mut w, 0..4);
        set_clock(0, 11, 0);
        lines.extend(append(&mut w, 4..6));
        if step >= 11 {
            set_clock(1, 0, 0);
            lines.extend(append(&mut w, 6..7));
            w.flush().unwrap();
        }
        w.crash_after = Some(step);
        let err = if step >= 11 {
            w.compact().unwrap_err()
        } else {
            w.flush().unwrap_err()
        };
        assert!(
            err.to_string()
                .contains(&format!("after flush step {step}")),
            "{err}"
        );
        drop(w);

        let mut w = open(&t);
        assert_eq!(all_rows(&w), sorted(lines.clone()), "step {step}");
        assert_eq!(files_on_disk(&t), listed(&w), "step {step}: strays left");
        assert!(!t.0.join(TMP_DIR).exists());
        w.manifest().check().unwrap();
        // And the table carries on.
        set_clock(2, 0, 0);
        lines.extend(append(&mut w, 10..11));
        w.flush().unwrap();
        w.compact().unwrap();
        assert_eq!(all_rows(&w), sorted(lines), "step {step}");
        assert!(w.manifest().files.iter().any(|f| f.area == Area::Archive));
    }
}

#[test]
fn layout_rules() {
    let _serial = serial();
    let t = TempTable::new();
    set_clock(0, 10, 0);
    let mut w = open(&t);
    let err = w.append(b"{}", Ack::Flushed).unwrap_err();
    assert!(matches!(err, IngestError::Unsupported(_)), "{err}");
    assert!(err.to_string().starts_with("unsupported: Ack::Flushed"));
    drop(w);
    let err = TableWriter::open(&t.0, TableOptions::default()).unwrap_err();
    assert_eq!(err.to_string(), "the table's layout is tiered, not flat");

    // A flat table has nothing to compact.
    let flat = TempTable::new();
    let mut w = TableWriter::open(&flat.0, TableOptions::default()).unwrap();
    w.append(b"{}", Ack::Durable).unwrap();
    w.flush_all().unwrap();
    assert!(w.compact().unwrap().is_empty());
    assert_eq!(w.manifest().files[0].area, Area::Data);
}

#[test]
fn recovery_removes_strays_in_every_area() {
    let _serial = serial();
    let t = TempTable::new();
    set_clock(0, 10, 0);
    let mut w = open(&t);
    append(&mut w, 0..2);
    set_clock(0, 11, 0);
    append(&mut w, 2..3);
    w.flush().unwrap();
    drop(w);
    for stray in [
        "live/date=2026-10-11/hour=10/stray.parquet",
        "live/date=2026-10-12/hour=00/x.parquet",
        "archive/date=2026-10-10/y.parquet",
        "data/z.parquet",
    ] {
        let path = t.0.join(stray);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }
    let w = open(&t);
    assert_eq!(files_on_disk(&t), listed(&w));
    assert!(!t.0.join("archive").join("date=2026-10-10").exists());
    assert!(!t.0.join("live/date=2026-10-12").exists());
}

// --- MC/DC ----------------------------------------------------------------

/// Appends one record in hour 10 and one in `second_hour` on a table of
/// `layout`; returns the sealed page count.
fn sealed_after_two_records(layout: Layout, second_hour: i64) -> usize {
    let t = TempTable::new();
    set_clock(0, 10, 0);
    let mut w = TableWriter::open(
        &t.0,
        TableOptions {
            layout,
            clock,
            ..TableOptions::default()
        },
    )
    .unwrap();
    w.append(b"{}", Ack::Memory).unwrap();
    set_clock(0, second_hour, 0);
    w.append(b"{}", Ack::Memory).unwrap();
    w.sealed_pages().len()
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_push_bc95699f__v1_tiered_head_of_an_earlier_hour_is_sealed() {
    let _serial = serial();
    assert_eq!(sealed_after_two_records(Layout::Tiered(tier()), 11), 1);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_push_bc95699f__v2_tiered_same_hour_is_not_sealed() {
    let _serial = serial();
    assert_eq!(sealed_after_two_records(Layout::Tiered(tier()), 10), 0);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_push_bc95699f__v3_flat_ignores_hours() {
    let _serial = serial();
    assert_eq!(sealed_after_two_records(Layout::Flat, 11), 0);
}

fn append_with(layout: Layout, ack: Ack) -> Result<u64, IngestError> {
    let t = TempTable::new();
    set_clock(0, 10, 0);
    let options = TableOptions {
        layout,
        clock,
        ..TableOptions::default()
    };
    TableWriter::open(&t.0, options).unwrap().append(b"{}", ack)
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_append_record_85ef2f0f__v1_flushed_on_tiered_is_refused() {
    let _serial = serial();
    assert!(matches!(
        append_with(Layout::Tiered(tier()), Ack::Flushed),
        Err(IngestError::Unsupported(_))
    ));
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_append_record_85ef2f0f__v2_durable_on_tiered_is_accepted() {
    let _serial = serial();
    assert_eq!(
        append_with(Layout::Tiered(tier()), Ack::Durable).unwrap(),
        1
    );
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_append_record_85ef2f0f__v3_flushed_on_flat_is_accepted() {
    let _serial = serial();
    assert_eq!(append_with(Layout::Flat, Ack::Flushed).unwrap(), 1);
}

/// A table directory with `area/a/b/file`, the file already deleted.
fn nested(t: &TempTable) -> PathBuf {
    let file = t.0.join("live/a/b/file");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    file
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_remove_empty_parents_58e0bb1d__v1_empty_parents_are_removed() {
    let t = TempTable::new();
    let file = nested(&t);
    remove_empty_parents(&t.0, &file);
    assert!(!t.0.join("live/a").exists());
    assert!(t.0.join("live").exists(), "the area directory stays");
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_remove_empty_parents_58e0bb1d__v2_the_table_itself_stays() {
    let t = TempTable::new();
    fs::create_dir_all(&t.0).unwrap();
    remove_empty_parents(&t.0, &t.0.join("file"));
    assert!(t.0.exists());
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_remove_empty_parents_58e0bb1d__v3_the_area_directory_stays() {
    let t = TempTable::new();
    fs::create_dir_all(t.0.join("live")).unwrap();
    remove_empty_parents(&t.0, &t.0.join("live/file"));
    assert!(t.0.join("live").exists());
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_table_remove_empty_parents_58e0bb1d__v4_a_non_empty_directory_stays() {
    let t = TempTable::new();
    let file = nested(&t);
    fs::write(file.parent().unwrap().join("other"), b"x").unwrap();
    remove_empty_parents(&t.0, &file);
    assert!(t.0.join("live/a/b/other").exists());
}
