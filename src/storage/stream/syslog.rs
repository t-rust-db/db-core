//! RFC 3164 (BSD) syslog parser.
//!
//! Format: `<PRI>TIMESTAMP HOSTNAME TAG[PID]: MESSAGE`
//!
//! Example: `<134>Sep  9 14:23:01 webserver nginx[1234]: GET /api/health 200`

use super::batch::{Facility, LogBatch, Severity, Source};

/// Syslog parser that fills `LogBatch` from raw lines.
pub struct SyslogParser {
    /// Current year for timestamp parsing (syslog doesn't include year).
    year: i32,
}

impl SyslogParser {
    /// Create a new parser using the current year.
    #[must_use]
    pub fn new() -> Self {
        Self { year: 2024 } // TODO: get from system
    }

    /// Create a parser with explicit year.
    #[must_use]
    pub const fn with_year(year: i32) -> Self {
        Self { year }
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
        // The caller bounds the batch (`SEGMENT_MAX_ROWS` for a segment);
        // `BATCH_SIZE` is only the initial capacity.
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

    /// Parse a single syslog line into the batch.
    fn parse_line<'a>(&self, batch: &mut LogBatch<'a>, line: &'a [u8]) {
        // Try to parse as UTF-8
        let line_str = match std::str::from_utf8(line) {
            Ok(s) => s,
            Err(_) => {
                // Non-UTF8 line: store raw only
                batch.push_line(line, None, None, None, None);
                return;
            }
        };

        // Parse <PRI>
        let (pri, rest) = match self.parse_pri(line_str) {
            Some(p) => p,
            None => {
                // No PRI: store as-is
                batch.push_line(line, None, None, None, Some(line_str));
                return;
            }
        };

        // Extract facility and severity from PRI (PRI = facility * 8 + severity)
        let facility_code = pri >> 3; // Upper 5 bits
        let syslog_severity = pri & 0x07; // Lower 3 bits
        let facility = Facility::from_code(facility_code);
        let severity = Severity::from_syslog(syslog_severity);

        // Parse timestamp (e.g., "Sep  9 14:23:01")
        let (timestamp_ns, rest) = self.parse_timestamp(rest);

        // Parse hostname
        let (hostname, rest) = self.parse_word(rest);

        // Set resource hostname if found
        if let Some(h) = hostname {
            if batch.resource.hostname.is_none() {
                batch.resource = super::batch::Resource::with_hostname(h);
            }
        }

        // Parse tag[pid]: (e.g., "nginx[1234]:")
        let (tag, pid, rest) = self.parse_tag_pid(rest);

        // Rest is the message
        let message = rest.trim();

        // Tier 2: push core fields
        batch.push_line(line, timestamp_ns, severity, facility, Some(message));

        // Tier 3: dynamic fields
        if let Some(t) = tag {
            batch.set_field("tag", t);
        }
        if let Some(p) = pid {
            batch.set_field("pid", p);
        }
        if let Some(h) = hostname {
            batch.set_field("hostname", h);
        }
    }

    /// Parse `<PRI>` prefix, returning (priority, rest).
    fn parse_pri<'a>(&self, s: &'a str) -> Option<(u8, &'a str)> {
        if !s.starts_with('<') {
            return None;
        }
        let end = s.find('>')?;
        let pri_str = s.get(1..end)?;
        let pri: u8 = pri_str.parse().ok()?;
        let rest = s.get(end.saturating_add(1)..)?;
        Some((pri, rest))
    }

    /// Parse BSD timestamp (e.g., "Sep  9 14:23:01"), returning (nanos, rest).
    fn parse_timestamp<'a>(&self, s: &'a str) -> (Option<i64>, &'a str) {
        // Expected format: "Mmm dd HH:MM:SS " or "Mmm  d HH:MM:SS "
        // Minimum: "Jan  1 00:00:00 " = 16 chars
        if s.len() < 16 {
            return (None, s);
        }

        let month_str = s.get(..3).unwrap_or("");
        let month = match month_str {
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
            _ => return (None, s),
        };

        // Day: chars 4-5, may have leading space
        let day_str = s.get(4..6).unwrap_or("").trim();
        let day: u32 = match day_str.parse() {
            Ok(d) => d,
            Err(_) => return (None, s),
        };

        // Time: HH:MM:SS at chars 7-14
        let time_str = s.get(7..15).unwrap_or("");
        let (hour, minute, second) = match self.parse_time(time_str) {
            Some(t) => t,
            None => return (None, s),
        };

        // Convert to nanos since epoch (simplified, ignores timezone)
        let timestamp_ns = self.to_epoch_nanos(month, day, hour, minute, second);

        // Rest starts after the space following the timestamp
        let rest = s.get(16..).unwrap_or("").trim_start();
        (timestamp_ns, rest)
    }

    /// Parse "HH:MM:SS" into (hour, minute, second).
    fn parse_time(&self, s: &str) -> Option<(u32, u32, u32)> {
        if s.len() < 8 {
            return None;
        }
        let hour: u32 = s.get(..2)?.parse().ok()?;
        let minute: u32 = s.get(3..5)?.parse().ok()?;
        let second: u32 = s.get(6..8)?.parse().ok()?;
        Some((hour, minute, second))
    }

    /// Convert date/time to nanoseconds since Unix epoch (simplified).
    fn to_epoch_nanos(
        &self,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<i64> {
        // Days in each month (non-leap year approximation)
        const DAYS_BEFORE_MONTH: [u32; 12] =
            [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

        if month == 0 || month > 12 || day == 0 || day > 31 {
            return None;
        }

        let month_idx = month.saturating_sub(1) as usize;
        let days_before = DAYS_BEFORE_MONTH.get(month_idx).copied()?;

        // Days since epoch for year (simplified: 365.25 days/year from 1970)
        let years_since_epoch = self.year.saturating_sub(1970);
        let leap_years = years_since_epoch / 4; // Approximation
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

    /// Parse tag[pid]: returning (tag, pid, rest).
    fn parse_tag_pid<'a>(&self, s: &'a str) -> (Option<&'a str>, Option<&'a str>, &'a str) {
        let s = s.trim_start();

        // Find end of tag (bracket, colon, or space)
        let end = s
            .find(|c| c == '[' || c == ':' || char::is_whitespace(c))
            .unwrap_or(s.len());

        if end == 0 {
            return (None, None, s);
        }

        let tag = s.get(..end);

        // Extract [pid] if present
        let rest = s.get(end..).unwrap_or("");
        let (pid, rest) = if rest.starts_with('[') {
            let pid_end = rest.find(']').unwrap_or(0);
            let pid = rest.get(1..pid_end);
            let rest = rest.get(pid_end.saturating_add(1)..).unwrap_or("");
            (pid, rest)
        } else {
            (None, rest)
        };

        // Skip colon and space
        let rest = rest.trim_start_matches(':').trim_start();

        (tag, pid, rest)
    }
}

impl Default for SyslogParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::SourceKind;

    #[test]
    fn parse_basic_syslog() {
        let parser = SyslogParser::with_year(2024);
        let source = Source::new(SourceKind::File, "/var/log/test.log");
        let input = b"<134>Sep  9 14:23:01 webserver nginx[1234]: GET /api/health 200\n";

        let (batch, consumed) = parser.parse_batch(source, input, 10);

        assert_eq!(batch.len(), 1);
        assert_eq!(consumed, input.len());
        assert_eq!(batch.severity.first(), Some(&Some(Severity::Info))); // PRI 134 = facility 16 * 8 + severity 6
        assert_eq!(batch.message.first(), Some(&Some("GET /api/health 200")));
        assert_eq!(batch.resource.hostname.as_deref(), Some("webserver"));
    }

    #[test]
    fn parse_multiple_lines() {
        let parser = SyslogParser::with_year(2024);
        let source = Source::new(SourceKind::File, "/var/log/test.log");
        let input = b"<134>Sep  9 14:23:01 web nginx[1]: line one\n<131>Sep  9 14:23:02 web nginx[2]: line two\n";

        let (batch, _) = parser.parse_batch(source, input, 10);

        assert_eq!(batch.len(), 2);
        assert_eq!(batch.severity.first(), Some(&Some(Severity::Info))); // 134 & 7 = 6 (info)
        assert_eq!(batch.severity.get(1), Some(&Some(Severity::Error))); // 131 & 7 = 3 (err)
    }

    #[test]
    fn parse_without_pri() {
        let parser = SyslogParser::with_year(2024);
        let source = Source::new(SourceKind::File, "/var/log/test.log");
        let input = b"Just a plain line without syslog format\n";

        let (batch, _) = parser.parse_batch(source, input, 10);

        assert_eq!(batch.len(), 1);
        assert_eq!(batch.severity.first(), Some(&None));
        assert_eq!(
            batch.message.first(),
            Some(&Some("Just a plain line without syslog format"))
        );
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<file-stem>_<line>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#299 MC/DC backfill).

    use super::SyslogParser;

    // syslog_209: `month == 0 || month > 12 || day == 0 || day > 31`
    #[test]
    fn mcdc__syslog_209__v1_month_zero() {
        let p = SyslogParser::with_year(2026);
        assert_eq!(p.to_epoch_nanos(0, 1, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__syslog_209__v2_month_over_twelve() {
        let p = SyslogParser::with_year(2026);
        assert_eq!(p.to_epoch_nanos(13, 1, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__syslog_209__v3_day_zero() {
        let p = SyslogParser::with_year(2026);
        assert_eq!(p.to_epoch_nanos(1, 0, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__syslog_209__v4_day_over_thirty_one() {
        let p = SyslogParser::with_year(2026);
        assert_eq!(p.to_epoch_nanos(1, 32, 0, 0, 0), None);
    }

    #[test]
    fn mcdc__syslog_209__v5_all_false_is_valid() {
        let p = SyslogParser::with_year(2026);
        assert!(p.to_epoch_nanos(1, 1, 0, 0, 0).is_some());
    }
}
