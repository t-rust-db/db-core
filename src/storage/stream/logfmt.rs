//! `key=value` parser (#320): Heroku, Go kit, zap console.
//!
//! Example: `ts=2026-09-10T08:00:05Z level=warn msg="request slow"
//! service=auth duration=184ms status=500`
//!
//! Shares the alias tables and typed inference with `jsonl` (#319) --
//! same promotion rules, flat by construction (no nesting). A number
//! carrying a unit suffix (`184ms`, `2.5s`, `1.2MB`) is kept as a string at
//! ingest; parsing it is the late-parse route (`duration_ms()`, #307), not a
//! parser special case here.

use super::alias::{self, Promotion};
use super::batch::{LineParser, LogBatch, Severity, Source};
use super::timestamp;

/// `key=value` parser that fills `LogBatch` from raw lines.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogfmtParser;

impl LogfmtParser {
    /// Create a new parser.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Cheap format sniff: at least two `key=value` pairs and no leading
    /// `{` or `<PRI>` (LAB271 rule) -- used by the format detector (#321).
    #[must_use]
    pub fn looks_like_logfmt(line: &[u8]) -> bool {
        let Ok(s) = std::str::from_utf8(line) else {
            return false;
        };
        let trimmed = s.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('<') {
            return false;
        }
        Self::tokenize(s).filter(|(_, v)| v.is_some()).count() >= 2
    }

    /// Parse lines from a buffer into a `LogBatch`. Returns the batch and
    /// the number of bytes consumed.
    pub fn parse_batch<'a>(
        &self,
        source: Source,
        buffer: &'a [u8],
        max_lines: usize,
    ) -> (LogBatch<'a>, usize) {
        let mut batch = LogBatch::new(source);
        let mut consumed: usize = 0;
        let mut lines_parsed: usize = 0;

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
                if lines_parsed >= max_lines {
                    break;
                }
            }
        }

        (batch, consumed)
    }

    /// Parse a single logfmt line into the batch.
    pub(crate) fn parse_line<'a>(&self, batch: &mut LogBatch<'a>, line: &'a [u8]) {
        let line_str = match std::str::from_utf8(line) {
            Ok(s) => s.trim_end_matches(['\r', '\n']),
            Err(_) => {
                batch.push_line(line, None, None, None, None);
                return;
            }
        };

        let pairs: Vec<(&'a str, Option<&'a str>)> = Self::tokenize(line_str).collect();

        let mut ts: Option<i64> = None;
        let mut severity: Option<Severity> = None;
        let mut message: Option<&'a str> = None;

        for (key, value) in &pairs {
            let Some(v) = value else { continue };
            match alias::classify(key) {
                Some(Promotion::Timestamp) => ts = ts.or_else(|| timestamp::parse_flexible(v)),
                Some(Promotion::Level) => severity = severity.or_else(|| alias::parse_level(v)),
                Some(Promotion::Message) => message = message.or(Some(*v)),
                _ => {}
            }
        }

        batch.push_line(line, ts, severity, None, message);

        for (key, value) in &pairs {
            match alias::classify(key) {
                Some(Promotion::Timestamp | Promotion::Level | Promotion::Message) => {}
                Some(Promotion::Host) => Self::set_typed(batch, "host", *value),
                Some(Promotion::Service) => Self::set_typed(batch, "service", *value),
                Some(Promotion::Pid) => Self::set_typed(batch, "pid", *value),
                None => Self::set_typed(batch, key, *value),
            }
        }
    }

    /// Set a Tier-3 field, inferring int/float/bool from the raw text
    /// (falling back to string). Bare keys (no `=value`) become `true`.
    fn set_typed<'a>(batch: &mut LogBatch<'a>, key: &str, value: Option<&'a str>) {
        let Some(v) = value else {
            batch.set_field_bool(key, true);
            return;
        };
        if let Ok(i) = v.parse::<i64>() {
            batch.set_field_int(key, i);
        } else if let Ok(f) = v.parse::<f64>() {
            batch.set_field_float(key, f);
        } else if let Ok(b) = v.parse::<bool>() {
            batch.set_field_bool(key, b);
        } else {
            batch.set_field(key, v);
        }
    }

    /// Tokenize `key=value` pairs, quoted values (with `\"`/`\\` escapes
    /// left verbatim, same as `jsonl`), and bare keys (`debug` -> `(key,
    /// None)`, promoted to `true` by the caller). Duplicate keys: last one
    /// wins by construction (later `set_*` calls overwrite earlier ones for
    /// the same row via re-push -- documented, not deduplicated here).
    fn tokenize(s: &str) -> impl Iterator<Item = (&str, Option<&str>)> {
        let mut rest = s;
        std::iter::from_fn(move || loop {
            rest = rest.trim_start();
            if rest.is_empty() {
                return None;
            }
            let key_end = rest
                .find(|c: char| c == '=' || c.is_whitespace())
                .unwrap_or(rest.len());
            let key = rest.get(..key_end)?;
            if key.is_empty() {
                // Stray '=' with no key; skip one char and keep scanning.
                let skip = rest.chars().next().map_or(0, char::len_utf8);
                rest = rest.get(skip..).unwrap_or("");
                continue;
            }
            let after_key = rest.get(key_end..)?;
            let Some(after_eq) = after_key.strip_prefix('=') else {
                rest = after_key;
                return Some((key, None));
            };
            if let Some(after_quote) = after_eq.strip_prefix('"') {
                let mut escaped = false;
                for (i, b) in after_quote.bytes().enumerate() {
                    if escaped {
                        escaped = false;
                    } else if b == b'\\' {
                        escaped = true;
                    } else if b == b'"' {
                        let value = after_quote.get(..i).unwrap_or("");
                        let after = i.checked_add(1).unwrap_or(after_quote.len());
                        rest = after_quote.get(after..).unwrap_or("");
                        return Some((key, Some(value)));
                    }
                }
                // Unterminated quote: take the rest of the line.
                rest = "";
                return Some((key, Some(after_quote)));
            }
            let value_end = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
            let value = after_eq.get(..value_end).unwrap_or("");
            rest = after_eq.get(value_end..).unwrap_or("");
            return Some((key, Some(value)));
        })
    }
}

impl LineParser for LogfmtParser {
    fn parse_batch<'a>(
        &self,
        source: Source,
        buffer: &'a [u8],
        max_lines: usize,
    ) -> (LogBatch<'a>, usize) {
        self.parse_batch(source, buffer, max_lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::batch::{FieldColumn, SourceKind};

    fn parse_one(line: &'static str) -> LogBatch<'static> {
        let parser = LogfmtParser::new();
        let source = Source::new(SourceKind::File, "test.log");
        let input = line.as_bytes();
        let (batch, consumed) = parser.parse_batch(source, input, 10);
        assert_eq!(consumed, input.len());
        batch
    }

    #[test]
    fn parses_heroku_style_line() {
        let batch = parse_one(
            "ts=2026-09-10T08:00:05Z level=warn msg=\"request slow\" service=auth duration=184ms status=500\n",
        );
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.severity[0], Some(Severity::Warn));
        assert!(batch.timestamp_ns[0].is_some());
        assert_eq!(batch.message[0], Some("request slow"));

        match batch.fields.get("service").expect("service field") {
            FieldColumn::Dict { dict, indices } => {
                assert_eq!(dict[indices[0].expect("value") as usize], "auth");
            }
            other => panic!("expected dict-encoded str, got {other:?}"),
        }
        match batch.fields.get("status").expect("status field") {
            FieldColumn::Int(v) => assert_eq!(v, &[Some(500)]),
            other => panic!("expected Int, got {other:?}"),
        }
        // Unit-suffixed number stays a string at ingest.
        match batch.fields.get("duration").expect("duration field") {
            FieldColumn::Dict { dict, indices } => {
                assert_eq!(dict[indices[0].expect("value") as usize], "184ms");
            }
            other => panic!("expected dict-encoded str, got {other:?}"),
        }
    }

    #[test]
    fn bare_key_promotes_to_true() {
        let batch = parse_one("msg=hi debug\n");
        match batch.fields.get("debug").expect("debug field") {
            FieldColumn::Bool(v) => assert_eq!(v, &[Some(true)]),
            other => panic!("expected Bool, got {other:?}"),
        }
    }

    #[test]
    fn detection_requires_two_pairs_and_no_json_or_syslog_lead() {
        assert!(LogfmtParser::looks_like_logfmt(
            b"level=info msg=\"started\""
        ));
        assert!(!LogfmtParser::looks_like_logfmt(b"level=info"));
        assert!(!LogfmtParser::looks_like_logfmt(b"{\"level\":\"info\"}"));
        assert!(!LogfmtParser::looks_like_logfmt(b"<134>syslog msg=here"));
    }
}
