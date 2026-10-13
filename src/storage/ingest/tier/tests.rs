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
