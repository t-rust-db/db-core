//! Common/Combined Log Format (Apache/nginx access log) parser.
//!
//! Format: `host ident authuser [date] "request" status bytes "referer" "user-agent"`
//!
//! Example: `10.0.0.1 - alice [10/Sep/2026:08:00:05 +0200] "GET /path HTTP/1.1" 500 1834 "https://example.com" "curl/8.0"`
//!
//! Unlike syslog, CLF carries no severity -- ADR-0018 amendment: leave
//! `severity = None` and never derive it from `status`; queries filter with
//! `WHERE status >= 500` instead. `status`/`bytes` are typed Tier-3 `Int`
//! columns (not Tier 2b: only access logs have them, and a Tier-3 Int/Dict
//! filter is just as fast as a predefined column).

use super::batch::{LineParser, LogBatch, Source};

/// CLF/combined access-log parser that fills `LogBatch` from raw lines.
#[derive(Debug, Clone, Default)]
pub struct ClfParser;

impl ClfParser {
    /// Create a new parser.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Parse lines from a buffer into a `LogBatch`.
    ///
    /// Returns the batch and the number of bytes consumed.
    pub fn parse_batch<'a>(
        &self,
        source: Source,
        buffer: &'a [u8],
        max_lines: usize,
    ) -> (LogBatch<'a>, usize) {
        let mut batch = LogBatch::new(source);
        let mut consumed: usize = 0;
        let mut lines_parsed: usize = 0;
        let limit = max_lines;

        let mut start = 0;
        for (i, &byte) in buffer.iter().enumerate() {
            if byte == b'\n' {
                if start < i {
                    let line = buffer.get(start..i).unwrap_or(b"");
                    self.parse_line(&mut batch, line);
                    lines_parsed = lines_parsed.saturating_add(1);
                }
                consumed = i.saturating_add(1);
                start = consumed;

                if lines_parsed >= limit {
                    break;
                }
            }
        }

        (batch, consumed)
    }

    /// Parse a single CLF line into the batch.
    fn parse_line<'a>(&self, batch: &mut LogBatch<'a>, line: &'a [u8]) {
        let line_str = match std::str::from_utf8(line) {
            Ok(s) => s,
            Err(_) => {
                // Non-UTF8 line: store raw only
                batch.push_line(line, None, None, None, None);
                return;
            }
        };

        let (host, rest) = self.parse_word(line_str);
        let (_ident, rest) = self.parse_word(rest);
        let (authuser, rest) = self.parse_word(rest);
        let (timestamp_ns, rest) = self.parse_bracketed_timestamp(rest);
        let (request, rest) = self.parse_quoted(rest);
        let (status_str, rest) = self.parse_word(rest);
        let (bytes_str, rest) = self.parse_word(rest);
        let (referer, rest) = self.parse_quoted(rest);
        let (user_agent, _rest) = self.parse_quoted(rest);

        // No severity: CLF access logs carry no log level. Use the request
        // line itself (e.g. `GET /path HTTP/1.1`) as the human-readable
        // message; queries that need severity-like filtering use `status`.
        let message = request.filter(|r| !r.is_empty());
        batch.push_line(line, timestamp_ns, None, None, message);

        if let Some(request) = message {
            let (method, r) = self.parse_word(request);
            let (path, r) = self.parse_word(r);
            let (protocol, _) = self.parse_word(r);
            if let Some(m) = method {
                batch.set_field("method", m);
            }
            if let Some(p) = path {
                batch.set_field("path", p);
            }
            if let Some(p) = protocol {
                batch.set_field("protocol", p);
            }
        }

        if let Some(status) = status_str.and_then(|s| s.parse::<i64>().ok()) {
            batch.set_field_int("status", status);
        }
        if let Some(bytes) = bytes_str.and_then(|s| s.parse::<i64>().ok()) {
            batch.set_field_int("bytes", bytes);
        }
        if let Some(host) = host {
            batch.set_field("ip", host);
        }
        if let Some(u) = authuser.filter(|u| *u != "-") {
            batch.set_field("user", u);
        }
        if let Some(r) = referer.filter(|r| *r != "-") {
            batch.set_field("referer", r);
        }
        if let Some(ua) = user_agent.filter(|u| *u != "-") {
            batch.set_field("ua", ua);
        }
    }

    /// Parse a word (whitespace-delimited), returning (word, rest).
    fn parse_word<'a>(&self, s: &'a str) -> (Option<&'a str>, &'a str) {
        let s = s.trim_start();
        if s.is_empty() {
            return (None, s);
        }
        match s.find(char::is_whitespace) {
            Some(i) => {
                let word = s.get(..i).unwrap_or("");
                let rest = s.get(i..).unwrap_or("").trim_start();
                (Some(word), rest)
            }
            None => (Some(s), ""),
        }
    }

    /// Parse a `"quoted"` field, returning (inner, rest). Malformed input
    /// (no opening/closing quote) leaves `rest` untouched and returns `None`.
    fn parse_quoted<'a>(&self, s: &'a str) -> (Option<&'a str>, &'a str) {
        let s = s.trim_start();
        if !s.starts_with('"') {
            return (None, s);
        }
        let after_quote = s.get(1..).unwrap_or("");
        match after_quote.find('"') {
            Some(end) => {
                let inner = after_quote.get(..end).unwrap_or("");
                let rest = after_quote
                    .get(end.saturating_add(1)..)
                    .unwrap_or("")
                    .trim_start();
                (Some(inner), rest)
            }
            None => (None, s),
        }
    }

    /// Parse `[date]`, returning (nanos, rest).
    fn parse_bracketed_timestamp<'a>(&self, s: &'a str) -> (Option<i64>, &'a str) {
        let s = s.trim_start();
        if !s.starts_with('[') {
            return (None, s);
        }
        let after_bracket = s.get(1..).unwrap_or("");
        match after_bracket.find(']') {
            Some(end) => {
                let inner = after_bracket.get(..end).unwrap_or("");
                let rest = after_bracket
                    .get(end.saturating_add(1)..)
                    .unwrap_or("")
                    .trim_start();
                (Self::parse_clf_timestamp(inner), rest)
            }
            None => (None, s),
        }
    }

    /// Parse `dd/Mon/yyyy:HH:MM:SS +zzzz` (the `[date]` field's contents,
    /// bracket already stripped) into nanoseconds since Unix epoch (UTC,
    /// simplified: no leap-second handling).
    fn parse_clf_timestamp(s: &str) -> Option<i64> {
        let mut date_parts = s.splitn(3, '/');
        let day: u32 = date_parts.next()?.parse().ok()?;
        let month = Self::month_from_str(date_parts.next()?)?;
        let remainder = date_parts.next()?; // "yyyy:HH:MM:SS +zzzz" or "yyyy:HH:MM:SS"

        let mut time_and_zone = remainder.splitn(2, ' ');
        let datetime = time_and_zone.next()?; // "yyyy:HH:MM:SS"
        let zone = time_and_zone.next();

        let mut dt_parts = datetime.splitn(2, ':');
        let year: i32 = dt_parts.next()?.parse().ok()?;
        let (hour, minute, second) = Self::parse_time(dt_parts.next()?)?;

        let ts = Self::epoch_nanos(year, month, day, hour, minute, second)?;
        Some(Self::apply_zone_offset(ts, zone))
    }

    /// Apply a `+HHMM`/`-HHMM` UTC offset (as found after the CLF timestamp)
    /// to a UTC nanosecond timestamp. An absent or malformed zone leaves the
    /// timestamp unchanged (treated as already UTC).
    fn apply_zone_offset(ts_ns: i64, zone: Option<&str>) -> i64 {
        let Some(z) = zone else { return ts_ns };
        if z.len() != 5 {
            return ts_ns;
        }
        let sign: i64 = if z.starts_with('-') { -1 } else { 1 };
        let Some(zh) = z.get(1..3).and_then(|s| s.parse::<i64>().ok()) else {
            return ts_ns;
        };
        let Some(zm) = z.get(3..5).and_then(|s| s.parse::<i64>().ok()) else {
            return ts_ns;
        };
        let offset_ns = zh
            .saturating_mul(3600)
            .saturating_add(zm.saturating_mul(60))
            .saturating_mul(1_000_000_000);
        // "+0200" means local time is UTC+2, so UTC = local - offset.
        ts_ns.saturating_sub(sign.saturating_mul(offset_ns))
    }

    /// Parse "HH:MM:SS" into (hour, minute, second).
    fn parse_time(s: &str) -> Option<(u32, u32, u32)> {
        if s.len() < 8 {
            return None;
        }
        let hour: u32 = s.get(..2)?.parse().ok()?;
        let minute: u32 = s.get(3..5)?.parse().ok()?;
        let second: u32 = s.get(6..8)?.parse().ok()?;
        Some((hour, minute, second))
    }

    /// Month abbreviation to 1-12.
    fn month_from_str(s: &str) -> Option<u32> {
        Some(match s {
            "Jan" => 1,
            "Feb" => 2,
            "Mar" => 3,
            "Apr" => 4,
            "May" => 5,
            "Jun" => 6,
            "Jul" => 7,
            "Aug" => 8,
            "Sep" => 9,
            "Oct" => 10,
            "Nov" => 11,
            "Dec" => 12,
            _ => return None,
        })
    }

    /// Convert an absolute date/time to nanoseconds since Unix epoch
    /// (simplified: no leap-second handling; leap years approximated as
    /// one every four years, same as `SyslogParser::to_epoch_nanos`).
    fn epoch_nanos(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<i64> {
        const DAYS_BEFORE_MONTH: [u32; 12] =
            [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

        if year < 1970
            || month == 0
            || month > 12
            || day == 0
            || day > 31
            || hour > 23
            || minute > 59
            || second > 59
        {
            return None;
        }

        let month_idx = month.saturating_sub(1) as usize;
        let days_before = DAYS_BEFORE_MONTH.get(month_idx).copied()?;

        let years_since_epoch = year.saturating_sub(1970);
        let leap_years = years_since_epoch / 4;
        #[allow(clippy::cast_sign_loss)]
        let days_from_years = (years_since_epoch as u32)
            .saturating_mul(365)
            .saturating_add(leap_years as u32);

        let total_days = days_from_years
            .saturating_add(days_before)
            .saturating_add(day.saturating_sub(1));

        let total_seconds = i64::from(total_days)
            .saturating_mul(86400)
            .saturating_add(i64::from(hour).saturating_mul(3600))
            .saturating_add(i64::from(minute).saturating_mul(60))
            .saturating_add(i64::from(second));

        Some(total_seconds.saturating_mul(1_000_000_000))
    }
}

impl LineParser for ClfParser {
    fn parse_batch<'a>(
        &self,
        source: Source,
        buffer: &'a [u8],
        max_lines: usize,
    ) -> (LogBatch<'a>, usize) {
        Self::parse_batch(self, source, buffer, max_lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::{FieldColumn, SourceKind};

    fn parse_one(line: &'static str) -> LogBatch<'static> {
        let parser = ClfParser::new();
        let source = Source::new(SourceKind::File, "/var/log/access.log");
        let input = line.as_bytes();
        let (batch, consumed) = parser.parse_batch(source, input, 10);
        assert_eq!(consumed, input.len());
        batch
    }

    #[test]
    fn parses_basic_combined_log_line() {
        let batch = parse_one(
            "10.0.0.1 - alice [10/Sep/2026:08:00:05 +0200] \"GET /path HTTP/1.1\" 500 1834 \"https://example.com\" \"curl/8.0\"\n",
        );

        assert_eq!(batch.len(), 1);
        assert_eq!(batch.severity.first(), Some(&None));
        assert!(batch.timestamp_ns.first().copied().flatten().is_some());
        assert_eq!(batch.message.first(), Some(&Some("GET /path HTTP/1.1")));

        match batch.fields.get("status") {
            Some(FieldColumn::Int(v)) => assert_eq!(v, &[Some(500)]),
            other => panic!("expected Int status, got {other:?}"),
        }
        match batch.fields.get("bytes") {
            Some(FieldColumn::Int(v)) => assert_eq!(v, &[Some(1834)]),
            other => panic!("expected Int bytes, got {other:?}"),
        }
        match batch.fields.get("method") {
            Some(FieldColumn::Dict { dict, .. }) => assert_eq!(dict.as_slice(), ["GET"]),
            other => panic!("expected Dict method, got {other:?}"),
        }
        assert_eq!(batch.fields.get("path").map(FieldColumn::len), Some(1));
        match batch.fields.get("ip") {
            Some(FieldColumn::Dict { dict, .. }) => assert_eq!(dict.as_slice(), ["10.0.0.1"]),
            other => panic!("expected Dict ip, got {other:?}"),
        }
        match batch.fields.get("user") {
            Some(FieldColumn::Dict { dict, .. }) => assert_eq!(dict.as_slice(), ["alice"]),
            other => panic!("expected Dict user, got {other:?}"),
        }
        match batch.fields.get("referer") {
            Some(FieldColumn::Dict { dict, .. }) => {
                assert_eq!(dict.as_slice(), ["https://example.com"]);
            }
            other => panic!("expected Dict referer, got {other:?}"),
        }
    }

    #[test]
    fn dash_placeholders_are_not_recorded_as_fields() {
        let batch = parse_one(
            "10.0.0.1 - - [10/Sep/2026:08:00:05 +0200] \"GET / HTTP/1.1\" 200 - \"-\" \"-\"\n",
        );

        assert!(batch.fields.get("user").is_none());
        assert!(batch.fields.get("referer").is_none());
        assert!(batch.fields.get("ua").is_none());
        assert!(batch.fields.get("bytes").is_none());
        match batch.fields.get("status") {
            Some(FieldColumn::Int(v)) => assert_eq!(v, &[Some(200)]),
            other => panic!("expected Int status, got {other:?}"),
        }
    }

    #[test]
    fn utc_offset_is_applied() {
        // 08:00:05 +0200 is 06:00:05 UTC.
        let plus = parse_one(
            "1.1.1.1 - - [10/Sep/2026:08:00:05 +0200] \"GET / HTTP/1.1\" 200 0 \"-\" \"-\"\n",
        );
        let utc = parse_one(
            "1.1.1.1 - - [10/Sep/2026:06:00:05 +0000] \"GET / HTTP/1.1\" 200 0 \"-\" \"-\"\n",
        );
        assert_eq!(plus.timestamp_ns.first(), utc.timestamp_ns.first());
        assert!(plus.timestamp_ns.first().copied().flatten().is_some());
    }

    #[test]
    fn status_ge_500_and_method_eq_post_selects_expected_rows() {
        let parser = ClfParser::new();
        let source = Source::new(SourceKind::File, "/var/log/access.log");
        let input = b"1.1.1.1 - - [10/Sep/2026:08:00:00 +0000] \"POST /submit HTTP/1.1\" 500 12 \"-\" \"-\"\n\
1.1.1.1 - - [10/Sep/2026:08:00:01 +0000] \"GET /health HTTP/1.1\" 200 3 \"-\" \"-\"\n\
1.1.1.1 - - [10/Sep/2026:08:00:02 +0000] \"POST /submit HTTP/1.1\" 200 5 \"-\" \"-\"\n";
        let (batch, _) = parser.parse_batch(source, input, 10);

        let statuses = match batch.fields.get("status").unwrap() {
            FieldColumn::Int(v) => v.clone(),
            other => panic!("expected Int, got {other:?}"),
        };
        let methods = match batch.fields.get("method").unwrap() {
            FieldColumn::Dict { dict, indices } => indices
                .iter()
                .map(|i| i.map(|i| dict[usize::from(i)]))
                .collect::<Vec<_>>(),
            other => panic!("expected Dict, got {other:?}"),
        };

        let matches: Vec<bool> = (0..batch.len())
            .map(|row| statuses[row] >= Some(500) && methods[row] == Some("POST"))
            .collect();
        assert_eq!(matches, vec![true, false, false]);
    }

    #[test]
    fn high_cardinality_path_degrades_dict_to_str() {
        let parser = ClfParser::new();
        let source = Source::new(SourceKind::File, "/var/log/access.log");

        let mut input = String::new();
        for i in 0..300 {
            input.push_str(&format!(
                "1.1.1.1 - - [10/Sep/2026:08:00:00 +0000] \"GET /item/{i} HTTP/1.1\" 200 0 \"-\" \"-\"\n"
            ));
        }
        let (batch, _) = parser.parse_batch(source, input.as_bytes(), 1000);

        assert_eq!(batch.len(), 300);
        match batch.fields.get("path") {
            Some(FieldColumn::Str(v)) => assert_eq!(v.len(), 300),
            other => panic!("expected Dict->Str degrade for high-cardinality path, got {other:?}"),
        }
        // Low-cardinality fields stay Dict.
        match batch.fields.get("method") {
            Some(FieldColumn::Dict { dict, .. }) => assert_eq!(dict.as_slice(), ["GET"]),
            other => panic!("expected method to stay Dict, got {other:?}"),
        }
    }

    #[test]
    fn non_utf8_line_stores_raw_only() {
        let parser = ClfParser::new();
        let source = Source::new(SourceKind::File, "/var/log/access.log");
        let input: &[u8] = b"not \xffutf8\n";
        let (batch, _) = parser.parse_batch(source, input, 10);

        assert_eq!(batch.len(), 1);
        assert_eq!(batch.message.first(), Some(&None));
        assert_eq!(batch.severity.first(), Some(&None));
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<file-stem>_<line>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#299 MC/DC backfill).

    use super::ClfParser;

    // clf_267: `year < 1970 || month == 0 || month > 12 || day == 0
    //   || day > 31 || hour > 23 || minute > 59 || second > 59`
    #[test]
    fn mcdc__clf_267__v1_year_before_epoch() {
        assert_eq!(ClfParser::epoch_nanos(1969, 1, 1, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__clf_267__v2_month_zero() {
        assert_eq!(ClfParser::epoch_nanos(2026, 0, 1, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__clf_267__v3_month_over_twelve() {
        assert_eq!(ClfParser::epoch_nanos(2026, 13, 1, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__clf_267__v4_day_zero() {
        assert_eq!(ClfParser::epoch_nanos(2026, 1, 0, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__clf_267__v5_day_over_thirty_one() {
        assert_eq!(ClfParser::epoch_nanos(2026, 1, 32, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__clf_267__v6_hour_over_twenty_three() {
        assert_eq!(ClfParser::epoch_nanos(2026, 1, 1, 24, 0, 0), None);
    }

    #[test]
    fn mcdc__clf_267__v7_minute_over_fifty_nine() {
        assert_eq!(ClfParser::epoch_nanos(2026, 1, 1, 0, 60, 0), None);
    }

    #[test]
    fn mcdc__clf_267__v8_second_over_fifty_nine() {
        assert_eq!(ClfParser::epoch_nanos(2026, 1, 1, 0, 0, 60), None);
    }

    #[test]
    fn mcdc__clf_267__v9_all_false_is_valid() {
        assert!(ClfParser::epoch_nanos(2026, 1, 1, 0, 0, 0).is_some());
    }
}
