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
