use super::*;

fn spec(name: &str, t: ColumnType, nullable: bool) -> ColumnSpec {
    ColumnSpec::new(name, t, nullable)
}

fn every_type() -> Vec<ColumnType> {
    vec![
        ColumnType::Boolean,
        ColumnType::Int32,
        ColumnType::Int64,
        ColumnType::Float,
        ColumnType::Double,
        ColumnType::Utf8,
        ColumnType::TimestampMicros,
        ColumnType::TimestampNanos,
        ColumnType::Binary,
        ColumnType::UInt8,
        ColumnType::UInt16,
        ColumnType::UInt32,
        ColumnType::UInt64,
        ColumnType::Decimal {
            precision: 38,
            scale: 10,
        },
        ColumnType::FixedLenByteArray(16),
        ColumnType::MapUtf8,
    ]
}

#[test]
fn type_names_round_trip() {
    for t in every_type() {
        assert_eq!(type_from_name(&type_name(t)), Some(t), "{t:?}");
    }
    for bad in [
        "",
        "int",
        "decimal(9)",
        "decimal(a,b)",
        "fixed(x)",
        "fixed(16",
    ] {
        assert_eq!(type_from_name(bad), None, "{bad}");
    }
}

#[test]
fn rows_round_trip_through_the_wal_encoding() {
    let row = vec![
        RecordValue::Null,
        RecordValue::Bool(true),
        RecordValue::Bool(false),
        RecordValue::Int(i64::MIN),
        RecordValue::UInt(u64::MAX),
        RecordValue::Float(-0.5),
        RecordValue::Str("é\u{0}".to_string()),
        RecordValue::Bytes(vec![0xff, 0x00]),
        RecordValue::Bytes(Vec::new()),
        RecordValue::Decimal(-(10i128.pow(38) - 1)),
        RecordValue::Map(Vec::new()),
        RecordValue::Map(vec![
            ("é".to_string(), None),
            ("k".to_string(), Some(String::new())),
        ]),
    ];
    let bytes = encode_row(&row).unwrap();
    assert_eq!(decode_row(&bytes), Some(row));
    assert_eq!(decode_row(&[]), Some(Vec::new()));
    // Truncated, an unknown tag, invalid UTF-8: not a row.
    assert_eq!(decode_row(&bytes[..bytes.len() - 1]), None);
    assert_eq!(decode_row(&[99]), None);
    assert_eq!(decode_row(&[TAG_STR, 1, 0, 0, 0, 0xff]), None);
}

#[test]
fn schemas_are_checked() {
    let ok = [spec("a", ColumnType::Int64, true)];
    assert_eq!(invalid_schema(&ok), None);
    assert!(invalid_schema(&[]).is_some());
    for name in ["", "a\"", "a\\b", "a\nb"] {
        assert!(
            invalid_schema(&[spec(name, ColumnType::Int64, true)]).is_some(),
            "{name:?}"
        );
    }
    let fixed0 = [spec("f", ColumnType::FixedLenByteArray(0), true)];
    assert!(invalid_schema(&fixed0)
        .unwrap()
        .contains("FIXED_LEN_BYTE_ARRAY"));
}

#[test]
fn rows_are_checked_without_conversion() {
    let fits = |t: ColumnType, v: RecordValue| check_row(&[spec("c", t, false)], &[v]).is_none();
    // Each type takes its own value.
    assert!(fits(ColumnType::Boolean, RecordValue::Bool(true)));
    assert!(fits(ColumnType::Int64, RecordValue::Int(-1)));
    assert!(fits(ColumnType::TimestampMicros, RecordValue::Int(5)));
    assert!(fits(ColumnType::Double, RecordValue::Float(0.5)));
    assert!(fits(ColumnType::Utf8, RecordValue::Str(String::new())));
    assert!(fits(ColumnType::Binary, RecordValue::Bytes(vec![1])));
    // ...and not another's.
    assert!(!fits(ColumnType::Int64, RecordValue::Float(1.0)));
    assert!(!fits(ColumnType::Double, RecordValue::Int(1)));
    assert!(!fits(ColumnType::Utf8, RecordValue::Bytes(Vec::new())));
    assert!(!fits(ColumnType::Boolean, RecordValue::Int(1)));
    // Ranges.
    assert!(fits(
        ColumnType::Int32,
        RecordValue::Int(i64::from(i32::MIN))
    ));
    assert!(!fits(
        ColumnType::Int32,
        RecordValue::Int(i64::from(i32::MAX) + 1)
    ));
    assert!(fits(ColumnType::UInt8, RecordValue::Int(255)));
    assert!(fits(ColumnType::UInt8, RecordValue::UInt(255)));
    assert!(!fits(ColumnType::UInt8, RecordValue::UInt(256)));
    assert!(!fits(ColumnType::UInt16, RecordValue::Int(-1)));
    assert!(fits(
        ColumnType::UInt32,
        RecordValue::UInt(u64::from(u32::MAX))
    ));
    assert!(fits(ColumnType::UInt64, RecordValue::UInt(u64::MAX)));
    let d = ColumnType::Decimal {
        precision: 4,
        scale: 2,
    };
    assert!(fits(d, RecordValue::Decimal(-9_999)));
    assert!(!fits(d, RecordValue::Decimal(10_000)));
    assert!(fits(
        ColumnType::FixedLenByteArray(2),
        RecordValue::Bytes(vec![1, 2])
    ));
    assert!(!fits(
        ColumnType::FixedLenByteArray(2),
        RecordValue::Bytes(vec![1])
    ));
    // NULL only where nullable.
    assert!(!fits(ColumnType::Int64, RecordValue::Null));
    assert_eq!(
        check_row(&[spec("c", ColumnType::Int64, true)], &[RecordValue::Null]),
        None
    );
}

#[test]
fn a_page_reads_as_the_engine_decodes_its_flushed_file() {
    let schema: Arc<[ColumnSpec]> = Arc::from(
        vec![
            spec("u", ColumnType::UInt64, true),
            spec(
                "d",
                ColumnType::Decimal {
                    precision: 9,
                    scale: 2,
                },
                true,
            ),
            spec("b", ColumnType::Binary, true),
            spec("f", ColumnType::Float, true),
            spec("u8", ColumnType::UInt8, true),
        ]
        .as_slice(),
    );
    let mut page = RowPage::new(Arc::clone(&schema));
    page.push(
        7,
        vec![
            RecordValue::UInt(5),
            RecordValue::Decimal(-1234),
            RecordValue::Bytes(vec![0xab, 0x01]),
            RecordValue::Float(1.5),
            RecordValue::Int(200),
        ],
    );
    page.push(8, vec![RecordValue::Null; 5]);
    assert_eq!((page.len(), page.is_empty()), (2, false));
    assert_eq!(page.observed_ts_ns(), &[7, 8]);
    let col = |name: &str| page.engine_column(name).unwrap().unwrap();
    assert_eq!(col("u"), [Value::Int(5), Value::Null]);
    assert_eq!(
        col("d"),
        [
            Value::Decimal {
                unscaled: -1234,
                scale: 2
            },
            Value::Null
        ],
        "exact, as the flushed file reads (#632)"
    );
    assert_eq!(col("b"), [Value::Str("ab01".into()), Value::Null]);
    assert_eq!(col("f"), [Value::Float(1.5), Value::Null]);
    assert_eq!(col("u8"), [Value::Int(200), Value::Null]);
    assert!(page.engine_column("nope").is_none());
    assert!(page.cleared().is_empty());

    // A UINT_64 above i64::MAX is an error, as reading the file is.
    let mut big = RowPage::new(Arc::from(
        vec![spec("u", ColumnType::UInt64, false)].as_slice(),
    ));
    big.push(1, vec![RecordValue::UInt(u64::MAX)]);
    let err = big.engine_column("u").unwrap().unwrap_err();
    assert!(err.contains("18446744073709551615"), "{err}");
}

#[test]
fn concat_gather_and_compare() {
    let a = ColumnValues::UInt32(vec![Some(u32::MAX), None]);
    let b = ColumnValues::UInt32(vec![Some(1)]);
    let all = concat(ColumnType::UInt32, &[&a, &b]);
    assert_eq!(
        all,
        ColumnValues::UInt32(vec![Some(u32::MAX), None, Some(1)])
    );
    // NULL first, then unsigned order.
    assert!(compare_rows(&all, 1, 2).is_lt());
    assert!(compare_rows(&all, 2, 0).is_lt());
    assert!(compare_rows(&all, 1, 1).is_eq());
    assert_eq!(
        gather(&all, &[2, 0]),
        ColumnValues::UInt32(vec![Some(1), Some(u32::MAX)])
    );
    let d = ColumnValues::Double(vec![Some(-0.0), Some(f64::NAN), Some(0.0)]);
    assert!(compare_rows(&d, 0, 2).is_lt(), "total order: -0.0 < 0.0");
    assert!(compare_rows(&d, 2, 1).is_lt(), "NaN last");
}

fn entries(e: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    e.iter()
        .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
        .collect()
}

/// #621: a repeated key's values join in order under its first position:
/// ", " (the HTTP list separator), a newline for Set-Cookie (any case);
/// NULLs are left out; an all-NULL key stays NULL.
#[test]
fn repeated_map_keys_are_joined() {
    let row = vec![
        RecordValue::Int(1),
        RecordValue::Map(entries(&[
            ("X-Forwarded-For", Some("10.0.0.1")),
            ("Host", Some("a")),
            ("X-Forwarded-For", None),
            ("X-Forwarded-For", Some("10.0.0.2")),
            ("set-cookie", Some("a=1; Path=/")),
            ("set-cookie", Some("b=2, c")),
            ("n", None),
            ("n", None),
            ("late", None),
            ("late", Some("x")),
            ("x-forwarded-for", Some("other case")),
        ])),
    ];
    assert_eq!(
        join_repeated_keys(row),
        [
            RecordValue::Int(1),
            RecordValue::Map(entries(&[
                ("X-Forwarded-For", Some("10.0.0.1, 10.0.0.2")),
                ("Host", Some("a")),
                ("set-cookie", Some("a=1; Path=/\nb=2, c")),
                ("n", None),
                ("late", Some("x")),
                // Keys compare exactly, as a MAP's do.
                ("x-forwarded-for", Some("other case")),
            ])),
        ]
    );
}

#[test]
fn a_map_column_takes_only_a_map() {
    let fits = |v: RecordValue| check_row(&[spec("h", ColumnType::MapUtf8, true)], &[v]).is_none();
    assert!(fits(RecordValue::Map(Vec::new())));
    assert!(fits(RecordValue::Null));
    assert!(!fits(RecordValue::Str("{}".into())));
    // A map value reads back as the engine's map.
    let mut page = RowPage::new(Arc::from(
        vec![spec("h", ColumnType::MapUtf8, true)].as_slice(),
    ));
    page.push(1, vec![RecordValue::Map(entries(&[("a", None)]))]);
    page.push(2, vec![RecordValue::Null]);
    assert_eq!(
        page.engine_column("h").unwrap().unwrap(),
        [Value::Map(Arc::new(vec![("a".into(), None)])), Value::Null]
    );
    // Truncated map bytes are not a row.
    let bytes = encode_row(&[RecordValue::Map(entries(&[("k", Some("v"))]))]).unwrap();
    assert_eq!(decode_row(&bytes[..bytes.len() - 1]), None);
    let mut bad_value_tag = bytes.clone();
    let tag_at = bytes.len() - 6;
    bad_value_tag[tag_at] = 99;
    assert_eq!(decode_row(&bad_value_tag), None);
}
