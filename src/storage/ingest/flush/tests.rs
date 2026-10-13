use super::*;
use crate::storage::ingest::page::HeadPage;
use crate::storage::parquet::ParquetFile;
use crate::storage::stream::detect::{DetectedParser, Format};
use crate::storage::stream::{Source, SourceKind};

/// A page of `lines` from LSN `lsn_lo`, observed at `100 + lsn`.
fn page(lsn_lo: u64, lines: &[&str]) -> Page {
    let mut head = HeadPage::default();
    for (i, line) in lines.iter().enumerate() {
        let lsn = lsn_lo + i as u64;
        head.push(lsn, 100 + lsn as i64, line.as_bytes());
    }
    let source = Source::new(SourceKind::Stdin, "t");
    head.snapshot(&source, &DetectedParser::for_format(Format::Jsonl))
        .unwrap()
}

fn write(pages: &[Page]) -> (Vec<u8>, FlushedFile) {
    // Unique per call: tests run in parallel.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("db-core-flush-{}-{n}.parquet", std::process::id()));
    let flushed = write_file(&path, pages, Format::Jsonl, &WriterOptions::default()).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    (bytes, flushed)
}

fn column_index(file: &ParquetFile, name: &str) -> usize {
    file.metadata().schema[1..]
        .iter()
        .position(|e| e.name == name)
        .unwrap_or_else(|| panic!("no column {name}"))
}

#[test]
fn pages_flush_with_predefined_columns_and_the_field_union() {
    let pages = [
        page(
            1,
            &[r#"{"level":"info","code":200,"msg":"ok"}"#, "not json"],
        ),
        page(3, &[r#"{"user":"ann","code":404}"#]),
    ];
    let (bytes, flushed) = write(&pages);
    assert_eq!(flushed.rows, 3);
    assert_eq!(flushed.bytes, bytes.len() as u64);
    assert_eq!(flushed.observed_ts, Some((101, 103)));
    let file = ParquetFile::open(&bytes).unwrap();
    let names: Vec<&str> = file.metadata().schema[1..]
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        &names[..7],
        &[
            "timestamp",
            "observed_ts",
            "severity",
            "severity_text",
            "facility",
            "message",
            "raw"
        ]
    );
    assert!(
        names.contains(&"code") && names.contains(&"user"),
        "{names:?}"
    );
    let rg = file.row_group(0).unwrap();
    let raw = rg.read_string_column(column_index(&file, "raw")).unwrap();
    assert_eq!(raw[1].as_deref(), Some("not json"));
    let user = rg.read_string_column(column_index(&file, "user")).unwrap();
    assert_eq!(user, vec![None, None, Some("ann".to_string())]);
    let code = rg.read_int64_column(column_index(&file, "code")).unwrap();
    assert_eq!(code, vec![Some(200), None, Some(404)]);
    let observed = rg
        .read_int64_column(column_index(&file, "observed_ts"))
        .unwrap();
    assert_eq!(observed, vec![Some(101), Some(102), Some(103)]);
    let kv = &file.metadata().key_value_metadata;
    assert!(kv.contains(&("db-core.lsn_lo".to_string(), Some("1".to_string()))));
    assert!(kv.contains(&("db-core.lsn_hi".to_string(), Some("3".to_string()))));
}

#[test]
fn conflicting_field_types_widen() {
    let pages = [
        page(1, &[r#"{"n":1,"x":1,"b":true}"#]),
        page(2, &[r#"{"n":2.5,"x":"one","b":false}"#]),
    ];
    let specs = schema(&pages);
    let type_of = |name: &str| specs.iter().find(|s| s.name == name).unwrap().column_type;
    assert_eq!(type_of("n"), ColumnType::Double);
    assert_eq!(type_of("x"), ColumnType::Utf8);
    assert_eq!(type_of("b"), ColumnType::Boolean);
    let (bytes, _) = write(&pages);
    let file = ParquetFile::open(&bytes).unwrap();
    let rg = file.row_group(0).unwrap();
    assert_eq!(
        rg.read_double_column(column_index(&file, "n")).unwrap(),
        vec![Some(1.0), Some(2.5)]
    );
    assert_eq!(
        rg.read_string_column(column_index(&file, "x")).unwrap(),
        vec![Some("1".to_string()), Some("one".to_string())]
    );
    assert_eq!(
        rg.read_boolean_column(column_index(&file, "b")).unwrap(),
        vec![Some(true), Some(false)]
    );
}

/// #584: values of conflicting types in one page are all written, as
/// strings, and integers and floats in one page widen to DOUBLE.
#[test]
fn a_type_conflict_inside_a_page_loses_no_value() {
    let pages = [page(
        1,
        &[
            r#"{"x":5,"z":2}"#,
            r#"{"x":"a","z":2.5}"#,
            r#"{"x":true}"#,
            r#"{"x":0.5}"#,
        ],
    )];
    let specs = schema(&pages);
    let type_of = |name: &str| specs.iter().find(|s| s.name == name).unwrap().column_type;
    assert_eq!(type_of("x"), ColumnType::Utf8);
    assert_eq!(type_of("z"), ColumnType::Double);
    let (bytes, _) = write(&pages);
    let file = ParquetFile::open(&bytes).unwrap();
    let rg = file.row_group(0).unwrap();
    assert_eq!(
        rg.read_string_column(column_index(&file, "x")).unwrap(),
        vec![
            Some("5".to_string()),
            Some("a".to_string()),
            Some("true".to_string()),
            Some("0.5".to_string())
        ]
    );
    assert_eq!(
        rg.read_double_column(column_index(&file, "z")).unwrap(),
        vec![Some(2.0), Some(2.5), None, None]
    );
}

#[test]
fn a_field_named_like_a_predefined_column_is_left_out() {
    let pages = [page(1, &[r#"{"raw":"shadow","facility":"x"}"#])];
    let names: Vec<String> = schema(&pages).into_iter().map(|s| s.name).collect();
    assert_eq!(names.iter().filter(|n| *n == "raw").count(), 1);
    assert_eq!(names.len(), 7, "{names:?}");
}

#[test]
fn no_pages_is_an_error() {
    let path = std::env::temp_dir().join("db-core-flush-none.parquet");
    assert!(write_file(&path, &[], Format::Jsonl, &WriterOptions::default()).is_err());
}

#[test]
fn convert_covers_every_target_type() {
    let values = vec![
        Value::Int(3),
        Value::Float(1.5),
        Value::Null,
        Value::Bool(true),
    ];
    assert_eq!(
        convert(values.clone(), ColumnType::TimestampMicros),
        ColumnValues::TimestampMicros(vec![Some(3), None, None, None])
    );
    assert_eq!(
        convert(values.clone(), ColumnType::Float),
        ColumnValues::Float(vec![None; 4])
    );
    assert_eq!(
        convert(values.clone(), ColumnType::Int32),
        ColumnValues::Int32(vec![Some(3), None, None, None])
    );
    assert_eq!(
        convert(values, ColumnType::Utf8),
        ColumnValues::Utf8(vec![
            Some("3".to_string()),
            Some("1.5".to_string()),
            None,
            Some("true".to_string())
        ])
    );
    assert_eq!(
        widen(ColumnType::Boolean, ColumnType::Int64),
        ColumnType::Utf8
    );
}

#[test]
fn json_strings_are_stored_unescaped_and_plain_lines_as_written() {
    let escaped = r#"{"msg":"a\"b\n","user":"caf\u00e9 \ud83d\ude00","path":"C:\\tmp"}"#;
    let pages = [page(
        1,
        &[escaped, r"plain C:\new line", r#"{"user":"x\qy"}"#],
    )];
    let (bytes, _) = write(&pages);
    let file = ParquetFile::open(&bytes).unwrap();
    let rg = file.row_group(0).unwrap();
    let read = |name: &str| rg.read_string_column(column_index(&file, name)).unwrap();
    assert_eq!(
        read("message"),
        vec![
            Some("a\"b\n".to_string()),
            // Not a JSON object: the whole line, backslashes and all.
            Some(r"plain C:\new line".to_string()),
            None,
        ]
    );
    assert_eq!(
        read("user"),
        vec![
            Some("café 😀".to_string()),
            None,
            // An invalid escape is kept as written.
            Some(r"x\qy".to_string()),
        ]
    );
    assert_eq!(read("path")[0].as_deref(), Some(r"C:\tmp"));
    // `raw` is the line as written.
    assert_eq!(read("raw")[0].as_deref(), Some(escaped));
}

#[test]
fn other_formats_store_strings_as_written() {
    let mut head = HeadPage::default();
    head.push(1, 101, br#"level=info msg="a \"q\"""#);
    let page = head
        .snapshot(
            &Source::new(SourceKind::Stdin, "t"),
            &DetectedParser::for_format(Format::Logfmt),
        )
        .unwrap();
    let path = std::env::temp_dir().join(format!(
        "db-core-flush-{}-logfmt.parquet",
        std::process::id()
    ));
    write_file(&path, &[page], Format::Logfmt, &WriterOptions::default()).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let file = ParquetFile::open(&bytes).unwrap();
    let message = file
        .row_group(0)
        .unwrap()
        .read_string_column(column_index(&file, "message"))
        .unwrap();
    assert_eq!(message, vec![Some(r#"a \"q\""#.to_string())]);
}
