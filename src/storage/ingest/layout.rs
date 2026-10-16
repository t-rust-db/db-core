//! Where a table's files go (ADR 0028, #573).
//!
//! A table's layout is fixed when it is created and recorded in the
//! manifest:
//!
//! - [`Layout::Flat`]: every flush is one file in `data/` (ADR 0027), and
//!   files are never rewritten.
//! - [`Layout::Tiered`]: files are partitioned by arrival time
//!   (`observed_ts`). A closed hour is written to
//!   `live/date=D/hour=H/` as one file, its rows sorted by `sort_key` and
//!   cut into row groups; `TableWriter::compact` merges a closed day's
//!   hours into `archive/date=D/`, which is final. Every output is built
//!   under `tmp/` and published with one directory rename, so a directory
//!   listing sees an hour or a day whole or not at all. An open hour with
//!   more than `spill_rows` rows in sealed pages spills them to
//!   `staging/date=D/hour=H/` as sorted runs, which its close merges.
//!
//! Because `observed_ts` and the LSN only grow together, every hour and
//! every day is a contiguous LSN range, and ADR 0027's manifest invariant
//! holds through every compaction.

use std::cmp::Ordering;

use crate::vm::batch::Value;

/// Nanoseconds in an hour.
pub const HOUR_NS: i64 = 3_600_000_000_000;
/// Nanoseconds in a day.
pub const DAY_NS: i64 = 86_400_000_000_000;

/// Settings of a tiered table. Only the choice of layout is recorded in the
/// manifest; these may change between opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierOptions {
    /// Columns a closed hour's rows are sorted by, in order, before they
    /// are cut into row groups; empty keeps arrival order. A name the
    /// table does not have sorts as NULL.
    pub sort_key: Vec<String>,
    /// Most rows in a row group. Row groups never span an hour.
    pub row_group_max_rows: usize,
    /// A day merge starts a new file once a file reaches this size, at an
    /// hour boundary, so a file can exceed it by one hour. A closed hour
    /// is always one file.
    pub max_file_size: u64,
    /// How long files replaced by a compaction are kept, for external
    /// readers that list directories instead of reading the manifest.
    pub delete_grace_ns: i64,
    /// Most rows the open hour keeps in sealed pages: past it, they spill
    /// to `staging/` as a sorted run (the head, at most one page, stays).
    /// The memory budget of a tiered table, in rows rather than bytes,
    /// since that is what is counted exactly.
    pub spill_rows: usize,
    /// Closed days kept in `archive/`: `TableWriter::compact`
    /// drops older ones, whole, in one commit, and deletes their files after
    /// the grace period. `None` (the default) keeps everything.
    pub retention_days: Option<u32>,
}

impl Default for TierOptions {
    /// Arrival order, 1M-row row groups, 1 GiB files, 10 minutes' grace,
    /// spills past 1M rows, no retention.
    fn default() -> Self {
        TierOptions {
            sort_key: Vec::new(),
            row_group_max_rows: 1_000_000,
            max_file_size: 1 << 30,
            delete_grace_ns: 600_000_000_000,
            spill_rows: 1_000_000,
            retention_days: None,
        }
    }
}

/// How a table lays out its files.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Layout {
    /// One file per flush in `data/`.
    #[default]
    Flat,
    /// Hours in `live/`, days in `archive/`.
    Tiered(TierOptions),
}

impl Layout {
    /// The kind recorded in the manifest.
    pub fn kind(&self) -> LayoutKind {
        match self {
            Layout::Flat => LayoutKind::Flat,
            Layout::Tiered(_) => LayoutKind::Tiered,
        }
    }
}

/// Which layout a table has, as the manifest records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    /// See [`Layout::Flat`].
    Flat,
    /// See [`Layout::Tiered`].
    Tiered,
}

impl LayoutKind {
    /// The manifest's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            LayoutKind::Flat => "flat",
            LayoutKind::Tiered => "tiered",
        }
    }

    /// The kind called `name`.
    pub fn from_name(name: &str) -> Option<LayoutKind> {
        [LayoutKind::Flat, LayoutKind::Tiered]
            .into_iter()
            .find(|k| k.as_str() == name)
    }
}

/// The directory a file is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    /// `data/`, a flat table's files.
    Data,
    /// `live/date=D/hour=H/`, a closed hour.
    Live,
    /// `archive/date=D/`, a closed day; final.
    Archive,
    /// `staging/date=D/hour=H/`, sorted runs the open hour spilled.
    Staging,
}

impl Area {
    /// The directory name, which is also the manifest's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Area::Data => "data",
            Area::Live => "live",
            Area::Archive => "archive",
            Area::Staging => "staging",
        }
    }

    /// The area called `name`.
    pub fn from_name(name: &str) -> Option<Area> {
        [Area::Data, Area::Live, Area::Archive, Area::Staging]
            .into_iter()
            .find(|a| a.as_str() == name)
    }
}

/// The hour (since the epoch) `ns` falls in.
pub fn hour_of(ns: i64) -> i64 {
    ns.div_euclid(HOUR_NS)
}

/// The day (since the epoch) an hour falls in.
pub fn day_of_hour(hour: i64) -> i64 {
    hour.div_euclid(24)
}

/// `YYYY-MM-DD` of `day` days since 1970-01-01 (proleptic Gregorian;
/// Howard Hinnant's `civil_from_days`).
pub fn date_string(day: i64) -> String {
    // Every intermediate is bounded by the 400-year era (doe < 146 097),
    // so only `era` can grow; saturating keeps an absurd day from
    // panicking.
    let z = day.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = doe
        .saturating_sub(doe / 1460)
        .saturating_add(doe / 36_524)
        .saturating_sub(doe / 146_096)
        / 365;
    let doy = doe.saturating_sub(
        yoe.saturating_mul(365)
            .saturating_add(yoe / 4)
            .saturating_sub(yoe / 100),
    );
    let mp = doy.saturating_mul(5).saturating_add(2) / 153;
    let d = doy
        .saturating_sub(mp.saturating_mul(153).saturating_add(2) / 5)
        .saturating_add(1);
    let m = if mp < 10 {
        mp.saturating_add(3)
    } else {
        mp.saturating_sub(9)
    };
    let y = yoe
        .saturating_add(era.saturating_mul(400))
        .saturating_add(i64::from(m <= 2));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `live/date=D/hour=H`, the directory of a closed hour.
pub fn live_dir(hour: i64) -> String {
    format!(
        "{}/date={}/hour={:02}",
        Area::Live.as_str(),
        date_string(day_of_hour(hour)),
        hour.rem_euclid(24)
    )
}

/// `staging/date=D/hour=H`, the directory of an open hour's spills.
pub fn staging_dir(hour: i64) -> String {
    format!(
        "{}/date={}/hour={:02}",
        Area::Staging.as_str(),
        date_string(day_of_hour(hour)),
        hour.rem_euclid(24)
    )
}

/// `archive/date=D`, the directory of a closed day.
pub fn archive_dir(day: i64) -> String {
    format!("{}/date={}", Area::Archive.as_str(), date_string(day))
}

/// The order rows are sorted in: NULL first, then numbers (integers and
/// floats compared as numbers; NaN last), booleans, strings, maps.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Int(_) | Value::Float(_) => 1,
            Value::Bool(_) => 2,
            Value::Str(_) => 3,
            Value::Map(_) => 4,
        }
    }
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).total_cmp(y),
        (Value::Float(x), Value::Int(y)) => x.total_cmp(&(*y as f64)),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Map(x), Value::Map(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

#[cfg(test)]
mod tests;
