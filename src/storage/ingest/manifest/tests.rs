use super::*;
use crate::storage::ingest::layout::{Area, LayoutKind, SessionKey};

fn entry(lo: u64, hi: u64) -> FileEntry {
    FileEntry {
        path: format!("data/{lo:020}-{hi:020}.parquet"),
        area: Area::Data,
        lsn_lo: lo,
        lsn_hi: hi,
        rows: hi - lo + 1,
        bytes: 1234,
        observed_ts: Some((i64::MIN, i64::MAX)),
        timestamp: None,
        keys: Vec::new(),
    }
}

fn two_files() -> Manifest {
    Manifest::new(Format::Jsonl, LayoutKind::Flat)
        .with_file(entry(1, 10))
        .with_file(entry(11, 15))
}

#[test]
fn round_trips_through_json() {
    for m in [Manifest::new(Format::Logfmt, LayoutKind::Flat), two_files()] {
        let text = m.to_json();
        assert_eq!(Manifest::parse(&text).unwrap(), m, "{text}");
    }
    assert_eq!(two_files().flushed_lsn, 15);
}

#[test]
fn every_format_has_a_name() {
    for f in [
        Format::Jsonl,
        Format::Logfmt,
        Format::Syslog,
        Format::Clf,
        Format::Freeform,
    ] {
        assert_eq!(format_from_name(f.as_str()), Some(f));
    }
    assert_eq!(format_from_name("csv"), None);
}

#[test]
fn the_invariant_is_enforced() {
    let cases = [
        (
            Manifest::new(Format::Jsonl, LayoutKind::Flat).with_file(entry(2, 5)),
            "starts at LSN 2, expected 1",
        ),
        (
            two_files().with_file(entry(17, 20)),
            "starts at LSN 17, expected 16",
        ),
        (
            {
                let mut m = two_files();
                m.files[1].lsn_lo = 10; // overlaps the first
                m
            },
            "starts at LSN 10, expected 11",
        ),
        (
            {
                let mut m = two_files();
                m.files[0].rows = 3;
                m
            },
            "holds LSNs 1..=10 but 3 rows",
        ),
        (
            {
                let mut m = two_files();
                m.flushed_lsn = 20;
                m
            },
            "files cover LSNs up to 15, flushed_lsn is 20",
        ),
        (
            {
                let mut m = Manifest::new(Format::Jsonl, LayoutKind::Flat);
                m.flushed_lsn = 1;
                m
            },
            "files cover LSNs up to 0, flushed_lsn is 1",
        ),
    ];
    for (m, message) in cases {
        let err = m.check().unwrap_err();
        assert!(err.to_string().contains(message), "{err}");
        // ...and a manifest that breaks it is refused on read.
        assert!(matches!(
            Manifest::parse(&m.to_json()),
            Err(ManifestError::Invariant(_))
        ));
    }
    let mut backwards = two_files();
    backwards.files[0].lsn_hi = 0;
    assert!(backwards.check().is_err());
}

#[test]
fn malformed_text_is_refused() {
    let good = two_files().to_json();
    let cases = [
        "".to_string(),
        "[]".to_string(),
        format!("{good} trailing"),
        good.replace("\"version\":2", "\"version\":3"),
        good.replace("\"version\":2", "\"version\":0"),
        good.replace("\"layout\":\"flat\"", "\"layout\":\"round\""),
        good.replace("\"layout\":\"flat\",", ""),
        good.replace("\"area\":\"data\"", "\"area\":\"attic\""),
        good.replace("\"format\":\"json\"", "\"format\":\"csv\""),
        good.replace("\"format\":\"json\"", "\"format\":7"),
        good.replace("\"flushed_lsn\":15", "\"flushed_lsn\":-1"),
        good.replace("\"flushed_lsn\":15", "\"flushed_lsn\":1.5"),
        good.replace("\"flushed_lsn\":15,", ""),
        good.replace("\"files\":[", "\"files\":{\"a\":["),
        good.replace("\"files\":[", "\"files\":[1,"),
        good.replace("},\n{", "} {"),
        good.replace("]}\n", "] x}\n"),
        good.replace("data/", "data\\\\"),
        good.replace("\"timestamp_min\":null", "\"timestamp_min\":3"),
        good.replace("\"observed_ts_min\"", "\"observed_ts_minimum\""),
    ];
    for text in cases {
        let err = Manifest::parse(&text).err();
        assert!(
            matches!(err, Some(ManifestError::Malformed(_))),
            "{text}: {err:?}"
        );
    }
    let empty_files = Manifest::new(Format::Jsonl, LayoutKind::Flat)
        .to_json()
        .replace("[]", "[ ] x");
    assert!(Manifest::parse(&empty_files).is_err());
}

#[test]
fn write_then_read_and_missing_is_none() {
    let dir = std::env::temp_dir().join(format!("db-core-manifest-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(Manifest::read(&dir).unwrap(), None);
    let m = two_files();
    m.write(&dir).unwrap();
    assert_eq!(Manifest::read(&dir).unwrap(), Some(m.clone()));
    assert!(!dir.join(MANIFEST_TMP).exists());
    // A stale tmp from a crash mid-write is just ignored.
    fs::write(dir.join(MANIFEST_TMP), b"garbage").unwrap();
    assert_eq!(Manifest::read(&dir).unwrap(), Some(m));
    // An invalid manifest is never written.
    let mut bad = two_files();
    bad.flushed_lsn = 99;
    assert!(matches!(bad.write(&dir), Err(ManifestError::Invariant(_))));
    fs::write(dir.join(MANIFEST_FILE), b"{").unwrap();
    assert!(matches!(
        Manifest::read(&dir),
        Err(ManifestError::Malformed(_))
    ));
    fs::remove_dir_all(&dir).unwrap();
    assert!(matches!(Manifest::read(&dir.join("x")), Ok(None)));
}

#[test]
fn error_messages() {
    assert_eq!(
        ManifestError::from(io::Error::other("disk")).to_string(),
        "manifest: disk"
    );
    assert_eq!(malformed("x").to_string(), "manifest: malformed: x");
}

#[test]
fn version_1_reads_as_a_flat_table() {
    let v2 = two_files().to_json();
    let v1 = v2
        .replace("\"version\":2", "\"version\":1")
        .replace("\"layout\":\"flat\",", "")
        .replace("\"area\":\"data\",", "");
    assert!(!v1.contains("area"), "{v1}");
    assert_eq!(Manifest::parse(&v1).unwrap(), two_files());
}

fn tiered(areas: &[Area]) -> Manifest {
    let mut m = Manifest::new(Format::Jsonl, LayoutKind::Tiered);
    for (i, area) in areas.iter().enumerate() {
        let lsn = i as u64 + 1;
        let mut e = entry(lsn, lsn);
        e.area = *area;
        m = m.with_file(e);
    }
    m
}

#[test]
fn areas_must_fit_the_layout() {
    tiered(&[Area::Archive, Area::Archive, Area::Live, Area::Live])
        .check()
        .unwrap();
    tiered(&[Area::Archive, Area::Live, Area::Staging, Area::Staging])
        .check()
        .unwrap();
    for (m, message) in [
        (
            tiered(&[Area::Live, Area::Archive]),
            "out of place in a tiered table",
        ),
        (
            tiered(&[Area::Data]),
            "data/, out of place in a tiered table",
        ),
        (
            tiered(&[Area::Staging, Area::Live]),
            "live/, out of place in a tiered table",
        ),
        (
            tiered(&[Area::Live, Area::Staging, Area::Archive]),
            "archive/, out of place in a tiered table",
        ),
        (
            {
                let mut m = two_files();
                m.files[1].area = Area::Live;
                m
            },
            "out of place in a flat table",
        ),
    ] {
        let err = m.check().unwrap_err();
        assert!(err.to_string().contains(message), "{err}");
    }
    let m = tiered(&[Area::Live, Area::Live, Area::Live]);
    let mut merged = entry(1, 2);
    merged.area = Area::Archive;
    let next = m.replacing(0..2, vec![merged]);
    next.check().unwrap();
    assert_eq!(next.files.len(), 2);
    assert_eq!(next.flushed_lsn, 3);
    assert_eq!(Manifest::parse(&next.to_json()).unwrap(), next);
}

#[test]
fn retention_drops_leading_files_and_records_where_it_stopped() {
    let m = tiered(&[Area::Archive, Area::Archive, Area::Live]);
    let next = m.dropping(2);
    assert_eq!((next.dropped_lsn, next.flushed_lsn), (2, 3));
    assert_eq!(next.files.len(), 1);
    next.check().unwrap();
    let text = next.to_json();
    assert!(text.contains("\"dropped_lsn\":2,"), "{text}");
    assert_eq!(Manifest::parse(&text).unwrap(), next);
    // Nothing dropped: the field is not written, as before retention.
    assert!(!m.to_json().contains("dropped_lsn"));
    assert_eq!(m.dropping(0), m);

    // Every file dropped: the table is empty up to flushed_lsn.
    let all = m.dropping(3);
    assert_eq!((all.dropped_lsn, all.flushed_lsn), (3, 3));
    all.check().unwrap();
    assert_eq!(Manifest::parse(&all.to_json()).unwrap(), all);

    for (bad, message) in [
        (
            {
                let mut b = next.clone();
                b.dropped_lsn = 1;
                b
            },
            "starts at LSN 3, expected 2",
        ),
        (
            {
                let mut b = all.clone();
                b.dropped_lsn = 4;
                b
            },
            "dropped_lsn 4 is past flushed_lsn 3",
        ),
    ] {
        let err = bad.check().unwrap_err();
        assert!(err.to_string().contains(message), "{err}");
    }
}

/// #656 review: objects inside arrays inside objects, nested deep, make a
/// manifest malformed -- not a stack overflow (the parser scans each
/// array flat and parses its objects afresh).
#[test]
fn deeply_nested_arrays_are_malformed_not_a_stack_overflow() {
    let n = 50_000;
    let deep = format!("{}{}", "{\"a\":[".repeat(n), "]}".repeat(n));
    assert!(Manifest::parse(&deep).is_err());
}

/// `two_files` in a tiered table whose files carry key ranges.
fn with_keys() -> Manifest {
    let mut m = two_files();
    m.layout = LayoutKind::Tiered;
    for (i, file) in m.files.iter_mut().enumerate() {
        file.area = Area::Live;
        file.keys = vec![
            KeyRange {
                column: "kind".to_string(),
                min: KeyBound::Str(format!("a{i}")),
                max: KeyBound::Str("zé".to_string()),
            },
            KeyRange {
                column: "n".to_string(),
                min: KeyBound::Int(i64::MIN),
                max: KeyBound::Int(i64::MAX),
            },
        ];
    }
    m
}

#[test]
fn key_ranges_round_trip_as_version_4() {
    let m = with_keys();
    let text = m.to_json();
    assert!(text.contains("\"version\":4"), "{text}");
    assert!(!text.contains("schema"), "{text}");
    assert_eq!(Manifest::parse(&text).unwrap(), m);
    // A record table's version 4 keeps its schema.
    let mut records = with_keys();
    records.schema = Some(vec![ColumnSpec::new(
        "kind",
        crate::storage::parquet::schema::ColumnType::Utf8,
        true,
    )]);
    let text = records.to_json();
    assert!(text.contains("\"version\":4"), "{text}");
    assert_eq!(Manifest::parse(&text).unwrap(), records);
    // Without key ranges the versions stay as before.
    assert!(two_files().to_json().contains("\"version\":2"));
}

#[test]
fn an_evolved_schema_is_version_5_so_older_readers_refuse_it() {
    use crate::storage::parquet::schema::ColumnType;
    // A reader that stops at the key-range version (4) refuses what it
    // does not know, which is how an older binary refuses this table.
    const { assert!(EVOLVED_VERSION > KEYS_VERSION) };
    let schema = vec![
        ColumnSpec::new("kind", ColumnType::Utf8, true),
        ColumnSpec::new("note", ColumnType::Utf8, true),
    ];
    for mut m in [two_files(), with_keys()] {
        m.schema = Some(schema.clone());
        m.evolved = true;
        let text = m.to_json();
        assert!(text.contains("\"version\":5"), "{text}");
        assert_eq!(Manifest::parse(&text).unwrap(), m);
        // Without the flag the same manifest is written as before.
        m.evolved = false;
        assert!(!m.to_json().contains("\"version\":5"));
    }
    let future = two_files()
        .to_json()
        .replace("\"version\":2", "\"version\":7");
    assert!(Manifest::parse(&future).is_err());
}

#[test]
fn a_session_key_is_version_6_so_older_readers_refuse_it() {
    use crate::storage::parquet::schema::ColumnType;
    const { assert!(SESSION_VERSION > EVOLVED_VERSION) };
    let mut m = with_keys();
    m.schema = Some(vec![ColumnSpec::new("sid", ColumnType::UInt64, false)]);
    m.session_key = Some(SessionKey {
        column: "sid".to_string(),
        max_session_secs: 1_800,
    });
    // `evolved` survives, though the version no longer says it.
    for evolved in [false, true] {
        m.evolved = evolved;
        let text = m.to_json();
        assert!(text.contains("\"version\":6"), "{text}");
        assert!(text.contains("\"session_key\":{\"column\":\"sid\",\"max_session_secs\":1800}"));
        assert_eq!(Manifest::parse(&text).unwrap(), m);
    }
    // Without one the version is as before.
    m.session_key = None;
    m.evolved = false;
    assert!(m.to_json().contains("\"version\":4"));
    let broken = |from: &str, to: &str| {
        let mut m = m.clone();
        m.session_key = Some(SessionKey {
            column: "sid".to_string(),
            max_session_secs: 1,
        });
        Manifest::parse(&m.to_json().replace(from, to))
    };
    assert!(broken("\"max_session_secs\":1", "\"max_session_secs\":4294967296").is_err());
    assert!(broken("\"max_session_secs\":1", "\"max_session_secs\":-1").is_err());
    assert!(broken("\"session_key\":{", "\"session_key\":[{").is_err());
}

#[test]
fn a_string_bound_that_needs_escaping_is_left_out() {
    let mut m = with_keys();
    m.files[0].keys[0].max = KeyBound::Str("a\"b".to_string());
    m.files[1].keys[0].min = KeyBound::Str("a\\b".to_string());
    m.files[1].keys[1].max = KeyBound::Str("tab\there".to_string());
    let read = Manifest::parse(&m.to_json()).unwrap();
    let columns = |f: &FileEntry| f.keys.iter().map(|k| k.column.clone()).collect::<Vec<_>>();
    assert_eq!(columns(&read.files[0]), ["n"]);
    assert!(read.files[1].keys.is_empty());
    // A file with no writable range leaves the version as it was.
    let mut none = two_files();
    none.files[0].keys = vec![KeyRange {
        column: "k".to_string(),
        min: KeyBound::Str("\n".to_string()),
        max: KeyBound::Str("x".to_string()),
    }];
    assert!(none.to_json().contains("\"version\":2"));
}

#[test]
fn malformed_key_ranges_are_refused_and_old_versions_ignore_them() {
    let good = with_keys().to_json();
    for text in [
        good.replace("\"min\":\"a0\"", "\"min\":5"),
        good.replace("\"min\":\"a0\"", "\"min\":null"),
        good.replace("\"keys\":[", "\"keys\":{\"x\":["),
        good.replace("\"column\":\"kind\",", ""),
    ] {
        assert!(
            matches!(Manifest::parse(&text), Err(ManifestError::Malformed(_))),
            "{text}"
        );
    }
    // Version 2 never had key ranges: a `keys` field is not read.
    let v2 = good.replace("\"version\":4", "\"version\":2");
    let read = Manifest::parse(&v2).unwrap();
    assert!(read.files.iter().all(|f| f.keys.is_empty()));
}

// storage_ingest_manifest_key_ranges_7319c889:
// `version < KEYS_VERSION || !fields.iter().any(|(k, _)| k == "keys")`
#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_manifest_key_ranges_7319c889__v1_an_old_version_reads_no_keys() {
    let text = with_keys()
        .to_json()
        .replace("\"version\":4", "\"version\":2");
    assert!(Manifest::parse(&text).unwrap().files[0].keys.is_empty());
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_manifest_key_ranges_7319c889__v2_a_file_without_keys_has_none() {
    let mut m = with_keys();
    m.files[1].keys.clear();
    assert!(Manifest::parse(&m.to_json()).unwrap().files[1]
        .keys
        .is_empty());
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_manifest_key_ranges_7319c889__v3_a_version_4_file_reads_its_keys() {
    let m = with_keys();
    assert_eq!(
        Manifest::parse(&m.to_json()).unwrap().files[0].keys.len(),
        2
    );
}

// storage_ingest_manifest_key_ranges_581bc309: `!lo.contains('\\') && !hi.contains('\\')`
#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_manifest_key_ranges_581bc309__v1_plain_bounds_read() {
    let m = with_keys();
    assert_eq!(Manifest::parse(&m.to_json()).unwrap(), m);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_manifest_key_ranges_581bc309__v2_an_escaped_min_is_refused() {
    let text = with_keys()
        .to_json()
        .replace("\"min\":\"a0\"", "\"min\":\"a\\\\0\"");
    assert!(matches!(
        Manifest::parse(&text),
        Err(ManifestError::Malformed(_))
    ));
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_manifest_key_ranges_581bc309__v3_an_escaped_max_is_refused() {
    let text = with_keys()
        .to_json()
        .replace("\"max\":\"zé\"", "\"max\":\"z\\\\é\"");
    assert!(matches!(
        Manifest::parse(&text),
        Err(ManifestError::Malformed(_))
    ));
}
