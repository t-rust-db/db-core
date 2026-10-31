//! JSON Lines parser (#319): Docker `json-file`, Kubernetes, Bunyan/Pino/
//! zerolog/slog/structlog, OTel file exporter. Schema-on-read stress test
//! for `FieldStore`.
//!
//! - Alias-table promotion to Tier-2 (`ts`/`level`/`msg`) and canonical
//!   Tier-3 names (`host`/`service`/`pid`) -- see [`super::alias`].
//! - Numeric Bunyan/Pino levels (`Severity::from_number`).
//! - Nesting flattens to dot paths one level deep (`req.method`); a value
//!   that is itself an object beyond that depth is dropped as a column (the
//!   full line stays available via Tier-1 `raw` for late `json_extract`,
//!   #307). Arrays are kept whole, as their raw JSON text (zero-copy).
//! - Docker `json-file` container unwrap: `{"log":"<payload>\n","stream":
//!   "stdout","time":"…"}`; `stream`/`container`/`container_time` land as
//!   Tier-3 fields and the payload becomes the row's message, left for
//!   per-line format re-detection (#321) rather than re-parsed here -- a
//!   JSON payload (Pino-in-Docker) arrives with its inner quotes escaped,
//!   and this parser never unescapes strings (Tier-3 values stay zero-copy
//!   slices into the source buffer).

use super::alias::{self, Promotion};
use super::batch::{LineParser, LogBatch, Severity, Source};
use super::json::{self, JsonValue};
use super::timestamp;

/// Maximum object nesting flattened into dot-path fields (one level below
/// the top-level object, e.g. `req.method`).
const MAX_FLATTEN_DEPTH: usize = 1;

/// JSON Lines parser that fills `LogBatch` from raw lines.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonlParser;

impl JsonlParser {
    /// Create a new parser.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Cheap format sniff: does this line look like a JSON object? (used by
    /// the format detector, #321, before committing to a full parse).
    #[must_use]
    pub fn looks_like_json_object(line: &[u8]) -> bool {
        line.iter()
            .find(|b| !b.is_ascii_whitespace())
            .is_some_and(|&b| b == b'{')
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

    /// Parse a single JSONL line into the batch.
    pub(crate) fn parse_line<'a>(&self, batch: &mut LogBatch<'a>, line: &'a [u8]) {
        let line_str = match std::str::from_utf8(line) {
            Ok(s) => s.trim_end_matches(['\r', '\n']),
            Err(_) => {
                batch.push_line(line, None, None, None, None);
                return;
            }
        };

        let Some((JsonValue::Object(fields), _)) = json::parse_object(line_str) else {
            // Not a JSON object (blank line, malformed, or a bare scalar):
            // preserve it as the message so the row still exists.
            let message = if line_str.is_empty() {
                None
            } else {
                Some(line_str)
            };
            batch.push_line(line, None, None, None, message);
            return;
        };

        if let Some(payload) = Self::docker_payload(&fields) {
            self.parse_docker_wrapped(batch, line, line_str, &fields, payload);
        } else {
            self.parse_object_fields(batch, line, &fields);
        }
    }

    /// Docker `json-file` shape: a top-level object carrying both `log`
    /// (the payload string) and `stream`.
    fn docker_payload<'a>(fields: &[(&'a str, JsonValue<'a>)]) -> Option<&'a str> {
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

    /// Handle a Docker `json-file` wrapper: unwrap the payload and record
    /// `stream`/`container`/`container_time` as Tier-3 fields. The payload
    /// itself is stored as the message and left for per-line format
    /// re-detection (#321) rather than re-parsed here -- a payload that is
    /// itself JSON (Pino-in-Docker) arrives with its inner quotes escaped
    /// (`\"`), and this parser deliberately never unescapes strings (Tier-3
    /// values stay zero-copy slices into the source buffer); unescaping
    /// would require an owned buffer that can't satisfy that lifetime.
    fn parse_docker_wrapped<'a>(
        &self,
        batch: &mut LogBatch<'a>,
        raw: &'a [u8],
        _line_str: &'a str,
        fields: &[(&'a str, JsonValue<'a>)],
        payload: &'a str,
    ) {
        // Strip a trailing newline: either a real one, or (since we don't
        // unescape) the literal two-char `\n` escape Docker writes for it.
        let payload = payload.trim_end_matches('\n').trim_end_matches("\\n");
        let stream = fields.iter().find_map(|(k, v)| {
            (*k == "stream").then_some(v).and_then(|v| {
                if let JsonValue::Str(s) = v {
                    Some(*s)
                } else {
                    None
                }
            })
        });
        let container_time = fields.iter().find_map(|(k, v)| {
            (*k == "time").then_some(v).and_then(|v| {
                if let JsonValue::Str(s) = v {
                    Some(*s)
                } else {
                    None
                }
            })
        });

        let message = if payload.is_empty() {
            None
        } else {
            Some(payload)
        };
        batch.push_line(raw, None, None, None, message);

        batch.set_field("container", "docker");
        if let Some(s) = stream {
            batch.set_field("stream", s);
        }
        if let Some(t) = container_time {
            batch.set_field("container_time", t);
        }
    }

    /// Promote alias-table fields to Tier-2/canonical Tier-3 columns, push
    /// the row, then flatten every remaining field.
    fn parse_object_fields<'a>(
        &self,
        batch: &mut LogBatch<'a>,
        raw: &'a [u8],
        fields: &[(&'a str, JsonValue<'a>)],
    ) {
        let mut ts: Option<i64> = None;
        let mut severity: Option<Severity> = None;
        let mut message: Option<&'a str> = None;

        for (key, value) in fields {
            match alias::classify(key) {
                Some(Promotion::Timestamp) => ts = ts.or_else(|| resolve_timestamp(value)),
                Some(Promotion::Level) => severity = severity.or_else(|| resolve_level(value)),
                Some(Promotion::Message) => {
                    if let JsonValue::Str(s) = value {
                        message = message.or(Some(*s));
                    }
                }
                _ => {}
            }
        }

        batch.push_line(raw, ts, severity, None, message);

        for (key, value) in fields {
            match alias::classify(key) {
                Some(Promotion::Timestamp | Promotion::Level | Promotion::Message) => {}
                Some(Promotion::Host) => Self::set_scalar(batch, "host", value),
                Some(Promotion::Service) => Self::set_scalar(batch, "service", value),
                Some(Promotion::Pid) => Self::set_scalar(batch, "pid", value),
                None => Self::set_field_value(batch, key, value, MAX_FLATTEN_DEPTH),
            }
        }
    }

    /// Set a canonical (alias-promoted) scalar Tier-3 field, typed by its
    /// JSON value kind.
    fn set_scalar<'a>(batch: &mut LogBatch<'a>, key: &str, value: &JsonValue<'a>) {
        match value {
            JsonValue::Str(s) => batch.set_field(key, s),
            JsonValue::Int(n) => batch.set_field_int(key, *n),
            JsonValue::Float(f) => batch.set_field_float(key, *f),
            JsonValue::Bool(b) => batch.set_field_bool(key, *b),
            JsonValue::Array(raw) => batch.set_field(key, raw),
            JsonValue::Null | JsonValue::Object(_) => {}
        }
    }

    /// Set a field value, flattening nested objects to `parent.child` dot
    /// paths while `depth_budget` allows. An object beyond the budget is
    /// dropped as a column (the raw line still carries the data).
    fn set_field_value<'a>(
        batch: &mut LogBatch<'a>,
        key: &str,
        value: &JsonValue<'a>,
        depth_budget: usize,
    ) {
        match value {
            JsonValue::Str(s) => batch.set_field(key, s),
            JsonValue::Int(n) => batch.set_field_int(key, *n),
            JsonValue::Float(f) => batch.set_field_float(key, *f),
            JsonValue::Bool(b) => batch.set_field_bool(key, *b),
            JsonValue::Array(raw) => batch.set_field(key, raw),
            JsonValue::Null => {}
            JsonValue::Object(inner) => {
                if depth_budget == 0 {
                    return;
                }
                for (inner_key, inner_value) in inner {
                    let flat_key = format!("{key}.{inner_key}");
                    Self::set_field_value(
                        batch,
                        &flat_key,
                        inner_value,
                        depth_budget.saturating_sub(1),
                    );
                }
            }
        }
    }
}

impl LineParser for JsonlParser {
    fn parse_batch<'a>(
        &self,
        source: Source,
        buffer: &'a [u8],
        max_lines: usize,
    ) -> (LogBatch<'a>, usize) {
        self.parse_batch(source, buffer, max_lines)
    }
}

/// Resolve a Tier-2 timestamp from a `ts`-alias JSON value: ISO 8601
/// strings, or epoch numbers (int/float seconds/ms/us/ns by digit count).
fn resolve_timestamp(value: &JsonValue<'_>) -> Option<i64> {
    match value {
        JsonValue::Str(s) => timestamp::parse_flexible(s),
        JsonValue::Int(n) => timestamp::parse_epoch(&n.to_string()),
        JsonValue::Float(f) => float_epoch(*f),
        _ => None,
    }
}

/// A fractional epoch number: the whole part picks the unit by digit
/// count exactly as an integer epoch does, and the fraction keeps its
/// sub-unit digits (#882: `1700000000.789` lost its `.789`). The digits
/// come from Rust's shortest round-trip rendering of `f`, which is what
/// the line said, not the binary approximation's tail (`.78900003...`);
/// digits finer than a nanosecond are dropped.
fn float_epoch(f: f64) -> Option<i64> {
    let text = f.to_string();
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let unit = timestamp::epoch_unit_nanos(whole.trim_start_matches('-').len())?;
    let unit_digits = usize::try_from(unit.ilog10()).ok()?;
    let fraction: String = fraction
        .chars()
        .chain(std::iter::repeat('0'))
        .take(unit_digits)
        .collect();
    let fraction_nanos: i64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().ok()?
    };
    let base = timestamp::parse_epoch(whole)?;
    if whole.starts_with('-') {
        base.checked_sub(fraction_nanos)
    } else {
        base.checked_add(fraction_nanos)
    }
}

/// Resolve a Tier-2 severity from a `level`-alias JSON value: strings via
/// [`Severity::parse`], numbers via the Bunyan/Pino scale.
fn resolve_level(value: &JsonValue<'_>) -> Option<Severity> {
    match value {
        JsonValue::Str(s) => alias::parse_level(s),
        JsonValue::Int(n) => Severity::from_number(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::batch::{FieldColumn, SourceKind};

    fn parse_one(line: &'static str) -> LogBatch<'static> {
        let parser = JsonlParser::new();
        let source = Source::new(SourceKind::File, "test.jsonl");
        let input = line.as_bytes();
        let (batch, consumed) = parser.parse_batch(source, input, 10);
        assert_eq!(consumed, input.len());
        batch
    }

    /// #656: a line nested past `json_path::MAX_DEPTH` is not parsed as
    /// an object -- it is kept whole as the message, as any malformed line
    /// is -- instead of overflowing the writer's stack.
    #[test]
    fn a_too_deeply_nested_line_is_kept_as_its_message() {
        let deep = format!("{}1{}", "{\"a\":".repeat(100_000), "}".repeat(100_000));
        // `parse_one` takes a `'static` line; a test may leak one.
        let line: &'static str = Box::leak(format!("{deep}\n").into_boxed_str());
        let batch = parse_one(line);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.message[0], Some(deep.as_str()));
    }

    /// #882: a fractional epoch kept only its whole part.
    #[test]
    fn a_fractional_epoch_keeps_its_fraction() {
        let ts = |line: &'static str| parse_one(line).timestamp_ns[0];
        assert_eq!(
            ts("{\"ts\":1700000000.789,\"msg\":\"x\"}\n"),
            Some(1_700_000_000_789_000_000)
        );
        assert_eq!(
            ts("{\"ts\":1700000000123.5,\"msg\":\"x\"}\n"),
            Some(1_700_000_000_123_500_000)
        );
        assert_eq!(ts("{\"ts\":12.5,\"msg\":\"x\"}\n"), None);
    }

    #[test]
    fn pino_shaped_line_promotes_ts_level_msg() {
        let batch = parse_one(
            "{\"level\":30,\"time\":1700000000000,\"msg\":\"hello\",\"req\":{\"method\":\"GET\",\"url\":\"/x\"}}\n",
        );
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.severity[0], Some(Severity::Info));
        assert!(batch.timestamp_ns[0].is_some());
        assert_eq!(batch.message[0], Some("hello"));

        match batch.fields.get("req.method").expect("flattened field") {
            FieldColumn::Dict { dict, indices } => {
                assert_eq!(dict[indices[0].expect("value") as usize], "GET");
            }
            other => panic!("expected dict-encoded str, got {other:?}"),
        }
    }

    #[test]
    fn alias_promotion_from_alternate_keys() {
        let batch = parse_one(
            "{\"timestamp\":\"2026-09-10T08:00:05Z\",\"severity\":\"warn\",\"message\":\"slow request\",\"hostname\":\"web-1\"}\n",
        );
        assert_eq!(batch.severity[0], Some(Severity::Warn));
        assert_eq!(batch.message[0], Some("slow request"));
        assert!(batch.fields.get("host").is_some());
    }

    #[test]
    fn nested_array_kept_as_raw_string() {
        let batch = parse_one("{\"msg\":\"x\",\"tags\":[1,2,3]}\n");
        match batch.fields.get("tags").expect("tags field") {
            FieldColumn::Dict { dict, indices } => {
                assert_eq!(dict[indices[0].expect("value") as usize], "[1,2,3]");
            }
            other => panic!("expected dict-encoded str, got {other:?}"),
        }
    }

    #[test]
    fn deep_nesting_beyond_budget_is_dropped_not_panicking() {
        let batch = parse_one("{\"a\":{\"b\":{\"c\":1}}}\n");
        assert_eq!(batch.len(), 1);
        assert!(batch.fields.get("a.b").is_none());
    }

    #[test]
    fn docker_wrapped_syslog_payload_stored_as_message() {
        let batch = parse_one(
            "{\"log\":\"<134>Sep  9 14:23:01 host app: hi\\n\",\"stream\":\"stdout\",\"time\":\"2026-09-10T08:00:05Z\"}\n",
        );
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.message[0], Some("<134>Sep  9 14:23:01 host app: hi"));
    }

    #[test]
    fn non_json_line_falls_back_to_raw() {
        let batch = parse_one("not json at all\n");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.message[0], Some("not json at all"));
    }

    #[test]
    fn detects_json_object_lines() {
        assert!(JsonlParser::looks_like_json_object(b"  {\"a\":1}"));
        assert!(!JsonlParser::looks_like_json_object(b"<134>syslog line"));
    }
}
