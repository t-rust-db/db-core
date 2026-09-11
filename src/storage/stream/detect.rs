//! Format detection with lock-on (#321): syslog / CLF / JSONL / logfmt,
//! plus Docker `json-file` / Kubernetes CRI container unwrap.
//!
//! Detects **per file with lock-on**: sample the first ~32 lines, pick the
//! format >= 80% of them match (lnav). Falls back to **per-line** detection
//! only if lock-on fails (LAB271 mixed-file mode).
//!
//! Detection order is cheap-first: `{` -> JSONL; `<digits>` -> syslog; IP +
//! `[` -> CLF; >= 2 `k=v` -> logfmt; else freeform.
//!
//! Container formats are detected at the container level; the payload
//! re-enters detection once per file (lock-on on the payload), not per
//! line. `format` is recorded as a Tier-3 Dict column so
//! `WHERE format = 'json'` works on mixed files.

use super::batch::{LogBatch, Source};
use super::clf::ClfParser;
use super::json::{self, JsonValue};
use super::jsonl::JsonlParser;
use super::logfmt::LogfmtParser;
use super::syslog::SyslogParser;

/// Default number of lines sampled for lock-on.
pub const DEFAULT_SAMPLE_LINES: usize = 32;

/// Fraction of sampled lines that must agree for lock-on to succeed.
const LOCK_ON_THRESHOLD: f64 = 0.8;

/// A detected line/file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// RFC 3164 syslog.
    Syslog,
    /// Apache/nginx combined log format.
    Clf,
    /// JSON Lines.
    Jsonl,
    /// `key=value` logfmt.
    Logfmt,
    /// None of the above (raw + timestamp/level sniff is future work).
    Freeform,
}

impl Format {
    /// Stable string for the Tier-3 `format` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Syslog => "syslog",
            Self::Clf => "clf",
            Self::Jsonl => "json",
            Self::Logfmt => "logfmt",
            Self::Freeform => "freeform",
        }
    }
}

/// Result of running lock-on/container detection over a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    /// The logical (payload) format.
    pub format: Format,
    /// Whether lock-on succeeded (>= 80% agreement); `false` means the
    /// sample was mixed and per-line detection should be used instead.
    pub locked: bool,
    /// Container wrapper, if any (`"docker"` or `"cri"`).
    pub container: Option<&'static str>,
}

/// Cheap-first classification of a single line's format. Operates on the
/// line's own bytes (a container payload, if any, should be extracted
/// first via [`docker_payload`]/[`cri_payload`]).
#[must_use]
pub fn detect_line(line: &[u8]) -> Format {
    let Some(start) = line.iter().position(|b| !b.is_ascii_whitespace()) else {
        return Format::Freeform;
    };
    let Some(s) = line.get(start..) else {
        return Format::Freeform;
    };

    if JsonlParser::looks_like_json_object(s) {
        return Format::Jsonl;
    }
    if looks_like_syslog(s) {
        return Format::Syslog;
    }
    if looks_like_clf(s) {
        return Format::Clf;
    }
    if LogfmtParser::looks_like_logfmt(s) {
        return Format::Logfmt;
    }
    Format::Freeform
}

/// `<digits>` PRI prefix.
fn looks_like_syslog(s: &[u8]) -> bool {
    let Some(rest) = s.strip_prefix(b"<") else {
        return false;
    };
    match rest.iter().position(|&b| b == b'>') {
        Some(pos) if pos > 0 => rest
            .get(..pos)
            .is_some_and(|d| d.iter().all(u8::is_ascii_digit)),
        _ => false,
    }
}

/// `IP - user [date] "request" ...` shape: an IP-ish first token and a
/// bracketed date followed by a quoted request.
fn looks_like_clf(s: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(s) else {
        return false;
    };
    let first = text.split_whitespace().next().unwrap_or("");
    let ip_like = !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    ip_like && text.contains('[') && text.contains('"')
}

/// Sample up to `n` complete (`\n`-terminated) lines from the start of
/// `buffer`.
fn sample_lines(buffer: &[u8], n: usize) -> Vec<&[u8]> {
    let mut lines = Vec::with_capacity(n);
    let mut start = 0usize;
    for (i, &b) in buffer.iter().enumerate() {
        if b == b'\n' {
            if start < i {
                if let Some(line) = buffer.get(start..i) {
                    lines.push(line);
                }
            }
            start = i.saturating_add(1);
            if lines.len() >= n {
                break;
            }
        }
    }
    lines
}

/// Docker `json-file` shape: a top-level JSON object with `log`+`stream`.
/// Returns the raw (unescaped) payload text.
#[must_use]
pub fn docker_payload(line: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(line).ok()?;
    let (JsonValue::Object(fields), _) = json::parse_object(s)? else {
        return None;
    };
    let has_stream = fields.iter().any(|(k, _)| *k == "stream");
    if !has_stream {
        return None;
    }
    fields.iter().find_map(|(k, v)| {
        if *k == "log" {
            if let JsonValue::Str(s) = v {
                Some(*s)
            } else {
                None
            }
        } else {
            None
        }
    })
}

/// Kubernetes CRI shape: `<RFC3339 ts> <stdout|stderr> <F|P> <payload>`.
#[must_use]
pub fn cri_payload(line: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(line).ok()?;
    let mut parts = s.splitn(4, ' ');
    let _ts = parts.next()?;
    let stream = parts.next()?;
    let tag = parts.next()?;
    let payload = parts.next()?;
    if (stream == "stdout" || stream == "stderr") && (tag == "F" || tag == "P") {
        Some(payload)
    } else {
        None
    }
}

/// Detect a container wrapper from a sample of lines: `"docker"` if a
/// majority parse as Docker `json-file`, `"cri"` if a majority parse as
/// Kubernetes CRI, else `None`.
fn detect_container(lines: &[&[u8]]) -> Option<&'static str> {
    if lines.is_empty() {
        return None;
    }
    let docker = lines.iter().filter(|l| docker_payload(l).is_some()).count();
    if docker.saturating_mul(2) >= lines.len() {
        return Some("docker");
    }
    let cri = lines.iter().filter(|l| cri_payload(l).is_some()).count();
    if cri.saturating_mul(2) >= lines.len() {
        return Some("cri");
    }
    None
}

/// Extract a line's logical payload given a container kind (identity if
/// `container` is `None` or extraction fails).
fn payload_for<'a>(line: &'a [u8], container: Option<&'static str>) -> &'a [u8] {
    match container {
        Some("docker") => docker_payload(line).map(str::as_bytes).unwrap_or(line),
        Some("cri") => cri_payload(line).map(str::as_bytes).unwrap_or(line),
        _ => line,
    }
}

/// Run lock-on detection over the first `sample_lines` lines of `buffer`:
/// detect a container wrapper, classify each (unwrapped) line, and lock on
/// a format if >= 80% agree.
#[must_use]
pub fn detect(buffer: &[u8], sample_lines_n: usize) -> Detection {
    let lines = sample_lines(buffer, sample_lines_n);
    if lines.is_empty() {
        return Detection {
            format: Format::Freeform,
            locked: false,
            container: None,
        };
    }

    let container = detect_container(&lines);
    let payloads: Vec<&[u8]> = lines.iter().map(|l| payload_for(l, container)).collect();

    let mut syslog = 0usize;
    let mut clf = 0usize;
    let mut jsonl = 0usize;
    let mut logfmt = 0usize;
    let mut freeform = 0usize;
    for p in &payloads {
        match detect_line(p) {
            Format::Syslog => syslog = syslog.saturating_add(1),
            Format::Clf => clf = clf.saturating_add(1),
            Format::Jsonl => jsonl = jsonl.saturating_add(1),
            Format::Logfmt => logfmt = logfmt.saturating_add(1),
            Format::Freeform => freeform = freeform.saturating_add(1),
        }
    }

    let total = payloads.len();
    let (format, best) = [
        (Format::Syslog, syslog),
        (Format::Clf, clf),
        (Format::Jsonl, jsonl),
        (Format::Logfmt, logfmt),
        (Format::Freeform, freeform),
    ]
    .into_iter()
    .max_by_key(|(_, count)| *count)
    .unwrap_or((Format::Freeform, 0));

    #[allow(clippy::cast_precision_loss)]
    let locked = total > 0 && (best as f64 / total as f64) >= LOCK_ON_THRESHOLD;

    Detection {
        format: if locked { format } else { Format::Freeform },
        locked,
        container,
    }
}

/// Set the Tier-3 `format` column to `format` for every row currently in
/// `batch` (the batch was parsed under one locked-on format, uniform for
/// every row).
pub fn tag_format(batch: &mut LogBatch<'_>, format: Format) {
    for row in 0..batch.len() {
        batch.fields.set("format", row, format.as_str());
    }
}

/// Parse a buffer using lock-on format detection: a locked sample picks one
/// parser for the whole buffer (fast path); a mixed sample falls back to
/// per-line detection, parsing each line with whichever parser its own
/// content matches. Every row gets a Tier-3 `format` column.
pub fn parse_detected<'a>(
    source: Source,
    buffer: &'a [u8],
    max_lines: usize,
    sample_lines_n: usize,
) -> (LogBatch<'a>, usize, Detection) {
    let detection = detect(buffer, sample_lines_n);

    if detection.locked {
        // A container wrapper means the wire format is JSON regardless of
        // the payload's logical format; the payload format is still what
        // gets tagged.
        let outer = if detection.container.is_some() {
            Format::Jsonl
        } else {
            detection.format
        };
        let (mut batch, consumed) = match outer {
            Format::Syslog => SyslogParser::new().parse_batch(source, buffer, max_lines),
            Format::Clf => ClfParser::new().parse_batch(source, buffer, max_lines),
            Format::Jsonl => JsonlParser::new().parse_batch(source, buffer, max_lines),
            Format::Logfmt => LogfmtParser::new().parse_batch(source, buffer, max_lines),
            Format::Freeform => parse_freeform(source, buffer, max_lines),
        };
        tag_format(&mut batch, detection.format);
        (batch, consumed, detection)
    } else {
        let (batch, consumed) = parse_per_line(source, buffer, max_lines, detection.container);
        (batch, consumed, detection)
    }
}

/// Store each line verbatim as its own row's message (no field extraction).
fn parse_freeform<'a>(source: Source, buffer: &'a [u8], max_lines: usize) -> (LogBatch<'a>, usize) {
    let mut batch = LogBatch::new(source);
    let mut consumed = 0usize;
    let mut parsed = 0usize;
    let mut start = 0usize;
    for (i, &b) in buffer.iter().enumerate() {
        if b == b'\n' {
            if start < i {
                if let Some(line) = buffer.get(start..i) {
                    let message = std::str::from_utf8(line).ok();
                    batch.push_line(line, None, None, None, message);
                    parsed = parsed.saturating_add(1);
                }
            }
            consumed = i.saturating_add(1);
            start = consumed;
            if parsed >= max_lines {
                break;
            }
        }
    }
    (batch, consumed)
}

/// Mixed-file fallback: classify and parse each line independently,
/// tagging its own detected format.
fn parse_per_line<'a>(
    source: Source,
    buffer: &'a [u8],
    max_lines: usize,
    container: Option<&'static str>,
) -> (LogBatch<'a>, usize) {
    let syslog = SyslogParser::new();
    let clf = ClfParser::new();
    let jsonl = JsonlParser::new();
    let logfmt = LogfmtParser::new();

    let mut batch = LogBatch::new(source);
    let mut consumed = 0usize;
    let mut parsed = 0usize;
    let mut start = 0usize;
    for (i, &b) in buffer.iter().enumerate() {
        if b == b'\n' {
            if start < i {
                if let Some(line) = buffer.get(start..i) {
                    let payload = payload_for(line, container);
                    let format = detect_line(payload);
                    let outer = if container.is_some() {
                        Format::Jsonl
                    } else {
                        format
                    };
                    match outer {
                        Format::Syslog => syslog.parse_line(&mut batch, line),
                        Format::Clf => clf.parse_line(&mut batch, line),
                        Format::Jsonl => jsonl.parse_line(&mut batch, line),
                        Format::Logfmt => logfmt.parse_line(&mut batch, line),
                        Format::Freeform => {
                            let message = std::str::from_utf8(line).ok();
                            batch.push_line(line, None, None, None, message);
                        }
                    }
                    let row = batch.len().saturating_sub(1);
                    batch.fields.set("format", row, format.as_str());
                    parsed = parsed.saturating_add(1);
                }
            }
            consumed = i.saturating_add(1);
            start = consumed;
            if parsed >= max_lines {
                break;
            }
        }
    }
    (batch, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::batch::{FieldColumn, SourceKind};

    #[test]
    fn detect_line_classifies_each_format() {
        assert_eq!(detect_line(b"{\"a\":1}"), Format::Jsonl);
        assert_eq!(
            detect_line(b"<134>Sep  9 14:23:01 host app: hi"),
            Format::Syslog
        );
        assert_eq!(
            detect_line(b"10.0.0.1 - - [10/Sep/2026:08:00:05 +0200] \"GET / HTTP/1.1\" 200 12"),
            Format::Clf
        );
        assert_eq!(
            detect_line(b"level=info msg=\"started\" service=api"),
            Format::Logfmt
        );
        assert_eq!(detect_line(b"just some text"), Format::Freeform);
    }

    #[test]
    fn lock_on_decides_in_one_sample_for_a_pure_syslog_file() {
        let line = b"<134>Sep  9 14:23:01 host app: hello\n";
        let mut buf = Vec::new();
        for _ in 0..40 {
            buf.extend_from_slice(line);
        }
        let detection = detect(&buf, DEFAULT_SAMPLE_LINES);
        assert_eq!(detection.format, Format::Syslog);
        assert!(detection.locked);
        assert_eq!(detection.container, None);
    }

    #[test]
    fn mixed_file_falls_back_to_per_line_detection() {
        let mut buf = Vec::new();
        for i in 0..40 {
            if i % 2 == 0 {
                buf.extend_from_slice(b"<134>Sep  9 14:23:01 host app: hello\n");
            } else {
                buf.extend_from_slice(b"level=info msg=\"hi\" service=api\n");
            }
        }
        let detection = detect(&buf, DEFAULT_SAMPLE_LINES);
        assert!(!detection.locked);

        let source = Source::new(SourceKind::File, "mixed.log");
        let (batch, consumed, _) = parse_detected(source, &buf, 40, DEFAULT_SAMPLE_LINES);
        assert_eq!(consumed, buf.len());
        assert_eq!(batch.len(), 40);
        match batch.fields.get("format").expect("format column") {
            FieldColumn::Dict { dict, .. } => {
                assert!(dict.contains(&"syslog"));
                assert!(dict.contains(&"logfmt"));
            }
            other => panic!("expected dict-encoded format, got {other:?}"),
        }
    }

    #[test]
    fn docker_wrapped_syslog_file_reports_format_syslog_container_docker() {
        let line = b"{\"log\":\"<134>Sep  9 14:23:01 host app: hi\\n\",\"stream\":\"stdout\",\"time\":\"2026-09-10T08:00:05Z\"}\n";
        let mut buf = Vec::new();
        for _ in 0..40 {
            buf.extend_from_slice(line);
        }

        let detection = detect(&buf, DEFAULT_SAMPLE_LINES);
        assert!(detection.locked);
        assert_eq!(detection.format, Format::Syslog);
        assert_eq!(detection.container, Some("docker"));

        let source = Source::new(SourceKind::File, "docker.log");
        let (batch, consumed, detection) = parse_detected(source, &buf, 40, DEFAULT_SAMPLE_LINES);
        assert_eq!(consumed, buf.len());
        assert_eq!(batch.len(), 40);
        assert_eq!(detection.format, Format::Syslog);
        assert_eq!(detection.container, Some("docker"));

        match batch.fields.get("format").expect("format column") {
            FieldColumn::Dict { dict, .. } => assert_eq!(dict.as_slice(), ["syslog"]),
            other => panic!("expected dict-encoded format, got {other:?}"),
        }
        match batch.fields.get("container").expect("container column") {
            FieldColumn::Dict { dict, .. } => assert_eq!(dict.as_slice(), ["docker"]),
            other => panic!("expected dict-encoded container, got {other:?}"),
        }
    }

    #[test]
    fn jsonl_fixture_locks_on_and_parses_every_line() {
        let buf = std::fs::read("tests/fixtures/stream/jsonl-1k.log").expect("fixture readable");
        let source = Source::new(SourceKind::File, "jsonl-1k.log");
        let (batch, consumed, detection) = parse_detected(source, &buf, 2000, DEFAULT_SAMPLE_LINES);
        assert_eq!(consumed, buf.len());
        assert_eq!(batch.len(), 1000);
        assert_eq!(detection.format, Format::Jsonl);
        assert!(detection.locked);

        match batch.fields.get("res.statusCode").expect("flattened field") {
            FieldColumn::Int(v) => assert_eq!(v.len(), 1000),
            other => panic!("expected Int, got {other:?}"),
        }
    }

    #[test]
    fn logfmt_fixture_locks_on_and_parses_every_line() {
        let buf = std::fs::read("tests/fixtures/stream/logfmt-1k.log").expect("fixture readable");
        let source = Source::new(SourceKind::File, "logfmt-1k.log");
        let (batch, consumed, detection) = parse_detected(source, &buf, 2000, DEFAULT_SAMPLE_LINES);
        assert_eq!(consumed, buf.len());
        assert_eq!(batch.len(), 1000);
        assert_eq!(detection.format, Format::Logfmt);
        assert!(detection.locked);

        match batch.fields.get("status").expect("status field") {
            FieldColumn::Int(v) => assert_eq!(v.len(), 1000),
            other => panic!("expected Int, got {other:?}"),
        }
    }
}
