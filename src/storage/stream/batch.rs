//! `LogBatch`: columnar storage for parsed log lines.
//!
//! Three-tier design:
//! 1. Raw line (`&[u8]`, zero-copy from mmap)
//! 2. Predefined typed columns (timestamp, severity, message, trace/span IDs)
//! 3. Open fields with dictionary encoding for low-cardinality strings

use std::collections::HashMap;
use std::sync::Arc;

/// Default batch size (rows per batch).
pub const BATCH_SIZE: usize = 256;

/// Syslog facility (RFC 3164/5424).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Facility {
    /// Kernel messages.
    Kern = 0,
    /// User-level messages.
    User = 1,
    /// Mail system.
    Mail = 2,
    /// System daemons.
    Daemon = 3,
    /// Security/authorization.
    Auth = 4,
    /// Syslog internal.
    Syslog = 5,
    /// Line printer.
    Lpr = 6,
    /// Network news.
    News = 7,
    /// UUCP.
    Uucp = 8,
    /// Clock daemon.
    Cron = 9,
    /// Security/authorization (private).
    AuthPriv = 10,
    /// FTP daemon.
    Ftp = 11,
    /// Local use 0.
    Local0 = 16,
    /// Local use 1.
    Local1 = 17,
    /// Local use 2.
    Local2 = 18,
    /// Local use 3.
    Local3 = 19,
    /// Local use 4.
    Local4 = 20,
    /// Local use 5.
    Local5 = 21,
    /// Local use 6.
    Local6 = 22,
    /// Local use 7.
    Local7 = 23,
}

impl Facility {
    /// Parse from syslog facility code (0-23).
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Kern),
            1 => Some(Self::User),
            2 => Some(Self::Mail),
            3 => Some(Self::Daemon),
            4 => Some(Self::Auth),
            5 => Some(Self::Syslog),
            6 => Some(Self::Lpr),
            7 => Some(Self::News),
            8 => Some(Self::Uucp),
            9 => Some(Self::Cron),
            10 => Some(Self::AuthPriv),
            11 => Some(Self::Ftp),
            16 => Some(Self::Local0),
            17 => Some(Self::Local1),
            18 => Some(Self::Local2),
            19 => Some(Self::Local3),
            20 => Some(Self::Local4),
            21 => Some(Self::Local5),
            22 => Some(Self::Local6),
            23 => Some(Self::Local7),
            _ => None,
        }
    }

    /// Name for display.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kern => "kern",
            Self::User => "user",
            Self::Mail => "mail",
            Self::Daemon => "daemon",
            Self::Auth => "auth",
            Self::Syslog => "syslog",
            Self::Lpr => "lpr",
            Self::News => "news",
            Self::Uucp => "uucp",
            Self::Cron => "cron",
            Self::AuthPriv => "authpriv",
            Self::Ftp => "ftp",
            Self::Local0 => "local0",
            Self::Local1 => "local1",
            Self::Local2 => "local2",
            Self::Local3 => "local3",
            Self::Local4 => "local4",
            Self::Local5 => "local5",
            Self::Local6 => "local6",
            Self::Local7 => "local7",
        }
    }
}

/// Log severity levels, aligned with OpenTelemetry SeverityNumber (1-24 scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Severity {
    /// TRACE level (OTel 1-4).
    Trace = 1,
    /// DEBUG level (OTel 5-8).
    Debug = 5,
    /// INFO level (OTel 9-12).
    Info = 9,
    /// WARN level (OTel 13-16).
    Warn = 13,
    /// ERROR level (OTel 17-20).
    Error = 17,
    /// FATAL/CRITICAL level (OTel 21-24).
    Fatal = 21,
}

impl Severity {
    /// Parse a severity string (case-insensitive, with aliases).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "trace" | "trc" | "verbose" => Some(Self::Trace),
            "debug" | "dbg" => Some(Self::Debug),
            "info" | "information" | "notice" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" | "err" | "severe" => Some(Self::Error),
            "fatal" | "critical" | "crit" | "panic" | "emerg" | "alert" => Some(Self::Fatal),
            _ => None,
        }
    }

    /// Parse from syslog severity (0-7, where 0 is most severe).
    #[must_use]
    pub fn from_syslog(severity: u8) -> Option<Self> {
        match severity {
            0 | 1 => Some(Self::Fatal), // emerg, alert
            2 => Some(Self::Fatal),     // crit
            3 => Some(Self::Error),     // err
            4 => Some(Self::Warn),      // warning
            5 => Some(Self::Info),      // notice
            6 => Some(Self::Info),      // info
            7 => Some(Self::Debug),     // debug
            _ => None,
        }
    }

    /// Name for display.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }
}

/// Log source type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// Local file.
    File,
    /// Docker container logs.
    Docker,
    /// systemd journal.
    Journal,
    /// Kubernetes pod logs.
    Kubernetes,
    /// Standard input.
    Stdin,
}

/// Log source metadata (batch-level, constant for all rows).
#[derive(Debug, Clone)]
pub struct Source {
    /// Source type.
    pub kind: SourceKind,
    /// Path or identifier for display.
    pub path: Arc<str>,
    /// Precomputed hash for fast filtering.
    pub path_hash: u64,
}

impl Source {
    /// Create a new source with precomputed hash.
    #[must_use]
    pub fn new(kind: SourceKind, path: &str) -> Self {
        Self {
            kind,
            path: Arc::from(path),
            path_hash: Self::hash_str(path),
        }
    }

    /// Simple FNV-1a hash for strings.
    fn hash_str(s: &str) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in s.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}

/// Resource metadata (system that produced the logs, batch-level).
#[derive(Debug, Clone, Default)]
pub struct Resource {
    /// Hostname.
    pub hostname: Option<Arc<str>>,
    /// Hostname hash for fast filtering.
    pub hostname_hash: u64,
    /// Service/application name.
    pub service: Option<Arc<str>>,
    /// Service hash for fast filtering.
    pub service_hash: u64,
    /// Environment (prod/staging/dev).
    pub environment: Option<Arc<str>>,
}

impl Resource {
    /// Create with hostname.
    #[must_use]
    pub fn with_hostname(hostname: &str) -> Self {
        Self {
            hostname: Some(Arc::from(hostname)),
            hostname_hash: Source::hash_str(hostname),
            ..Default::default()
        }
    }
}

/// A columnar batch of parsed log lines.
#[derive(Debug)]
pub struct LogBatch<'a> {
    /// Number of rows in this batch.
    len: usize,

    // === Batch-level metadata ===
    /// Log source (file path, container, etc.).
    pub source: Source,
    /// Resource metadata (hostname, service).
    pub resource: Resource,

    // === Tier 1: Raw lines ===
    /// Raw line bytes, zero-copy slices into source buffer.
    pub raw: Vec<&'a [u8]>,

    // === Tier 2: Predefined typed columns ===
    /// Timestamp in nanoseconds since Unix epoch.
    pub timestamp_ns: Vec<Option<i64>>,
    /// Log severity.
    pub severity: Vec<Option<Severity>>,
    /// Extracted message body (may differ from raw if structured).
    pub message: Vec<Option<&'a str>>,

    // === Tier 2b: Format-specific typed ===
    /// Syslog facility (syslog only).
    pub facility: Vec<Option<Facility>>,

    // === Tier 2c: Correlation IDs (fixed-size) ===
    /// W3C trace ID (128-bit).
    pub trace_id: Vec<Option<[u8; 16]>>,
    /// W3C span ID (64-bit).
    pub span_id: Vec<Option<[u8; 8]>>,

    // === Tier 3: Open fields ===
    /// Dynamic fields with dictionary encoding.
    pub fields: FieldStore<'a>,
}

impl<'a> LogBatch<'a> {
    /// Create an empty batch with the given source.
    #[must_use]
    pub fn new(source: Source) -> Self {
        Self {
            len: 0,
            source,
            resource: Resource::default(),
            raw: Vec::with_capacity(BATCH_SIZE),
            timestamp_ns: Vec::with_capacity(BATCH_SIZE),
            severity: Vec::with_capacity(BATCH_SIZE),
            message: Vec::with_capacity(BATCH_SIZE),
            facility: Vec::with_capacity(BATCH_SIZE),
            trace_id: Vec::with_capacity(BATCH_SIZE),
            span_id: Vec::with_capacity(BATCH_SIZE),
            fields: FieldStore::new(),
        }
    }

    /// Number of rows in this batch.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the batch is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the batch is full.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len >= BATCH_SIZE
    }

    /// Push a raw line with Tier 2 fields.
    pub fn push_line(
        &mut self,
        raw: &'a [u8],
        timestamp_ns: Option<i64>,
        severity: Option<Severity>,
        facility: Option<Facility>,
        message: Option<&'a str>,
    ) {
        self.raw.push(raw);
        self.timestamp_ns.push(timestamp_ns);
        self.severity.push(severity);
        self.facility.push(facility);
        self.message.push(message);
        self.trace_id.push(None);
        self.span_id.push(None);
        self.len = self.len.saturating_add(1);
    }

    /// Set a dynamic field value for the current row.
    pub fn set_field(&mut self, name: &str, value: &'a str) {
        self.fields.set(name, self.len.saturating_sub(1), value);
    }

    /// Get the raw line at index (returns empty slice if out of bounds).
    #[must_use]
    pub fn raw_line(&self, index: usize) -> &'a [u8] {
        self.raw.get(index).copied().unwrap_or(b"")
    }

    /// Clear the batch for reuse.
    pub fn clear(&mut self) {
        self.len = 0;
        self.raw.clear();
        self.timestamp_ns.clear();
        self.severity.clear();
        self.facility.clear();
        self.message.clear();
        self.trace_id.clear();
        self.span_id.clear();
        self.fields.clear();
    }
}

/// Dictionary-encoded or raw string column.
#[derive(Debug, Clone)]
pub enum FieldColumn<'a> {
    /// High-cardinality: raw string references.
    Str(Vec<Option<&'a str>>),
    /// Low-cardinality: dictionary-encoded (index into dict).
    Dict {
        /// Unique values.
        dict: Vec<&'a str>,
        /// Index per row (None = null, Some(i) = dict[i]).
        indices: Vec<Option<u16>>,
    },
    /// Integer values.
    Int(Vec<Option<i64>>),
    /// Floating-point values.
    Float(Vec<Option<f64>>),
    /// Boolean values.
    Bool(Vec<Option<bool>>),
}

/// Threshold for switching from Dict to Str encoding.
const DICT_CARDINALITY_THRESHOLD: usize = 256;

impl<'a> FieldColumn<'a> {
    /// Create a new string column (starts as Dict, degrades to Str).
    fn new_str(capacity: usize) -> Self {
        Self::Dict {
            dict: Vec::new(),
            indices: Vec::with_capacity(capacity),
        }
    }

    /// Push a string value, handling Dict→Str degradation.
    fn push_str(&mut self, value: Option<&'a str>) {
        match self {
            Self::Dict { dict, indices } => {
                if let Some(v) = value {
                    // Look up or insert into dictionary
                    let idx = dict.iter().position(|&d| d == v);
                    let idx = match idx {
                        Some(i) => i,
                        None => {
                            if dict.len() >= DICT_CARDINALITY_THRESHOLD {
                                // Degrade to Str
                                let mut strs: Vec<Option<&'a str>> =
                                    Vec::with_capacity(indices.len().saturating_add(1));
                                for opt_idx in indices.iter() {
                                    strs.push(
                                        opt_idx.and_then(|i| dict.get(usize::from(i)).copied()),
                                    );
                                }
                                strs.push(Some(v));
                                *self = Self::Str(strs);
                                return;
                            }
                            dict.push(v);
                            dict.len().saturating_sub(1)
                        }
                    };
                    // Safe: idx < DICT_CARDINALITY_THRESHOLD < u16::MAX
                    #[allow(clippy::cast_possible_truncation)]
                    indices.push(Some(idx as u16));
                } else {
                    indices.push(None);
                }
            }
            Self::Str(strs) => strs.push(value),
            _ => {} // Type mismatch, ignore
        }
    }

    /// Number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Str(v) => v.len(),
            Self::Dict { indices, .. } => indices.len(),
            Self::Int(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Bool(v) => v.len(),
        }
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear for reuse.
    fn clear(&mut self) {
        match self {
            Self::Str(v) => v.clear(),
            Self::Dict { dict, indices } => {
                dict.clear();
                indices.clear();
            }
            Self::Int(v) => v.clear(),
            Self::Float(v) => v.clear(),
            Self::Bool(v) => v.clear(),
        }
    }
}

/// Storage for dynamic fields (Tier 3).
#[derive(Debug, Default)]
pub struct FieldStore<'a> {
    /// Field name → column, with interned names.
    columns: HashMap<Arc<str>, FieldColumn<'a>>,
    /// Insertion order for deterministic iteration.
    order: Vec<Arc<str>>,
}

impl<'a> FieldStore<'a> {
    /// Create an empty field store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a field value at the given row index.
    pub fn set(&mut self, name: &str, row: usize, value: &'a str) {
        let key = self
            .order
            .iter()
            .find(|k| k.as_ref() == name)
            .cloned()
            .unwrap_or_else(|| {
                let key = Arc::<str>::from(name);
                self.order.push(Arc::clone(&key));
                key
            });

        let col = self
            .columns
            .entry(Arc::clone(&key))
            .or_insert_with(|| FieldColumn::new_str(BATCH_SIZE));

        // Pad with nulls if needed
        while col.len() < row {
            col.push_str(None);
        }
        col.push_str(Some(value));
    }

    /// Get a field column by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&FieldColumn<'a>> {
        self.columns.get(name)
    }

    /// Iterate over field names in insertion order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.order.iter().map(|s| s.as_ref())
    }

    /// Clear all fields.
    pub fn clear(&mut self) {
        for col in self.columns.values_mut() {
            col.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_parse() {
        assert_eq!(Severity::parse("ERROR"), Some(Severity::Error));
        assert_eq!(Severity::parse("warn"), Some(Severity::Warn));
        assert_eq!(Severity::parse("CRITICAL"), Some(Severity::Fatal));
        assert_eq!(Severity::parse("unknown"), None);
    }

    #[test]
    fn severity_from_syslog() {
        assert_eq!(Severity::from_syslog(0), Some(Severity::Fatal)); // emerg
        assert_eq!(Severity::from_syslog(3), Some(Severity::Error)); // err
        assert_eq!(Severity::from_syslog(4), Some(Severity::Warn)); // warning
        assert_eq!(Severity::from_syslog(6), Some(Severity::Info)); // info
        assert_eq!(Severity::from_syslog(7), Some(Severity::Debug)); // debug
    }

    #[test]
    fn batch_push_and_access() {
        let source = Source::new(SourceKind::File, "/var/log/test.log");
        let mut batch = LogBatch::new(source);

        let line = b"test line";
        batch.push_line(
            line,
            Some(1_234_567_890_000_000_000),
            Some(Severity::Info),
            Some(Facility::Daemon),
            Some("test line"),
        );

        assert_eq!(batch.len(), 1);
        assert!(!batch.is_empty());
        assert!(!batch.is_full());
        assert_eq!(batch.raw_line(0), b"test line");
    }

    #[test]
    fn field_store_dict_encoding() {
        let mut store = FieldStore::new();

        // Same value repeated should use dict encoding
        store.set("service", 0, "nginx");
        store.set("service", 1, "nginx");
        store.set("service", 2, "postgres");

        let col = store.get("service").unwrap();
        match col {
            FieldColumn::Dict { dict, indices } => {
                assert_eq!(dict.len(), 2); // nginx, postgres
                assert_eq!(indices.len(), 3);
            }
            _ => panic!("Expected Dict encoding"),
        }
    }
}
