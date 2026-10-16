use super::*;

#[test]
fn dates_and_partition_directories() {
    for (day, date) in [
        (0, "1970-01-01"),
        (-1, "1969-12-31"),
        (59, "1970-03-01"),
        (11_016, "2000-02-29"),
        (20_737, "2026-10-11"),
        (-719_468, "0000-03-01"),
    ] {
        assert_eq!(date_string(day), date, "{day}");
    }
    let hour = hour_of(20_737 * DAY_NS + 14 * HOUR_NS + 5);
    assert_eq!(live_dir(hour), "live/date=2026-10-11/hour=14");
    assert_eq!(staging_dir(hour), "staging/date=2026-10-11/hour=14");
    assert_eq!(archive_dir(day_of_hour(hour)), "archive/date=2026-10-11");
    assert_eq!(hour_of(-1), -1, "before the epoch rounds down");
    assert_eq!(live_dir(-1), "live/date=1969-12-31/hour=23");
}

#[test]
fn names_round_trip() {
    for kind in [LayoutKind::Flat, LayoutKind::Tiered] {
        assert_eq!(LayoutKind::from_name(kind.as_str()), Some(kind));
    }
    for area in [Area::Data, Area::Live, Area::Archive, Area::Staging] {
        assert_eq!(Area::from_name(area.as_str()), Some(area));
    }
    assert_eq!(LayoutKind::from_name("x"), None);
    assert_eq!(Area::from_name("x"), None);
    assert_eq!(Layout::default().kind(), LayoutKind::Flat);
    assert_eq!(
        Layout::Tiered(TierOptions::default()).kind(),
        LayoutKind::Tiered
    );
}

#[test]
fn the_sort_order() {
    let ordered = [
        Value::Null,
        Value::Float(f64::NEG_INFINITY),
        Value::Int(-1),
        Value::Float(0.5),
        Value::Int(1),
        Value::Float(f64::NAN),
        Value::Bool(false),
        Value::Bool(true),
        Value::Str("a".into()),
        Value::Str("b".into()),
    ];
    for (i, a) in ordered.iter().enumerate() {
        for (j, b) in ordered.iter().enumerate() {
            assert_eq!(compare(a, b), i.cmp(&j), "{a:?} vs {b:?}");
        }
    }
    assert_eq!(compare(&Value::Int(2), &Value::Float(2.0)), Ordering::Equal);
}
