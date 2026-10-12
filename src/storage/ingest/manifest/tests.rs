use super::*;
use crate::storage::ingest::layout::{Area, LayoutKind};

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
