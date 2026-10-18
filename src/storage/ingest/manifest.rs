//! `manifest.json` (ADR 0027): the table's only commit point.
//!
//! It lists the live Parquet files, each with the LSN range it holds, and
//! `flushed_lsn`; a file not listed does not exist for readers. Replacing
//! it is write `manifest.json.tmp`, fsync, rename over `manifest.json`,
//! fsync the directory, so a reader sees the old or the new manifest,
//! never a mix.
//!
//! **Invariant:** file LSN ranges are disjoint, in order, and together
//! exactly `[dropped_lsn + 1, flushed_lsn]`. [`Manifest::check`] enforces
//! it on read.
//!
//! The format is JSON written and read here (db-core has no serde): one
//! object with `version`, `format`, `layout`, `flushed_lsn` and `files`,
//! each file with its `area` (ADR 0028). Numbers are integers; strings are
//! the table's own generated names and never need escaping (a backslash in
//! one is refused). Version 1 (#572, before layouts) reads as a flat table.
//!
//! A record table (#594) adds `schema`, a list of `{name, type, nullable}`
//! (column names are refused at creation if they would need escaping), and
//! is written as version 3, so a reader that predates record tables
//! refuses it rather than reading it as a lines table. A lines table is
//! still written as version 2.
//!
//! A tiered table whose retention dropped its oldest days (ADR 0028)
//! records `dropped_lsn`, written only when it is not 0: a reader that
//! predates retention then refuses the manifest (its files no longer start
//! at LSN 1) rather than reading it.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use super::layout::{Area, LayoutKind};
use super::record::{type_from_name, type_name};
use crate::json_path::{parse_object, JsonValue};
use crate::storage::parquet::schema::ColumnSpec;
use crate::storage::stream::detect::Format;

/// The manifest's file name inside the table directory.
pub const MANIFEST_FILE: &str = "manifest.json";
const MANIFEST_TMP: &str = "manifest.json.tmp";
const VERSION: i64 = 2;
/// The version of a record table's manifest.
const RECORDS_VERSION: i64 = 3;

/// One Parquet file in the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the table directory.
    pub path: String,
    /// The area it is in.
    pub area: Area,
    /// First LSN in the file.
    pub lsn_lo: u64,
    /// Last LSN in the file.
    pub lsn_hi: u64,
    /// Rows (one per LSN).
    pub rows: u64,
    /// File size in bytes.
    pub bytes: u64,
    /// Smallest and largest `observed_ts` (ns), if any row has one.
    pub observed_ts: Option<(i64, i64)>,
    /// Smallest and largest `timestamp` (ns), if any row has one.
    pub timestamp: Option<(i64, i64)>,
}

/// The table's committed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The record format every line is parsed with.
    pub format: Format,
    /// Where files go.
    pub layout: LayoutKind,
    /// A record table's schema (#594); `None` for a lines table.
    pub schema: Option<Vec<ColumnSpec>>,
    /// Every LSN up to this one is in `files` or was dropped.
    pub flushed_lsn: u64,
    /// Every LSN up to this one was dropped by retention (0: none was).
    pub dropped_lsn: u64,
    /// The live files, in LSN order.
    pub files: Vec<FileEntry>,
}

/// Why a manifest could not be read or is not valid.
#[derive(Debug)]
pub enum ManifestError {
    /// Reading or writing failed.
    Io(io::Error),
    /// The text is not a manifest this version understands.
    Malformed(String),
    /// The file LSN ranges break the invariant.
    Invariant(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Io(e) => write!(f, "manifest: {e}"),
            ManifestError::Malformed(why) => write!(f, "manifest: malformed: {why}"),
            ManifestError::Invariant(why) => write!(f, "manifest: invariant broken: {why}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<io::Error> for ManifestError {
    fn from(e: io::Error) -> Self {
        ManifestError::Io(e)
    }
}

fn malformed(why: &str) -> ManifestError {
    ManifestError::Malformed(why.to_string())
}

/// The format a manifest name stands for.
pub fn format_from_name(name: &str) -> Option<Format> {
    [
        Format::Jsonl,
        Format::Logfmt,
        Format::Syslog,
        Format::Clf,
        Format::Freeform,
    ]
    .into_iter()
    .find(|f| f.as_str() == name)
}

impl Manifest {
    /// An empty table in `format` with `layout`.
    pub fn new(format: Format, layout: LayoutKind) -> Self {
        Manifest {
            format,
            layout,
            schema: None,
            flushed_lsn: 0,
            dropped_lsn: 0,
            files: Vec::new(),
        }
    }

    /// Checks the invariant: ranges in order, each `lsn_lo <= lsn_hi` with
    /// `rows` equal to its length, the first starting right after
    /// `dropped_lsn` (at 1 if nothing was dropped), each next one
    /// right after the previous, the last ending at `flushed_lsn`. A flat
    /// table's files are all in `data/`; a tiered table's in `archive/`,
    /// then `live/`, then `staging/` (closed days come before open ones,
    /// and only the open hour spills).
    pub fn check(&self) -> Result<(), ManifestError> {
        if self.dropped_lsn > self.flushed_lsn {
            return Err(ManifestError::Invariant(format!(
                "dropped_lsn {} is past flushed_lsn {}",
                self.dropped_lsn, self.flushed_lsn
            )));
        }
        let mut next = self.dropped_lsn.saturating_add(1);
        // The furthest tier seen: archive, live, staging.
        let mut tier = 0u8;
        for file in &self.files {
            let allowed = match (self.layout, file.area) {
                (LayoutKind::Flat, Area::Data) => true,
                (LayoutKind::Tiered, area @ (Area::Archive | Area::Live | Area::Staging)) => {
                    let rank = match area {
                        Area::Archive => 0,
                        Area::Live => 1,
                        _ => 2,
                    };
                    let in_order = rank >= tier;
                    tier = rank;
                    in_order
                }
                _ => false,
            };
            if !allowed {
                return Err(ManifestError::Invariant(format!(
                    "{} is in {}/, out of place in a {} table",
                    file.path,
                    file.area.as_str(),
                    self.layout.as_str()
                )));
            }
            if file.lsn_lo != next {
                return Err(ManifestError::Invariant(format!(
                    "{} starts at LSN {}, expected {next}",
                    file.path, file.lsn_lo
                )));
            }
            let span = file
                .lsn_hi
                .checked_sub(file.lsn_lo)
                .and_then(|d| d.checked_add(1));
            if span != Some(file.rows) {
                return Err(ManifestError::Invariant(format!(
                    "{} holds LSNs {}..={} but {} rows",
                    file.path, file.lsn_lo, file.lsn_hi, file.rows
                )));
            }
            next = file.lsn_hi.saturating_add(1);
        }
        let covered = next.saturating_sub(1);
        if covered != self.flushed_lsn {
            return Err(ManifestError::Invariant(format!(
                "files cover LSNs up to {covered}, flushed_lsn is {}",
                self.flushed_lsn
            )));
        }
        Ok(())
    }

    /// The manifest with `file` appended and `flushed_lsn` moved to its end.
    pub fn with_file(&self, file: FileEntry) -> Manifest {
        let mut next = self.clone();
        next.flushed_lsn = file.lsn_hi;
        next.files.push(file);
        next
    }

    /// The manifest with `files[replaced]` swapped for `outputs` (a
    /// compaction's commit); `flushed_lsn` stays.
    pub fn replacing(&self, replaced: std::ops::Range<usize>, outputs: Vec<FileEntry>) -> Manifest {
        let mut next = self.clone();
        next.files.splice(replaced, outputs);
        next
    }

    /// The manifest without its first `count` files (retention's commit):
    /// `dropped_lsn` moves to the last one's end; `flushed_lsn` stays.
    pub fn dropping(&self, count: usize) -> Manifest {
        let mut next = self.clone();
        let dropped: Vec<FileEntry> = next.files.drain(..count.min(next.files.len())).collect();
        if let Some(last) = dropped.last() {
            next.dropped_lsn = last.lsn_hi;
        }
        next
    }

    /// The JSON text.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        let version = if self.schema.is_some() {
            RECORDS_VERSION
        } else {
            VERSION
        };
        out.push_str(&format!(
            "{{\"version\":{version},\"format\":\"{}\",\"layout\":\"{}\",",
            self.format.as_str(),
            self.layout.as_str(),
        ));
        if let Some(schema) = &self.schema {
            out.push_str("\"schema\":[");
            for (i, spec) in schema.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    "{{\"name\":\"{}\",\"type\":\"{}\",\"nullable\":{}}}",
                    spec.name,
                    type_name(spec.column_type),
                    spec.nullable
                ));
            }
            out.push_str("],");
        }
        if self.dropped_lsn > 0 {
            out.push_str(&format!("\"dropped_lsn\":{},", self.dropped_lsn));
        }
        out.push_str(&format!("\"flushed_lsn\":{},\"files\":[", self.flushed_lsn));
        for (i, file) in self.files.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n{{\"path\":\"{}\",\"area\":\"{}\",\"lsn_lo\":{},\"lsn_hi\":{},\"rows\":{},\"bytes\":{}",
                file.path,
                file.area.as_str(),
                file.lsn_lo,
                file.lsn_hi,
                file.rows,
                file.bytes
            ));
            write_range(&mut out, "observed_ts", file.observed_ts);
            write_range(&mut out, "timestamp", file.timestamp);
            out.push('}');
        }
        out.push_str("]}\n");
        out
    }

    /// Parses and checks manifest text.
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let (root, rest) = parse_object(text).ok_or_else(|| malformed("not a JSON object"))?;
        if !rest.trim().is_empty() {
            return Err(malformed("text after the object"));
        }
        let fields = owned(&root)?.into_object()?;
        let version = int(&fields, "version")?;
        if !(1..=RECORDS_VERSION).contains(&version) {
            return Err(malformed("unsupported version"));
        }
        let format = format_from_name(&string(&fields, "format")?)
            .ok_or_else(|| malformed("unknown format"))?;
        // Version 1 had neither layouts nor areas: a flat table.
        let layout = if version == 1 {
            LayoutKind::Flat
        } else {
            LayoutKind::from_name(&string(&fields, "layout")?)
                .ok_or_else(|| malformed("unknown layout"))?
        };
        let schema = if version == RECORDS_VERSION {
            let Json::Array(columns) = field(&fields, "schema")? else {
                return Err(malformed("schema is not an array"));
            };
            Some(
                columns
                    .into_iter()
                    .map(|c| column_spec(&c.into_object()?))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        } else {
            None
        };
        let flushed_lsn = unsigned(&fields, "flushed_lsn")?;
        let dropped_lsn = if fields.iter().any(|(k, _)| k == "dropped_lsn") {
            unsigned(&fields, "dropped_lsn")?
        } else {
            0
        };
        let Json::Array(files) = field(&fields, "files")? else {
            return Err(malformed("files is not an array"));
        };
        let files = files
            .into_iter()
            .map(|f| file_entry(&f.into_object()?, version))
            .collect::<Result<Vec<_>, _>>()?;
        let manifest = Manifest {
            format,
            layout,
            schema,
            flushed_lsn,
            dropped_lsn,
            files,
        };
        manifest.check()?;
        Ok(manifest)
    }

    /// Reads `<table>/manifest.json`; `None` if there is none yet.
    pub fn read(table: &Path) -> Result<Option<Manifest>, ManifestError> {
        match fs::read_to_string(table.join(MANIFEST_FILE)) {
            Ok(text) => Manifest::parse(&text).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ManifestError::Io(e)),
        }
    }

    /// Atomically replaces `<table>/manifest.json`: the commit point.
    pub fn write(&self, table: &Path) -> Result<(), ManifestError> {
        self.check()?;
        let tmp = table.join(MANIFEST_TMP);
        {
            let mut file = File::create(&tmp)?;
            file.write_all(self.to_json().as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, table.join(MANIFEST_FILE))?;
        File::open(table)?.sync_all()?;
        Ok(())
    }
}

fn write_range(out: &mut String, name: &str, range: Option<(i64, i64)>) {
    match range {
        Some((lo, hi)) => out.push_str(&format!(",\"{name}_min\":{lo},\"{name}_max\":{hi}")),
        None => out.push_str(&format!(",\"{name}_min\":null,\"{name}_max\":null")),
    }
}

/// An owned JSON value: the parser's borrowed tree, with arrays (kept as
/// source text by the parser) parsed into their elements.
#[derive(Debug, Clone)]
enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Other,
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
}

impl Json {
    fn into_object(self) -> Result<Vec<(String, Json)>, ManifestError> {
        match self {
            Json::Object(fields) => Ok(fields),
            _ => Err(malformed("expected an object")),
        }
    }
}

/// Arrays deeper than this make a manifest malformed. A real one nests
/// an array or two; the parser scans an array's text flat and parses its
/// objects afresh, so without a cap here `{"a":[{"a":[...` would recurse
/// once per level unbounded (#656).
const MAX_ARRAY_DEPTH: usize = 8;

fn owned(value: &JsonValue) -> Result<Json, ManifestError> {
    owned_at(value, 0)
}

/// [`owned`] inside `arrays` enclosing arrays.
fn owned_at(value: &JsonValue, arrays: usize) -> Result<Json, ManifestError> {
    Ok(match value {
        JsonValue::Null => Json::Null,
        JsonValue::Int(v) => Json::Int(*v),
        JsonValue::Str(s) => Json::Str((*s).to_string()),
        JsonValue::Object(fields) => Json::Object(
            fields
                .iter()
                .map(|(k, v)| Ok(((*k).to_string(), owned_at(v, arrays)?)))
                .collect::<Result<Vec<_>, ManifestError>>()?,
        ),
        JsonValue::Array(text) => {
            let arrays = arrays
                .checked_add(1)
                .filter(|a| *a <= MAX_ARRAY_DEPTH)
                .ok_or_else(|| malformed("arrays nested too deeply"))?;
            Json::Array(array_of_objects(text, arrays)?)
        }
        JsonValue::Bool(b) => Json::Bool(*b),
        JsonValue::Float(_) => Json::Other,
    })
}

fn field(fields: &[(String, Json)], name: &str) -> Result<Json, ManifestError> {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| ManifestError::Malformed(format!("missing {name}")))
}

fn int(fields: &[(String, Json)], name: &str) -> Result<i64, ManifestError> {
    match field(fields, name)? {
        Json::Int(v) => Ok(v),
        _ => Err(ManifestError::Malformed(format!(
            "{name} is not an integer"
        ))),
    }
}

fn unsigned(fields: &[(String, Json)], name: &str) -> Result<u64, ManifestError> {
    u64::try_from(int(fields, name)?)
        .map_err(|_| ManifestError::Malformed(format!("{name} is negative")))
}

fn string(fields: &[(String, Json)], name: &str) -> Result<String, ManifestError> {
    match field(fields, name)? {
        Json::Str(s) if !s.contains('\\') => Ok(s),
        _ => Err(ManifestError::Malformed(format!(
            "{name} is not a plain string"
        ))),
    }
}

fn range(fields: &[(String, Json)], name: &str) -> Result<Option<(i64, i64)>, ManifestError> {
    let lo = field(fields, &format!("{name}_min"))?;
    let hi = field(fields, &format!("{name}_max"))?;
    match (lo, hi) {
        (Json::Int(lo), Json::Int(hi)) => Ok(Some((lo, hi))),
        (Json::Null, Json::Null) => Ok(None),
        _ => Err(ManifestError::Malformed(format!("{name} range"))),
    }
}

/// One record-table column.
fn column_spec(fields: &[(String, Json)]) -> Result<ColumnSpec, ManifestError> {
    let name = string(fields, "name")?;
    let column_type = type_from_name(&string(fields, "type")?)
        .ok_or_else(|| ManifestError::Malformed(format!("column {name}: unknown type")))?;
    let Json::Bool(nullable) = field(fields, "nullable")? else {
        return Err(ManifestError::Malformed(format!(
            "column {name}: nullable is not a boolean"
        )));
    };
    Ok(ColumnSpec::new(name, column_type, nullable))
}

fn file_entry(fields: &[(String, Json)], version: i64) -> Result<FileEntry, ManifestError> {
    let area = if version == 1 {
        Area::Data
    } else {
        Area::from_name(&string(fields, "area")?).ok_or_else(|| malformed("unknown area"))?
    };
    Ok(FileEntry {
        path: string(fields, "path")?,
        area,
        lsn_lo: unsigned(fields, "lsn_lo")?,
        lsn_hi: unsigned(fields, "lsn_hi")?,
        rows: unsigned(fields, "rows")?,
        bytes: unsigned(fields, "bytes")?,
        observed_ts: range(fields, "observed_ts")?,
        timestamp: range(fields, "timestamp")?,
    })
}

/// The elements of a JSON array of objects, from its source text.
fn array_of_objects(text: &str, arrays: usize) -> Result<Vec<Json>, ManifestError> {
    let mut rest = text
        .trim()
        .strip_prefix('[')
        .ok_or_else(|| malformed("expected an array"))?
        .trim_start();
    let mut items = Vec::new();
    if let Some(after) = rest.strip_prefix(']') {
        return end_of_array(after, items);
    }
    loop {
        let (item, after) =
            parse_object(rest).ok_or_else(|| malformed("an array element is not an object"))?;
        items.push(owned_at(&item, arrays)?);
        let after = after.trim_start();
        if let Some(next) = after.strip_prefix(',') {
            rest = next.trim_start();
            continue;
        }
        let Some(after) = after.strip_prefix(']') else {
            return Err(malformed("array: expected , or ]"));
        };
        return end_of_array(after, items);
    }
}

fn end_of_array(after: &str, items: Vec<Json>) -> Result<Vec<Json>, ManifestError> {
    if after.trim().is_empty() {
        Ok(items)
    } else {
        Err(malformed("text after an array"))
    }
}

#[cfg(test)]
mod tests;
