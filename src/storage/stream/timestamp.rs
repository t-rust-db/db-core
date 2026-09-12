//! Flexible timestamp parsing shared by JSONL and logfmt (#319/#320):
//! ISO 8601 with fractional seconds and zone, or a bare epoch number
//! disambiguated by digit count (s/ms/us/ns). Same simplified epoch/leap-
//! year approximation as `ClfParser`/`SyslogParser` (no leap-second
//! handling, years before 1970 unsupported).

const DAYS_BEFORE_MONTH: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

/// Compose `Y-M-D H:M:S` (+ optional nanosecond fraction, UTC) into
/// nanoseconds since Unix epoch.
#[must_use]
pub fn epoch_nanos(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32, nanos: u32) -> Option<i64> {
    if y < 1970
        || m == 0
        || m > 12
        || d == 0
        || d > 31
        || h > 23
        || min > 59
        || s > 60
        || nanos > 999_999_999
    {
        return None;
    }

    let month_idx = m.saturating_sub(1) as usize;
    let days_before = *DAYS_BEFORE_MONTH.get(month_idx)?;

    let years_since_epoch = y.saturating_sub(1970);
    let leap_years = years_since_epoch / 4;
    #[allow(clippy::cast_sign_loss)]
    let days_from_years = (years_since_epoch as u32)
        .saturating_mul(365)
        .saturating_add(leap_years as u32);

    let total_days = days_from_years
        .saturating_add(days_before)
        .saturating_add(d.saturating_sub(1));

    let total_seconds = i64::from(total_days)
        .saturating_mul(86400)
        .saturating_add(i64::from(h).saturating_mul(3600))
        .saturating_add(i64::from(min).saturating_mul(60))
        .saturating_add(i64::from(s));

    Some(
        total_seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(i64::from(nanos)),
    )
}

/// Parse a `+HHMM`/`-HHMM`/`Z` UTC offset suffix and apply it to a UTC
/// nanosecond timestamp (subtracting the offset to normalize to UTC).
/// Malformed or empty input leaves the timestamp unchanged.
fn apply_zone_offset(ts: i64, zone: &str) -> i64 {
    if zone.is_empty() || zone.eq_ignore_ascii_case("Z") {
        return ts;
    }
    let sign: i64 = if zone.starts_with('-') { -1 } else { 1 };
    let Some(rest) = zone.get(1..) else { return ts };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() < 4 {
        return ts;
    }
    let Some(hh) = digits.get(0..2).and_then(|s| s.parse::<i64>().ok()) else {
        return ts;
    };
    let Some(mm) = digits.get(2..4).and_then(|s| s.parse::<i64>().ok()) else {
        return ts;
    };
    let offset_ns = hh
        .saturating_mul(3600)
        .saturating_add(mm.saturating_mul(60))
        .saturating_mul(1_000_000_000);
    ts.saturating_sub(sign.saturating_mul(offset_ns))
}

/// Parse an ISO 8601 timestamp: `YYYY-MM-DDTHH:MM:SS[.fraction][Z|+HH:MM]`.
/// Also accepts a space instead of `T` (common in log timestamps).
#[must_use]
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    if s.get(4..5)? != "-" {
        return None;
    }
    let month: u32 = s.get(5..7)?.parse().ok()?;
    if s.get(7..8)? != "-" {
        return None;
    }
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let sep = s.get(10..11)?;
    if sep != "T" && sep != "t" && sep != " " {
        return None;
    }
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    if s.get(13..14)? != ":" {
        return None;
    }
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    if s.get(16..17)? != ":" {
        return None;
    }
    let second: u32 = s.get(17..19)?.parse().ok()?;

    let mut rest = s.get(19..).unwrap_or("");
    let mut nanos: u32 = 0;
    if rest.starts_with('.') {
        let after_dot = rest.get(1..).unwrap_or("");
        let frac_end = after_dot
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_dot.len());
        let frac = after_dot.get(..frac_end).unwrap_or("");
        let mut padded = String::with_capacity(9);
        padded.push_str(frac.get(..frac.len().min(9)).unwrap_or(""));
        while padded.len() < 9 {
            padded.push('0');
        }
        nanos = padded.parse().ok()?;
        rest = after_dot.get(frac_end..).unwrap_or("");
    }

    let ts = epoch_nanos(year, month, day, hour, minute, second, nanos)?;
    Some(apply_zone_offset(ts, rest))
}

/// Parse a bare epoch number, disambiguating seconds/milliseconds/
/// microseconds/nanoseconds by digit count (LAB271 rule):
/// 10 digits = s, 13 = ms, 16 = us, 19 = ns.
#[must_use]
pub fn parse_epoch(s: &str) -> Option<i64> {
    let digits = s.strip_prefix('-').unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: i64 = s.parse().ok()?;
    let scale: i64 = match digits.len() {
        10 => 1_000_000_000,
        13 => 1_000_000,
        16 => 1_000,
        19 => 1,
        _ => return None,
    };
    value.checked_mul(scale)
}

/// Parse a flexible timestamp: ISO 8601 first, then a bare epoch number.
#[must_use]
pub fn parse_flexible(s: &str) -> Option<i64> {
    parse_iso8601(s).or_else(|| parse_epoch(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MC/DC vectors (`mcdc__<id>__vN`, joined to tests/mcdc/obligations.json
    // by `make test-mcdc`). ---------------------------------------------------

    // storage_stream_timestamp_epoch_nanos_104c1cd5 (`epoch_nanos`): the
    // nine-leaf `||` range guard. v1 is the all-false vector (every field in
    // range); v2..v10 each flip exactly one leaf to true, in leaf order.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v1_all_in_range() {
        assert!(epoch_nanos(2026, 9, 10, 8, 0, 5, 0).is_some());
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v2_year_before_epoch() {
        assert_eq!(epoch_nanos(1969, 9, 10, 8, 0, 5, 0), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v3_month_zero() {
        assert_eq!(epoch_nanos(2026, 0, 10, 8, 0, 5, 0), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v4_month_thirteen() {
        assert_eq!(epoch_nanos(2026, 13, 10, 8, 0, 5, 0), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v5_day_zero() {
        assert_eq!(epoch_nanos(2026, 9, 0, 8, 0, 5, 0), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v6_day_thirty_two() {
        assert_eq!(epoch_nanos(2026, 9, 32, 8, 0, 5, 0), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v7_hour_twenty_four() {
        assert_eq!(epoch_nanos(2026, 9, 10, 24, 0, 5, 0), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v8_minute_sixty() {
        assert_eq!(epoch_nanos(2026, 9, 10, 8, 60, 5, 0), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v9_second_sixty_one() {
        // 60 is allowed (leap second), 61 is not.
        assert!(epoch_nanos(2026, 9, 10, 8, 0, 60, 0).is_some());
        assert_eq!(epoch_nanos(2026, 9, 10, 8, 0, 61, 0), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_epoch_nanos_104c1cd5__v10_nanos_overflow_second() {
        assert_eq!(epoch_nanos(2026, 9, 10, 8, 0, 5, 1_000_000_000), None);
    }

    // storage_stream_timestamp_apply_zone_offset_6d98e87c (`apply_zone_offset`):
    // `zone.is_empty() || zone.eq_ignore_ascii_case("Z")`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_apply_zone_offset_6d98e87c__v1_empty_zone_is_utc() {
        assert_eq!(apply_zone_offset(1_000, ""), 1_000);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_apply_zone_offset_6d98e87c__v2_z_zone_is_utc() {
        assert_eq!(apply_zone_offset(1_000, "Z"), 1_000);
        assert_eq!(apply_zone_offset(1_000, "z"), 1_000);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_apply_zone_offset_6d98e87c__v3_numeric_offset_is_applied() {
        // +01:00 local is one hour *earlier* in UTC.
        assert_eq!(apply_zone_offset(3_600_000_000_000, "+0100"), 0);
    }

    // storage_stream_timestamp_parse_iso8601_6354738d (`parse_iso8601`):
    // `sep != "T" && sep != "t" && sep != " "` -- v1..v3 each make one leaf
    // false (an accepted separator), v4 makes all three true (rejected).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_parse_iso8601_6354738d__v1_upper_t_separator() {
        assert!(parse_iso8601("2026-09-10T08:00:05Z").is_some());
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_parse_iso8601_6354738d__v2_lower_t_separator() {
        assert_eq!(
            parse_iso8601("2026-09-10t08:00:05Z"),
            parse_iso8601("2026-09-10T08:00:05Z")
        );
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_parse_iso8601_6354738d__v3_space_separator() {
        assert_eq!(
            parse_iso8601("2026-09-10 08:00:05Z"),
            parse_iso8601("2026-09-10T08:00:05Z")
        );
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_parse_iso8601_6354738d__v4_other_separator_rejected() {
        assert_eq!(parse_iso8601("2026-09-10X08:00:05Z"), None);
    }

    // storage_stream_timestamp_parse_epoch_9466aaa5 (`parse_epoch`):
    // `digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit())`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_parse_epoch_9466aaa5__v1_empty_input() {
        assert_eq!(parse_epoch(""), None);
        assert_eq!(parse_epoch("-"), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_parse_epoch_9466aaa5__v2_non_digit_input() {
        assert_eq!(parse_epoch("17000000ab"), None);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_stream_timestamp_parse_epoch_9466aaa5__v3_all_digits_parse_by_width() {
        assert_eq!(parse_epoch("1700000000"), Some(1_700_000_000_000_000_000));
    }

    #[test]
    fn parses_iso8601_with_fraction_and_zone() {
        let ns = parse_iso8601("2026-09-10T08:00:05.123Z").expect("parses");
        let base = epoch_nanos(2026, 9, 10, 8, 0, 5, 0).expect("base");
        assert_eq!(ns, base + 123_000_000);
    }

    #[test]
    fn parses_iso8601_with_offset() {
        let plus = parse_iso8601("2026-09-10T08:00:05+02:00").expect("parses");
        let utc = parse_iso8601("2026-09-10T06:00:05Z").expect("parses");
        assert_eq!(plus, utc);
    }

    #[test]
    fn parses_epoch_by_digit_count() {
        assert_eq!(parse_epoch("1700000000"), Some(1_700_000_000_000_000_000));
        assert_eq!(
            parse_epoch("1700000000000"),
            Some(1_700_000_000_000_000_000)
        );
        assert_eq!(
            parse_epoch("1700000000000000"),
            Some(1_700_000_000_000_000_000)
        );
        assert_eq!(
            parse_epoch("1700000000000000000"),
            Some(1_700_000_000_000_000_000)
        );
        assert_eq!(parse_epoch("not-a-number"), None);
    }

    #[test]
    fn flexible_prefers_iso_then_falls_back_to_epoch() {
        assert!(parse_flexible("2026-09-10T08:00:05Z").is_some());
        assert_eq!(parse_flexible("1700000000"), parse_epoch("1700000000"));
        assert_eq!(parse_flexible("garbage"), None);
    }
}
