use super::*;
use crate::storage::ingest::layout::Area;
use crate::storage::parquet::writer::ColumnType;
use crate::storage::stream::detect::{DetectedParser, Format};
use crate::storage::stream::{Source, SourceKind};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("db-core-tier-{}-{name}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn page(lsn_lo: u64, lines: &[&str]) -> Page {
    let mut head = super::super::page::HeadPage::default();
    for (i, line) in lines.iter().enumerate() {
        head.push(lsn_lo + i as u64, 100 + i as i64, line.as_bytes());
    }
    head.snapshot(
        &Source::new(SourceKind::Stdin, "t"),
        &DetectedParser::for_format(Format::Jsonl),
    )
    .unwrap()
}

#[test]
fn bounds_widen() {
    assert_eq!(widen_bounds(None, None), None);
    assert_eq!(widen_bounds(Some((1, 2)), None), Some((1, 2)));
    assert_eq!(widen_bounds(None, Some((3, 4))), Some((3, 4)));
    assert_eq!(widen_bounds(Some((1, 5)), Some((0, 3))), Some((0, 5)));
}

#[test]
fn sorting_is_stable_by_each_key_in_turn() {
    let specs = vec![
        ColumnSpec::new("k", ColumnType::Utf8, true),
        ColumnSpec::new("n", ColumnType::Int64, true),
    ];
    let s = |x: &str| Value::Str(x.to_string().into());
    let columns = vec![
        vec![s("b"), s("a"), s("b"), Value::Null],
        vec![Value::Int(2), Value::Int(9), Value::Int(1), Value::Int(5)],
    ];
    let by_k = sorted(columns.clone(), &specs, &["k".to_string()]);
    assert_eq!(by_k[0], vec![Value::Null, s("a"), s("b"), s("b")]);
    assert_eq!(
        by_k[1],
        vec![Value::Int(5), Value::Int(9), Value::Int(2), Value::Int(1)]
    );
    let by_k_n = sorted(columns.clone(), &specs, &["k".to_string(), "n".to_string()]);
    assert_eq!(
        by_k_n[1],
        vec![Value::Int(5), Value::Int(9), Value::Int(1), Value::Int(2)]
    );
    // An unknown key, or none, keeps arrival order.
    assert_eq!(
        sorted(columns.clone(), &specs, &["nope".to_string()]),
        columns
    );
    assert_eq!(sorted(columns.clone(), &specs, &[]), columns);
}

#[test]
fn an_hour_without_pages_is_an_error() {
    let dir = temp_dir("empty");
    let err = write_hour(
        &dir,
        &[],
        Format::Jsonl,
        &TierOptions::default(),
        &WriterOptions::default(),
    );
    assert!(err.unwrap_err().to_string().contains("no pages"));
}

#[test]
fn an_hour_is_one_file_sorted_across_pages_then_cut() {
    let dir = temp_dir("span");
    let pages = [
        page(1, &[r#"{"k":"z"}"#, r#"{"k":"y"}"#, r#"{"k":"x"}"#]),
        page(4, &[r#"{"k":"w"}"#, r#"{"n":1}"#]),
    ];
    let tier = TierOptions {
        sort_key: vec!["k".to_string()],
        row_group_max_rows: 2,
        max_file_size: 1,
        ..TierOptions::default()
    };
    let outputs = write_hour(
        &dir,
        &pages,
        Format::Jsonl,
        &tier,
        &WriterOptions::default(),
    )
    .unwrap();
    assert_eq!(outputs.len(), 1);
    let o = &outputs[0];
    assert_eq!((o.lsn_lo, o.lsn_hi, o.file.rows), (1, 5, 5));
    assert_eq!(o.file.observed_ts, Some((100, 102)));
    let bytes = fs::read(dir.join(&o.name)).unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let k = file.metadata().schema[1..]
        .iter()
        .position(|e| e.name == "k")
        .unwrap();
    let groups: Vec<Vec<Option<String>>> = file
        .row_groups()
        .map(|rg| rg.read_string_column(k).unwrap())
        .collect();
    let s = |x: &str| Some(x.to_string());
    assert_eq!(
        groups,
        [vec![None, s("w")], vec![s("x"), s("y")], vec![s("z")]]
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_day_merge_refuses_inputs_that_do_not_match_the_manifest() {
    let dir = temp_dir("hour");
    let pages = [page(1, &[r#"{"a":1}"#, r#"{"a":2}"#])];
    let outputs = write_hour(
        &dir,
        &pages,
        Format::Jsonl,
        &TierOptions::default(),
        &WriterOptions::default(),
    )
    .unwrap();
    let bytes = fs::read(dir.join(&outputs[0].name)).unwrap();
    let entry = FileEntry {
        path: "live/x.parquet".to_string(),
        area: Area::Live,
        lsn_lo: 1,
        lsn_hi: 3, // one more than the file holds
        rows: 3,
        bytes: 0,
        observed_ts: None,
        timestamp: None,
    };
    let out = temp_dir("day");
    let err = merge_day(
        &out,
        &[(&entry, &bytes)],
        None,
        &TierOptions::default(),
        &WriterOptions::default(),
    );
    assert!(err
        .unwrap_err()
        .to_string()
        .contains("holds LSNs up to 2, the manifest says 3"));
    let err = merge_day(
        &out,
        &[(&entry, b"not parquet")],
        None,
        &TierOptions::default(),
        &WriterOptions::default(),
    );
    assert!(err
        .unwrap_err()
        .to_string()
        .starts_with("table: live/x.parquet"));
    fs::remove_dir_all(&dir).unwrap();
    fs::remove_dir_all(&out).unwrap();
}

#[test]
fn every_physical_type_reads_back() {
    use crate::storage::parquet::writer::{ColumnValues, ParquetWriter};
    let specs = vec![
        ColumnSpec::new("i32", ColumnType::Int32, true),
        ColumnSpec::new("f32", ColumnType::Float, true),
        ColumnSpec::new("b", ColumnType::Boolean, true),
    ];
    let mut w = ParquetWriter::new(Vec::new(), specs, WriterOptions::default()).unwrap();
    w.write_columns(vec![
        ColumnValues::Int32(vec![Some(7), None]),
        ColumnValues::Float(vec![Some(1.5), None]),
        ColumnValues::Boolean(vec![None, Some(true)]),
    ])
    .unwrap();
    let (bytes, _) = w.finish().unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let rg = file.row_group(0).unwrap();
    assert_eq!(
        read_values(&rg, 0, PhysicalType::Int32).unwrap(),
        [Value::Int(7), Value::Null]
    );
    assert_eq!(
        read_values(&rg, 1, PhysicalType::Float).unwrap(),
        [Value::Float(1.5), Value::Null]
    );
    assert_eq!(
        read_values(&rg, 2, PhysicalType::Boolean).unwrap(),
        [Value::Null, Value::Bool(true)]
    );
    assert!(read_values(&rg, 0, PhysicalType::Int96).is_err());
    assert!(
        read_values(&rg, 0, PhysicalType::ByteArray).is_err(),
        "wrong type for the chunk"
    );
}

/// `outputs` (one file in `dir`) as the manifest entry of a staging run.
fn run_entry(dir: &Path, outputs: &[Output]) -> (FileEntry, Vec<u8>) {
    let o = &outputs[0];
    let entry = FileEntry {
        path: o.name.clone(),
        area: Area::Staging,
        lsn_lo: o.lsn_lo,
        lsn_hi: o.lsn_hi,
        rows: o.file.rows,
        bytes: o.file.bytes,
        observed_ts: o.file.observed_ts,
        timestamp: o.file.timestamp,
    };
    (entry, fs::read(dir.join(&o.name)).unwrap())
}

/// Column `name` of every row group of `file`, as strings.
fn string_groups(file: &ParquetFile<'_>, name: &str) -> Vec<Vec<Option<String>>> {
    let index = file.metadata().schema[1..]
        .iter()
        .position(|e| e.name == name)
        .unwrap();
    file.row_groups()
        .map(|rg| rg.read_string_column(index).unwrap())
        .collect()
}

#[test]
fn an_hour_closes_by_merging_its_runs_and_pages_under_a_widened_schema() {
    let dir = temp_dir("close");
    let run_tier = TierOptions {
        sort_key: vec!["k".to_string()],
        row_group_max_rows: 1,
        ..TierOptions::default()
    };
    let parquet = WriterOptions::default();
    let write_run = |name: &str, pages: &[Page]| {
        let run_dir = dir.join(name);
        fs::create_dir_all(&run_dir).unwrap();
        let outputs = write_hour(&run_dir, pages, Format::Jsonl, &run_tier, &parquet).unwrap();
        run_entry(&run_dir, &outputs)
    };
    // `n` is an integer in the first run and a string in the second: the
    // merge widens it. `extra` is only in the pages.
    let first = write_run(
        "r1",
        &[page(
            1,
            &[r#"{"k":"m","n":1}"#, r#"{"k":"c"}"#, r#"{"k":"x"}"#],
        )],
    );
    let second = write_run("r2", &[page(4, &[r#"{"k":"b","n":"t"}"#, r#"{"k":"y"}"#])]);
    let pages = [page(6, &[r#"{"k":"a","extra":true}"#, r#"{"k":"n"}"#])];
    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    let tier = TierOptions {
        row_group_max_rows: 3,
        ..run_tier.clone()
    };
    let outputs = close_hour(
        &out,
        &[(&first.0, &first.1), (&second.0, &second.1)],
        &pages,
        Format::Jsonl,
        None,
        &tier,
        &parquet,
    )
    .unwrap();
    assert_eq!(outputs.len(), 1);
    let o = &outputs[0];
    assert_eq!((o.lsn_lo, o.lsn_hi, o.file.rows), (1, 7, 7));
    assert_eq!(o.file.observed_ts, Some((100, 102)));
    let bytes = fs::read(out.join(&o.name)).unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let s = |x: &str| Some(x.to_string());
    assert_eq!(
        string_groups(&file, "k"),
        [
            vec![s("a"), s("b"), s("c")],
            vec![s("m"), s("n"), s("x")],
            vec![s("y")]
        ]
    );
    assert_eq!(
        string_groups(&file, "n"),
        [
            vec![None, s("t"), None],
            vec![s("1"), None, None],
            vec![None]
        ]
    );
    assert!(file.metadata().schema.iter().any(|e| e.name == "extra"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_hour_without_runs_closes_like_write_hour() {
    let dir = temp_dir("close-plain");
    let pages = [page(1, &[r#"{"k":"b"}"#, r#"{"k":"a"}"#])];
    let tier = TierOptions {
        sort_key: vec!["k".to_string()],
        ..TierOptions::default()
    };
    let outputs = close_hour(
        &dir,
        &[],
        &pages,
        Format::Jsonl,
        None,
        &tier,
        &WriterOptions::default(),
    )
    .unwrap();
    let bytes = fs::read(dir.join(&outputs[0].name)).unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let s = |x: &str| Some(x.to_string());
    assert_eq!(string_groups(&file, "k"), [vec![s("a"), s("b")]]);
    fs::remove_dir_all(&dir).unwrap();
}

/// `close_hour` over one run of `run_lines` (written in row groups of
/// `run_rows`) and `pages`; returns the merged file's `k`, in order.
fn close_one_run(name: &str, run_lines: &[&str], run_rows: usize, pages: &[Page]) -> Vec<String> {
    let dir = temp_dir(name);
    let tier = TierOptions {
        sort_key: vec!["k".to_string()],
        row_group_max_rows: run_rows,
        ..TierOptions::default()
    };
    let parquet = WriterOptions::default();
    let run_dir = dir.join("run");
    fs::create_dir_all(&run_dir).unwrap();
    let outputs = write_hour(
        &run_dir,
        &[page(1, run_lines)],
        Format::Jsonl,
        &tier,
        &parquet,
    )
    .unwrap();
    let (entry, bytes) = run_entry(&run_dir, &outputs);
    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    let merged = close_hour(
        &out,
        &[(&entry, &bytes)],
        pages,
        Format::Jsonl,
        None,
        &TierOptions {
            row_group_max_rows: 1_000,
            ..tier
        },
        &parquet,
    )
    .unwrap();
    let bytes = fs::read(out.join(&merged[0].name)).unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let k = string_groups(&file, "k")
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    fs::remove_dir_all(&dir).unwrap();
    k
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_tier_close_hour_b2b17113__v1_an_exhausted_row_group_loads_the_next() {
    // Row groups of one row: each pick ends a chunk with another behind it.
    let k = close_one_run("v1", &[r#"{"k":"b"}"#, r#"{"k":"a"}"#], 1, &[]);
    assert_eq!(k, ["a", "b"]);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_tier_close_hour_b2b17113__v2_a_chunk_with_rows_left_is_kept() {
    // One row group of two: the first pick leaves a row in the chunk.
    let k = close_one_run("v2", &[r#"{"k":"b"}"#, r#"{"k":"a"}"#], 2, &[]);
    assert_eq!(k, ["a", "b"]);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_tier_close_hour_b2b17113__v3_an_exhausted_run_without_more_is_done() {
    // The pages' run has no file: once its row is picked it is done, and
    // the run's row follows.
    let pages = [page(2, &[r#"{"k":"a"}"#])];
    let k = close_one_run("v3", &[r#"{"k":"b"}"#], 1, &pages);
    assert_eq!(k, ["a", "b"]);
}
