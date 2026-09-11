//! Shared Tier-2 promotion tables for structured formats (JSONL, logfmt).
//!
//! Both formats promote well-known keys to predefined `LogBatch` columns via
//! the same alias tables (ADR-0018, #319/#320): `ts`, `level`, `msg`, `host`,
//! `service`, `pid`. Ported from LAB271 loglume `format/json.rs`.

use super::batch::Severity;

/// Alias set for the event timestamp field.
pub const TS_ALIASES: &[&str] = &["ts", "time", "timestamp", "@timestamp", "date", "datetime"];

/// Alias set for the severity/level field.
pub const LEVEL_ALIASES: &[&str] = &["level", "severity", "lvl", "log_level", "loglevel"];

/// Alias set for the message field.
pub const MESSAGE_ALIASES: &[&str] = &["msg", "message", "text", "body"];

/// Alias set for the hostname field.
pub const HOST_ALIASES: &[&str] = &["host", "hostname", "server"];

/// Alias set for the service/application field.
pub const SERVICE_ALIASES: &[&str] = &["service", "app", "application", "name", "program"];

/// Alias set for the process-id field.
pub const PID_ALIASES: &[&str] = &["pid", "process_id"];

/// Which Tier-2 slot a key promotes to, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    /// Event timestamp.
    Timestamp,
    /// Severity/level.
    Level,
    /// Message body.
    Message,
    /// Hostname (Tier-3 `host`, kept as an open field -- no predefined
    /// hostname column on `LogBatch` today).
    Host,
    /// Service/application name (Tier-3 `service`).
    Service,
    /// Process id (Tier-3 `pid`).
    Pid,
}

/// Classify a key (case-insensitive) against the alias tables. Returns
/// `None` for keys that stay as ordinary Tier-3 fields.
#[must_use]
pub fn classify(key: &str) -> Option<Promotion> {
    if TS_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(key)) {
        Some(Promotion::Timestamp)
    } else if LEVEL_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(key)) {
        Some(Promotion::Level)
    } else if MESSAGE_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(key)) {
        Some(Promotion::Message)
    } else if HOST_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(key)) {
        Some(Promotion::Host)
    } else if SERVICE_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(key)) {
        Some(Promotion::Service)
    } else if PID_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(key)) {
        Some(Promotion::Pid)
    } else {
        None
    }
}

/// Parse a level value that may be a string (`"warn"`) or a Bunyan/Pino
/// numeric level (`30`). Tried as a number first (cheap, no allocation).
#[must_use]
pub fn parse_level(value: &str) -> Option<Severity> {
    value
        .parse::<i64>()
        .ok()
        .and_then(Severity::from_number)
        .or_else(|| Severity::parse(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_aliases_case_insensitively() {
        assert_eq!(classify("TIMESTAMP"), Some(Promotion::Timestamp));
        assert_eq!(classify("severity"), Some(Promotion::Level));
        assert_eq!(classify("Message"), Some(Promotion::Message));
        assert_eq!(classify("hostname"), Some(Promotion::Host));
        assert_eq!(classify("app"), Some(Promotion::Service));
        assert_eq!(classify("process_id"), Some(Promotion::Pid));
        assert_eq!(classify("custom_field"), None);
    }

    #[test]
    fn parses_numeric_and_string_levels() {
        assert_eq!(parse_level("30"), Some(Severity::Info));
        assert_eq!(parse_level("60"), Some(Severity::Fatal));
        assert_eq!(parse_level("warn"), Some(Severity::Warn));
        assert_eq!(parse_level("nonsense"), None);
    }
}
