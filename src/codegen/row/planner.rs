// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Cost model (#461, spec 011/Req 3): [`Stats`] decodes `sqlite_stat1`
//! rows for one table into an in-memory shape, and [`estimate_scan_cost`]/
//! [`estimate_index_cost`] turn those stats into a [`PlanCost`].
//! [`load_stats`] reads every table's `sqlite_stat1` rows in one pass —
//! the CLI (`query`/`repl`) calls it once per statement, alongside its
//! existing `read_schema` call, and threads the result into
//! `codegen::select::join_access::choose_join_access` (spec 011/Req 4,
//! #461 Phase 3) so a table with no `ANALYZE` history behaves exactly
//! as it did before this module existed.
//!
//! Missing stats (no `ANALYZE` has ever run) deliberately produce a
//! conservative worst-case estimate rather than panicking or dividing by
//! zero — that's what keeps every existing stats-free optimization
//! (`009-vdbe-codegen` Requirement 16) behaviorally unaffected by this
//! module's mere existence.

use crate::codegen::row::{IndexSchema, TableSchema};
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Row-count and per-index `avg_eq` statistics for one table, decoded
/// from its `sqlite_stat1` rows (spec 011/Req 2's `"<rows>"` table-row
/// format and `"<rows> <avg_eq>"` index-row format). Empty (the
/// `Default`) when `ANALYZE` has never populated stats for this table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    table_rows: Option<u64>,
    /// Index name -> `(index row count, avg_eq)`.
    index_stats: HashMap<String, (u64, u64)>,
    /// `sqlite_stat4` samples per index (#498), keyed by index name.
    /// Empty unless the file was analyzed by a `SQLITE_ENABLE_STAT4`
    /// build (real sqlite3 is; our own `ANALYZE` writes `stat1` only).
    index_samples: HashMap<String, IndexSamples>,
    /// The `sz=N` hint on the table's own `stat1` row, if any --
    /// sqlite3's `decodeIntArray` overrides its declared-type row-width
    /// estimate with it (see [`row_width_log_est`]).
    table_row_size: Option<u64>,
    /// `sz=N` hints on index `stat1` rows, same meaning.
    index_row_sizes: HashMap<String, u64>,
}

impl Stats {
    /// Decodes `Stats` from a table's `sqlite_stat1` rows: `(idx, stat)`
    /// pairs, `idx = None` for the table's own row-count row and
    /// `idx = Some(name)` for one of its indexes — exactly the shape
    /// `SELECT idx, stat FROM sqlite_stat1 WHERE tbl = ?` returns. A row
    /// whose `stat` text doesn't parse as the expected integer(s) is
    /// skipped rather than treated as a hard error — a hand-edited or
    /// corrupt `sqlite_stat1` degrades to "no stats for that entry",
    /// which [`estimate_scan_cost`]/[`estimate_index_cost`] already
    /// handle safely.
    pub fn from_stat1_rows(rows: impl IntoIterator<Item = (Option<String>, String)>) -> Self {
        let mut table_rows = None;
        let mut index_stats = HashMap::new();
        let mut table_row_size = None;
        let mut index_row_sizes = HashMap::new();
        for (idx, stat) in rows {
            let mut parts = stat.split_whitespace();
            // `sz=N` may trail the integers (sqlite3 `decodeIntArray`);
            // any other trailing word (`unordered`, `noskipscan`) is
            // ignored here.
            let sz = stat
                .split_whitespace()
                .find_map(|w| w.strip_prefix("sz=").and_then(|n| n.parse::<u64>().ok()))
                .map(|n| n.max(2));
            match idx {
                None => {
                    table_rows = parts.next().and_then(|s| s.parse().ok());
                    table_row_size = sz;
                }
                Some(name) => {
                    let rows = parts.next().and_then(|s| s.parse().ok());
                    let avg_eq = parts.next().and_then(|s| s.parse().ok());
                    if let (Some(rows), Some(avg_eq)) = (rows, avg_eq) {
                        index_stats.insert(name.clone(), (rows, avg_eq));
                    }
                    if let Some(sz) = sz {
                        index_row_sizes.insert(name, sz);
                    }
                }
            }
        }
        Stats {
            table_rows,
            index_stats,
            index_samples: HashMap::new(),
            table_row_size,
            index_row_sizes,
        }
    }

    /// Attaches `sqlite_stat4` samples for `index_name` (#498). `samples`
    /// are the index's `sqlite_stat4` rows in stored order; this derives
    /// sqlite3's `nRowEst0`/`aAvgEq` (`initAvgEq`) from them and from
    /// the index's `stat1` row when present. Samples for an index the
    /// file has no `stat1` row for are still usable: sqlite3 then takes
    /// the row count from the final sample.
    pub fn with_stat4_samples(mut self, index_name: &str, samples: Vec<Stat4Sample>) -> Self {
        if samples.is_empty() {
            return self;
        }
        let stat1 = self.index_stats.get(index_name).copied();
        let summary = IndexSamples::new(samples, stat1);
        self.index_samples.insert(index_name.to_string(), summary);
        self
    }

    /// Whether any index of this table carries `stat4` samples --
    /// sqlite3's `TF_HasStat4`, which shaves 2 off the full-scan cost.
    pub fn has_stat4(&self) -> bool {
        !self.index_samples.is_empty()
    }

    /// The `stat4` samples for `index_name`, if the file carries them.
    pub fn index_samples(&self, index_name: &str) -> Option<&IndexSamples> {
        self.index_samples.get(index_name)
    }

    /// The table's total row count, or `None` if `ANALYZE` has never
    /// recorded one.
    pub fn table_rows(&self) -> Option<u64> {
        self.table_rows
    }

    /// `(index row count, avg_eq)` for the named index, or `None` if
    /// `ANALYZE` has never recorded stats for it.
    pub fn index_stats(&self, index_name: &str) -> Option<(u64, u64)> {
        self.index_stats.get(index_name).copied()
    }
}

/// A plan's estimated cost: rows it would touch, and (in this MVP cost
/// model) I/O treated as one page-worth of work per row — spec 011/Req 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCost {
    /// Estimated number of rows the plan would touch.
    pub estimated_rows: u64,
    /// Estimated I/O cost, in page-worth-of-work units (one per row).
    pub estimated_io: u64,
}

impl PlanCost {
    /// The conservative "no stats available" estimate: `estimated_rows`
    /// pinned to `u64::MAX` so a full scan under this estimate always
    /// loses to any index probe that *does* have stats, and never loses
    /// to another stats-free scan (every stats-free estimate is equally
    /// maximal, so callers comparing two of these must not treat the
    /// comparison as meaningful — see spec 011/Req 4's "no ANALYZE"
    /// scenario, which never reaches a cost comparison at all).
    const UNKNOWN: PlanCost = PlanCost {
        estimated_rows: u64::MAX,
        estimated_io: u64::MAX,
    };
}

/// Estimates the cost of a full table scan: `stats`' recorded row count,
/// or [`PlanCost::UNKNOWN`] if `ANALYZE` has never run for this table.
pub fn estimate_scan_cost(stats: &Stats) -> PlanCost {
    match stats.table_rows() {
        Some(rows) => PlanCost {
            estimated_rows: rows,
            estimated_io: rows,
        },
        None => PlanCost::UNKNOWN,
    }
}

/// The minimum leading-column `avg_eq` (average rows sharing one
/// leading-column value) for skip-scan to be considered worthwhile —
/// #485. Mirrors oracle sqlite3's documented "about 18 or more
/// duplicates" threshold (sqlite.org/optoverview.html's Skip-Scan
/// section), confirmed empirically against sqlite3 3.51.0: `avg_eq =
/// 19` on a composite index's leading column picks a skip-scan plan
/// (`SEARCH t USING INDEX idx (ANY(a) AND b=?)`), `avg_eq = 17` falls
/// back to a full scan.
const SKIP_SCAN_MIN_AVG_EQ: u64 = 18;

/// Whether a skip-scan over `index_name` (probing a non-leading column
/// while the leading column is unconstrained) is worth attempting —
/// #485. `false` whenever `ANALYZE` has never recorded stats for this
/// index, matching oracle sqlite3's behavior of never choosing
/// skip-scan without `ANALYZE` history (its own no-stats default guess
/// of 10 duplicates sits below [`SKIP_SCAN_MIN_AVG_EQ`]).
pub fn is_skip_scan_worthwhile(index_name: &str, stats: &Stats) -> bool {
    stats
        .index_stats(index_name)
        .is_some_and(|(_rows, avg_eq)| avg_eq >= SKIP_SCAN_MIN_AVG_EQ)
}

/// Estimates the cost of an equality probe against `index_name`: the
/// index's recorded `avg_eq` (average rows sharing one key value,
/// floored at 1 since even a matching probe touches at least one row),
/// or [`PlanCost::UNKNOWN`] if `ANALYZE` has never recorded stats for
/// this index.
pub fn estimate_index_cost(index_name: &str, stats: &Stats) -> PlanCost {
    match stats.index_stats(index_name) {
        Some((_rows, avg_eq)) => {
            let estimated_rows = avg_eq.max(1);
            PlanCost {
                estimated_rows,
                estimated_io: estimated_rows,
            }
        }
        None => PlanCost::UNKNOWN,
    }
}

/// #545: the smallest `ANALYZE`-recorded row count a join level's inner
/// table must clear before building a transient automatic index for an
/// otherwise-unindexed equality join column is estimated cheaper than
/// the plain nested-loop scan it replaces (sqlite.org/optoverview.html
/// #autoindex) — building the index costs one full scan of the table up
/// front, so it only pays for itself once the table is big enough that
/// repeatedly nested-loop-scanning it (once per outer row) would cost
/// more overall: a small table's full scan is already cheap, so skip
/// the extra machinery.
const MIN_ROWS_TO_AUTO_INDEX: u64 = 25;

/// Whether building a transient automatic index (#545) for `stats`'
/// table is estimated worthwhile in place of a plain nested-loop scan:
/// `true` only when `ANALYZE` has recorded at least
/// [`MIN_ROWS_TO_AUTO_INDEX`] rows — a stats-free database (no
/// `ANALYZE` has ever run) never triggers this optimization, matching
/// [`is_skip_scan_worthwhile`]'s own "no stats, no optimization"
/// default.
pub fn is_automatic_index_worthwhile(stats: &Stats) -> bool {
    stats
        .table_rows()
        .is_some_and(|rows| rows >= MIN_ROWS_TO_AUTO_INDEX)
}

/// One `sqlite_stat4` row for an index (#498): the sampled key (the
/// index record decoded, key columns then the rowid) and, per key
/// column prefix, the number of index rows equal to / less than /
/// distinct-and-less-than the sample. Mirrors sqlite3's `IndexSample`.
#[derive(Debug, Clone, PartialEq)]
pub struct Stat4Sample {
    /// The sample's key values, leading column first.
    pub key: Vec<Value>,
    /// `neq`: rows whose first `i+1` key columns equal the sample's.
    pub n_eq: Vec<u64>,
    /// `nlt`: rows whose first `i+1` key columns sort before the sample's.
    pub n_lt: Vec<u64>,
    /// `ndlt`: distinct prefixes sorting before the sample's.
    pub n_dlt: Vec<u64>,
}

impl Stat4Sample {
    fn lt0(&self) -> u64 {
        self.n_lt.first().copied().unwrap_or(0)
    }
    fn eq0(&self) -> u64 {
        self.n_eq.first().copied().unwrap_or(0)
    }
    fn dlt0(&self) -> u64 {
        self.n_dlt.first().copied().unwrap_or(0)
    }
    fn key0(&self) -> &Value {
        self.key.first().unwrap_or(&Value::Null)
    }
}

/// An index's `sqlite_stat4` samples plus the two derived quantities
/// sqlite3's `initAvgEq` computes from them: the row count the samples
/// describe (`nRowEst0`) and the average duplicate count of the leading
/// key column (`aAvgEq[0]`), used when a probe lands between samples.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSamples {
    samples: Vec<Stat4Sample>,
    n_row_est0: u64,
    avg_eq: u64,
}

impl IndexSamples {
    /// Port of `initAvgEq` for the leading key column, which is the only
    /// column [`estimate_range_rows`] probes (single-column range seeks,
    /// no equality prefix). `stat1` is the index's `(rows, avg_eq)`.
    fn new(samples: Vec<Stat4Sample>, stat1: Option<(u64, u64)>) -> Self {
        let count = samples.len();
        let last = samples.last();
        let (n_row, n_dist100, usable) = match stat1 {
            Some((rows, avg)) if avg != 0 => (
                rows,
                rows.saturating_mul(100).checked_div(avg).unwrap_or(0),
                count,
            ),
            _ => (
                last.map_or(0, Stat4Sample::lt0),
                last.map_or(0, Stat4Sample::dlt0).saturating_mul(100),
                count.saturating_sub(1),
            ),
        };
        let mut sum_eq: u64 = 0;
        let mut n_sum100: u64 = 0;
        for (i, sample) in samples.iter().enumerate().take(usable) {
            let next = samples.get(i.saturating_add(1));
            let is_last = next.is_none();
            let dlt_changes = next.is_some_and(|n| n.dlt0() != sample.dlt0());
            if is_last || dlt_changes {
                sum_eq = sum_eq.saturating_add(sample.eq0());
                n_sum100 = n_sum100.saturating_add(100);
            }
        }
        let mut avg_eq = 0;
        if n_dist100 > n_sum100 && sum_eq < n_row {
            avg_eq = n_row
                .saturating_sub(sum_eq)
                .saturating_mul(100)
                .checked_div(n_dist100.saturating_sub(n_sum100))
                .unwrap_or(0);
        }
        IndexSamples {
            samples,
            n_row_est0: n_row,
            avg_eq: avg_eq.max(1),
        }
    }

    /// Port of `whereKeyStats` for a one-column probe: `(rows less than
    /// probe, rows equal to probe, sample index)`. `round_up` picks the
    /// 2/3 rather than 1/3 point of the gap when the probe falls between
    /// two samples (sqlite3 rounds the upper bound up, the lower down).
    fn key_stats(&self, probe: &Value, round_up: bool) -> (u64, u64, usize) {
        let mut i_min = 0usize;
        let mut i_sample = self.samples.len();
        let mut i_lower: u64 = 0;
        let mut res = Ordering::Less;
        // `whereKeyStats`'s bisection, one field per sample.
        loop {
            let i_test = i_min.midpoint(i_sample);
            let Some(sample) = self.samples.get(i_test) else {
                break;
            };
            res = compare_binary(sample.key0(), probe);
            if res == Ordering::Less {
                i_lower = sample.lt0().saturating_add(sample.eq0());
                i_min = i_test.saturating_add(1);
            } else {
                i_sample = i_test;
            }
            if res == Ordering::Equal || i_min >= i_sample {
                break;
            }
        }
        let i = i_sample;
        if res == Ordering::Equal {
            if let Some(sample) = self.samples.get(i) {
                return (sample.lt0(), sample.eq0(), i);
            }
        }
        let i_upper = self
            .samples
            .get(i)
            .map_or(self.n_row_est0, Stat4Sample::lt0);
        let gap = i_upper.saturating_sub(i_lower);
        let gap = if round_up {
            gap.saturating_mul(2) / 3
        } else {
            gap / 3
        };
        (i_lower.saturating_add(gap), self.avg_eq, i)
    }
}

/// BINARY-collation ordering of two values (NULL < numeric < text <
/// blob, `sqlite3VdbeRecordCompare` for a one-column record), used to
/// place a probe among the samples. The range seek itself is only ever
/// compiled for a BINARY leading column (#298), so no other collation
/// can reach here. Kept local: `codegen-row` does not depend on
/// `storage-row`, whose b-tree code has the same ordering.
fn compare_binary(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Integer(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "SQLite compares INTEGER against REAL as f64 (sqlite3IntFloatCompare)"
    )]
    match (a, b) {
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Integer(x), Value::Real(y)) => {
            (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (Value::Real(x), Value::Integer(y)) => {
            x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal)
        }
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// sqlite3's `LogEst`: an integer approximating `10 * log2(x)`. The
/// planner's whole cost model is done in these units so that the
/// seek-versus-scan decision below reproduces the oracle's exactly
/// (`sqlite3LogEst`, `sqlite3LogEstAdd`, `estLog` in `util.c`/`where.c`).
/// Every value this module produces lies in `0..=700` (`10 * log2` of a
/// `u64` is at most 640, plus the cost model's small additive constants),
/// so the plain `i16` arithmetic below cannot overflow; the ported
/// formulas are kept verbatim rather than rewritten as saturating ops.
pub type LogEst = i16;

/// `sqlite3LogEst(x)`.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "verbatim sqlite3LogEst: y is bounded by 10 * 64 and x only shrinks"
)]
pub fn log_est(x: u64) -> LogEst {
    const A: [LogEst; 8] = [0, 2, 3, 5, 6, 7, 8, 9];
    let mut x = x;
    let mut y: LogEst = 40;
    if x < 8 {
        if x < 2 {
            return 0;
        }
        while x < 8 {
            y -= 10;
            x <<= 1;
        }
    } else {
        while x > 255 {
            y += 40;
            x >>= 4;
        }
        while x > 15 {
            y += 10;
            x >>= 1;
        }
    }
    let idx = usize::try_from(x & 7).unwrap_or(0);
    A.get(idx).copied().unwrap_or(0) + y - 10
}

/// `sqlite3LogEstAdd(a, b)`: the LogEst of the sum of two LogEst values.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "verbatim sqlite3LogEstAdd over bounded LogEst operands (see `LogEst`)"
)]
pub fn log_est_add(a: LogEst, b: LogEst) -> LogEst {
    const X: [LogEst; 32] = [
        10, 10, 9, 9, 8, 8, 7, 7, 7, 6, 6, 6, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2,
        2, 2,
    ];
    let (big, small) = if a >= b { (a, b) } else { (b, a) };
    if big > small + 49 {
        return big;
    }
    if big > small + 31 {
        return big + 1;
    }
    let idx = usize::try_from(big - small).unwrap_or(0);
    big + X.get(idx).copied().unwrap_or(0)
}

/// `estLog(N)`: the LogEst of `log2(N)`, sqlite3's per-lookup b-tree
/// descent cost.
fn est_log(n: LogEst) -> LogEst {
    if n <= 10 {
        0
    } else {
        log_est(u64::try_from(n).unwrap_or(0)).saturating_sub(33)
    }
}

/// sqlite3's per-column row-width estimate (`Column.szEst`): a standard
/// TEXT/BLOB/ANY type counts 5, a numeric or untyped column 1, and a
/// type with a `(N)` width suffix `N/4 + 1` (capped at 255).
fn column_size_est(declared_type: &str) -> u64 {
    let t = declared_type.trim();
    if let Some((_, after_paren)) = t.split_once('(') {
        let digits: String = after_paren
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(n) = digits.parse::<u64>() {
            return (n / 4).saturating_add(1).min(255);
        }
    }
    if t.eq_ignore_ascii_case("TEXT")
        || t.eq_ignore_ascii_case("BLOB")
        || t.eq_ignore_ascii_case("ANY")
    {
        5
    } else {
        1
    }
}

/// `szTabRow`: `estimateTableWidth`, or the `sz=` hint from `stat1`.
fn table_row_width(schema: &TableSchema, stats: &Stats) -> LogEst {
    if let Some(sz) = stats.table_row_size {
        return log_est(sz);
    }
    let mut w: u64 = schema
        .column_types
        .iter()
        .map(|t| column_size_est(t))
        .fold(0, u64::saturating_add);
    if schema.rowid_alias.is_none() {
        w = w.saturating_add(1);
    }
    log_est(w.saturating_mul(4))
}

/// `szIdxRow`: `estimateIndexWidth` (key columns plus the trailing
/// rowid), or the index's `sz=` hint.
fn index_row_width(schema: &TableSchema, index: &IndexSchema, stats: &Stats) -> LogEst {
    if let Some(sz) = stats.index_row_sizes.get(&index.name) {
        return log_est(*sz);
    }
    let w: u64 = index
        .columns
        .iter()
        .map(|c| {
            schema
                .columns
                .iter()
                .position(|name| name.eq_ignore_ascii_case(&c.name))
                .and_then(|i| schema.column_types.get(i))
                .map_or(1, |t| column_size_est(t))
        })
        .fold(1, u64::saturating_add);
    log_est(w.saturating_mul(4))
}

/// One side of a range predicate on an index's leading column (#498).
#[derive(Debug, Clone, PartialEq)]
pub struct RangeBound {
    /// The bound, already coerced to the column's affinity, or `None`
    /// for a bound the planner cannot see through (a bind parameter, a
    /// subquery, an expression) -- sqlite3 then uses its fixed default.
    pub value: Option<Value>,
    /// `>=`/`<=` rather than `>`/`<`.
    pub inclusive: bool,
}

/// Port of `whereRangeScanEst` for a range on an index's leading column
/// with no equality prefix: the LogEst of the rows the seek visits.
/// `index_rows_log` is the index's `stat1` row count as a LogEst
/// (`aiRowLogEst[0]`). With `stat4` samples and literal bounds, each
/// bound is located in the histogram; otherwise sqlite3's TUNING
/// defaults apply -- an open range keeps 1/4 of the rows, a closed one
/// 1/64 -- and a bound that cannot be probed keeps its default even when
/// the other side is probed.
fn estimate_range_rows(
    samples: Option<&IndexSamples>,
    index_rows_log: LogEst,
    lower: Option<RangeBound>,
    upper: Option<RangeBound>,
) -> LogEst {
    let mut n_out = index_rows_log;
    let mut lower_left = lower.as_ref();
    let mut upper_left = upper.as_ref();
    if let Some(samples) = samples {
        let mut i_lower: u64 = 0;
        let mut i_upper: u64 = samples.n_row_est0;
        let mut lwr_idx: Option<usize> = None;
        let mut upr_idx: Option<usize> = None;
        if let Some(RangeBound {
            value: Some(v),
            inclusive,
        }) = &lower
        {
            let (lt, eq, idx) = samples.key_stats(v, false);
            // `x > v` starts past the rows equal to v; `x >= v` includes them.
            let new = if *inclusive {
                lt
            } else {
                lt.saturating_add(eq)
            };
            i_lower = i_lower.max(new);
            lwr_idx = Some(idx);
            n_out = n_out.saturating_sub(1);
            lower_left = None;
        }
        if let Some(RangeBound {
            value: Some(v),
            inclusive,
        }) = &upper
        {
            let (lt, eq, idx) = samples.key_stats(v, true);
            // `x <= v` includes the rows equal to v; `x < v` stops before them.
            let new = if *inclusive {
                lt.saturating_add(eq)
            } else {
                lt
            };
            i_upper = i_upper.min(new);
            upr_idx = Some(idx);
            n_out = n_out.saturating_sub(1);
            upper_left = None;
        }
        if lower_left.is_none() || upper_left.is_none() {
            let n_new = if i_upper > i_lower {
                let n = log_est(i_upper.saturating_sub(i_lower));
                // TUNING: both bounds inside the same sample gap --
                // assume the range is a quarter of that gap.
                if lwr_idx.is_some() && lwr_idx == upr_idx {
                    n.saturating_sub(20)
                } else {
                    n
                }
            } else {
                10
            };
            n_out = n_out.min(n_new);
        }
    }
    // The stats-free tail of `whereRangeScanEst` for whatever bound the
    // histogram did not consume.
    let mut n_new = n_out;
    if lower_left.is_some() {
        n_new = n_new.saturating_sub(20);
    }
    if upper_left.is_some() {
        n_new = n_new.saturating_sub(20);
    }
    if lower_left.is_some() && upper_left.is_some() {
        n_new = n_new.saturating_sub(20);
    }
    n_out = n_out
        .saturating_sub(LogEst::from(lower_left.is_some()))
        .saturating_sub(LogEst::from(upper_left.is_some()));
    n_new = n_new.max(10);
    n_out.min(n_new)
}

/// Whether sqlite3 would drive `index` with a range seek on its leading
/// column (`SEARCH ... USING INDEX`) rather than scan the table
/// (`SCAN`), given `stats` (#498). Ports the two `WhereLoop` costs
/// `whereLoopAddBtree`/`whereLoopAddBtreeIndex` build for this shape
/// and sqlite3's cheaper-wins rule:
///
/// - full scan: `log(rows) + 16`, minus 2 when the table has `stat4`;
/// - range seek: `nOut` from [`estimate_range_rows`], plus the index
///   descent (`estLog`) and the per-row index-record cost, plus for a
///   non-`covering` index the per-row table lookup (`nOut + 16`).
///
/// Returns `true` (seek, the behaviour before this gate existed)
/// whenever the index has no `stat1` row: with no statistics sqlite3
/// also always seeks a range, so a never-analyzed file is unaffected.
/// With `stat1` only, the arithmetic likewise always favours the seek;
/// only `stat4` samples can demote it.
pub fn range_seek_beats_scan(
    schema: &TableSchema,
    index: &IndexSchema,
    lower: Option<RangeBound>,
    upper: Option<RangeBound>,
    covering: bool,
    stats: &Stats,
) -> bool {
    let Some((index_rows, _)) = stats.index_stats(&index.name) else {
        return true;
    };
    // `loadStat1`: a table with index `stat1` rows but no row of its own
    // takes its row count from the index (the bench fixtures are like
    // this -- `ANALYZE` writes one row per index and one per table only
    // when the table has no index).
    let table_rows = stats.table_rows().unwrap_or(index_rows);
    let r_size = log_est(index_rows);
    let n_out = estimate_range_rows(stats.index_samples(&index.name), r_size, lower, upper);
    let sz_idx = index_row_width(schema, index, stats);
    let sz_tab = table_row_width(schema, stats).max(1);
    let per_row = sz_idx
        .saturating_mul(15)
        .checked_div(sz_tab)
        .unwrap_or(0)
        .saturating_add(1);
    let r_cost_idx = log_est_add(est_log(r_size), n_out.saturating_add(per_row));
    let r_run = if covering {
        r_cost_idx
    } else {
        log_est_add(r_cost_idx, n_out.saturating_add(16))
    };

    let stat4_discount: LogEst = if stats.has_stat4() { 2 } else { 0 };
    let scan_run = log_est(table_rows)
        .saturating_add(16)
        .saturating_sub(stat4_discount);
    // A tie goes to the seek: `whereLoopFindLesser` lets the index loop
    // (fewer output rows) replace the full scan at equal `rRun`, which
    // the oracle confirms (`x > 50000` on the bench fixture: 210 vs 210,
    // `SEARCH`).
    r_run <= scan_run
}

#[cfg(test)]
mod tests {
    use super::*;

    /// spec 011/Req 3 scenario "Missing stats fall back to a conservative
    /// default".
    #[test]
    fn missing_stats_fall_back_to_max_cost() {
        let stats = Stats::default();
        let cost = estimate_scan_cost(&stats);
        assert_eq!(cost.estimated_rows, u64::MAX);
        assert_eq!(cost.estimated_io, u64::MAX);

        let idx_cost = estimate_index_cost("idx_a", &stats);
        assert_eq!(idx_cost.estimated_rows, u64::MAX);
    }

    /// spec 011/Req 3 scenario "An indexed equality is cheaper than a
    /// scan once stats exist".
    #[test]
    fn indexed_equality_cheaper_than_scan_with_stats() {
        let stats = Stats::from_stat1_rows(vec![
            (None, "10000".to_string()),
            (Some("idx_a".to_string()), "10000 10".to_string()),
        ]);

        let scan = estimate_scan_cost(&stats);
        let indexed = estimate_index_cost("idx_a", &stats);

        assert_eq!(scan.estimated_rows, 10000);
        assert_eq!(indexed.estimated_rows, 10);
        assert!(indexed.estimated_rows < scan.estimated_rows);
    }

    #[test]
    fn unknown_index_name_falls_back_to_unknown() {
        let stats = Stats::from_stat1_rows(vec![(None, "5".to_string())]);
        let cost = estimate_index_cost("no_such_index", &stats);
        assert_eq!(cost.estimated_rows, u64::MAX);
    }

    #[test]
    fn malformed_stat_text_is_skipped_not_a_hard_error() {
        let stats = Stats::from_stat1_rows(vec![(None, "not-a-number".to_string())]);
        assert_eq!(stats.table_rows(), None);
    }

    /// #485: mirrors oracle sqlite3 3.51.0's empirically-confirmed
    /// skip-scan threshold — a leading-column `avg_eq` of 19 picks
    /// skip-scan, 17 does not (`SKIP_SCAN_MIN_AVG_EQ = 18`).
    #[test]
    fn skip_scan_worthwhile_matches_oracle_threshold() {
        let above = Stats::from_stat1_rows(vec![(Some("idx".to_string()), "20001 19".to_string())]);
        assert!(is_skip_scan_worthwhile("idx", &above));

        let below = Stats::from_stat1_rows(vec![(Some("idx".to_string()), "20001 17".to_string())]);
        assert!(!is_skip_scan_worthwhile("idx", &below));

        let at_threshold =
            Stats::from_stat1_rows(vec![(Some("idx".to_string()), "20001 18".to_string())]);
        assert!(is_skip_scan_worthwhile("idx", &at_threshold));
    }

    /// #485: without `ANALYZE` having ever recorded stats for the
    /// index, skip-scan is never chosen — matches oracle's behavior of
    /// never picking skip-scan absent `ANALYZE` history.
    #[test]
    fn skip_scan_never_worthwhile_without_analyze_stats() {
        let stats = Stats::default();
        assert!(!is_skip_scan_worthwhile("idx", &stats));
    }

    /// #545: a table below the row-count threshold isn't worth building
    /// a transient automatic index for.
    #[test]
    fn automatic_index_not_worthwhile_below_threshold() {
        let stats = Stats::from_stat1_rows(vec![(None, "24".to_string())]);
        assert!(!is_automatic_index_worthwhile(&stats));
    }

    /// #545: at/above the row-count threshold, a transient automatic
    /// index is judged worthwhile.
    #[test]
    fn automatic_index_worthwhile_at_and_above_threshold() {
        let stats = Stats::from_stat1_rows(vec![(None, "25".to_string())]);
        assert!(is_automatic_index_worthwhile(&stats));

        let stats = Stats::from_stat1_rows(vec![(None, "10000".to_string())]);
        assert!(is_automatic_index_worthwhile(&stats));
    }

    /// #545: without `ANALYZE` having ever recorded a row count, the
    /// automatic index is never chosen — same "no stats, no
    /// optimization" default as [`is_skip_scan_worthwhile`].
    #[test]
    fn automatic_index_never_worthwhile_without_analyze_stats() {
        let stats = Stats::default();
        assert!(!is_automatic_index_worthwhile(&stats));
    }

    // ---------------------------------------------------------------
    // #498: sqlite3's LogEst arithmetic and the stat4 range estimate.
    // ---------------------------------------------------------------

    /// Pins from sqlite3's own `LogEst` documentation table (util.c).
    #[test]
    fn log_est_matches_sqlite3_reference_values() {
        for (x, expected) in [
            (1u64, 0i16),
            (2, 10),
            (3, 16),
            (8, 30),
            (10, 33),
            (25, 46),
            (100, 66),
            (1000, 99),
            (1024, 100),
            (1_000_000, 199),
            (830_000, 196),
            (4096, 120),
        ] {
            assert_eq!(log_est(x), expected, "log_est({x})");
        }
        assert_eq!(log_est_add(197, 204), 211);
        assert_eq!(log_est_add(43, 197), 197);
        assert_eq!(est_log(196), 43);
        assert_eq!(est_log(10), 0);
    }

    #[test]
    fn column_size_est_follows_sqlite3_declared_type_rules() {
        assert_eq!(column_size_est("INTEGER"), 1);
        assert_eq!(column_size_est("REAL"), 1);
        assert_eq!(column_size_est(""), 1);
        assert_eq!(column_size_est("TEXT"), 5);
        assert_eq!(column_size_est("blob"), 5);
        assert_eq!(column_size_est("VARCHAR(100)"), 26);
        assert_eq!(column_size_est("CHAR"), 1);
    }

    /// `tests/fixtures/btrees/stat4_range.db`: `t(id INTEGER PRIMARY KEY,
    /// a INTEGER, b INTEGER, s TEXT)`, `ia(a)`, 4096 rows with `a = id`,
    /// analyzed by sqlite3 3.53.4 -- its 24 `sqlite_stat4` rows verbatim.
    fn fixture_samples() -> Vec<Stat4Sample> {
        [
            132, 398, 455, 652, 911, 936, 1367, 1823, 1931, 2068, 2114, 2279, 2425, 2427, 2483,
            2735, 3191, 3194, 3390, 3647, 3737, 3759, 3956, 4047,
        ]
        .iter()
        .map(|&below| Stat4Sample {
            // The sample's key is the row with `below` rows before it.
            key: vec![Value::Integer(below + 1), Value::Integer(below + 1)],
            n_eq: vec![1, 1],
            n_lt: vec![below as u64, below as u64],
            n_dlt: vec![below as u64, below as u64],
        })
        .collect()
    }

    fn fixture_stats() -> Stats {
        Stats::from_stat1_rows(vec![(Some("ia".to_string()), "4096 1".to_string())])
            .with_stat4_samples("ia", fixture_samples())
    }

    fn fixture_schema() -> (TableSchema, IndexSchema) {
        let index = IndexSchema {
            name: "ia".to_string(),
            unique: false,
            columns: vec![crate::codegen::row::IndexedColumn {
                name: "a".to_string(),
                desc: false,
                collation: crate::value::Collation::Binary,
            }],
            root_page: 3,
        };
        let schema = TableSchema {
            name: "t".to_string(),
            root_page: 2,
            columns: ["id", "a", "b", "s"]
                .iter()
                .map(|c| (*c).to_string())
                .collect(),
            column_types: ["INTEGER", "INTEGER", "INTEGER", "TEXT"]
                .iter()
                .map(|c| (*c).to_string())
                .collect(),
            rowid_alias: Some(0),
            indexes: vec![index.clone()],
            ..Default::default()
        };
        (schema, index)
    }

    fn bound(v: i64, inclusive: bool) -> (Value, bool) {
        (Value::Integer(v), inclusive)
    }

    /// The estimate itself, pinned to the values sqlite3's
    /// `whereRangeScanEst` computes for the fixture (ported step by step
    /// in Python against the same 24 samples: `BETWEEN 10 AND 3900` ->
    /// rows 44..3891, LogEst 118; `> 4000` -> 3988..4096, LogEst 67).
    #[test]
    fn range_estimate_matches_sqlite3_on_the_fixture_samples() {
        let stats = fixture_stats();
        let samples = stats.index_samples("ia");
        let rows_log = log_est(4096);
        let (lo, hi) = (bound(10, true), bound(3900, true));
        let n = estimate_range_rows(
            samples,
            rows_log,
            Some(RangeBound {
                value: Some(lo.0.clone()),
                inclusive: true,
            }),
            Some(RangeBound {
                value: Some(hi.0.clone()),
                inclusive: true,
            }),
        );
        assert_eq!(n, 118);
        let lo = bound(4000, false);
        let n = estimate_range_rows(
            samples,
            rows_log,
            Some(RangeBound {
                value: Some(lo.0.clone()),
                inclusive: false,
            }),
            None,
        );
        assert_eq!(n, 67);
        // No histogram: sqlite3's fixed 1/4 (one bound) and 1/64 (two).
        let n = estimate_range_rows(
            None,
            rows_log,
            Some(RangeBound {
                value: None,
                inclusive: false,
            }),
            None,
        );
        assert_eq!(n, rows_log - 20);
        let n = estimate_range_rows(
            None,
            rows_log,
            Some(RangeBound {
                value: None,
                inclusive: true,
            }),
            Some(RangeBound {
                value: None,
                inclusive: true,
            }),
        );
        assert_eq!(n, rows_log - 60);
    }

    /// Every plan sqlite3 3.53.4 chose on the fixture, reproduced.
    #[test]
    fn seek_versus_scan_matches_the_oracle_on_the_fixture() {
        let stats = fixture_stats();
        let (schema, index) = fixture_schema();
        let decide = |lower: Option<(Value, bool)>, upper: Option<(Value, bool)>| {
            let lo = lower.as_ref().map(|(v, inc)| RangeBound {
                value: Some(v.clone()),
                inclusive: *inc,
            });
            let hi = upper.as_ref().map(|(v, inc)| RangeBound {
                value: Some(v.clone()),
                inclusive: *inc,
            });
            range_seek_beats_scan(&schema, &index, lo, hi, false, &stats)
        };
        // SCAN
        assert!(!decide(Some(bound(10, true)), Some(bound(3900, true))));
        assert!(!decide(Some(bound(100, false)), None));
        assert!(!decide(None, Some(bound(4000, false))));
        // SEARCH
        assert!(decide(Some(bound(10, true)), Some(bound(20, true))));
        assert!(decide(Some(bound(4000, false)), None));
        assert!(decide(Some(bound(1000, true)), Some(bound(2000, true))));
        assert!(decide(Some(bound(1000, true)), Some(bound(3000, true))));
        // A bound the planner cannot see (subquery/parameter) keeps
        // sqlite3's default and the seek: `a BETWEEN 10 AND (SELECT ...)`.
        let lo = bound(10, true);
        assert!(range_seek_beats_scan(
            &schema,
            &index,
            Some(RangeBound {
                value: Some(lo.0.clone()),
                inclusive: true
            }),
            Some(RangeBound {
                value: None,
                inclusive: true
            }),
            false,
            &stats
        ));
        // A covering walk never fetches the row and always wins.
        let (lo, hi) = (bound(10, true), bound(3900, true));
        assert!(range_seek_beats_scan(
            &schema,
            &index,
            Some(RangeBound {
                value: Some(lo.0.clone()),
                inclusive: true
            }),
            Some(RangeBound {
                value: Some(hi.0.clone()),
                inclusive: true
            }),
            true,
            &stats
        ));
    }

    /// No statistics, or `stat1` only: the seek is never demoted, so a
    /// file that was never analyzed (or analyzed by our own `ANALYZE`,
    /// which writes `stat1` only) behaves exactly as before #498.
    #[test]
    fn without_stat4_the_range_seek_is_never_demoted() {
        let (schema, index) = fixture_schema();
        let (lo, hi) = (bound(10, true), bound(3900, true));
        let wide = |stats: &Stats| {
            range_seek_beats_scan(
                &schema,
                &index,
                Some(RangeBound {
                    value: Some(lo.0.clone()),
                    inclusive: true,
                }),
                Some(RangeBound {
                    value: Some(hi.0.clone()),
                    inclusive: true,
                }),
                false,
                stats,
            )
        };
        assert!(wide(&Stats::default()));
        assert!(wide(&Stats::from_stat1_rows(vec![
            (None, "4096".to_string()),
            (Some("ia".to_string()), "4096 1".to_string()),
        ])));
    }

    /// The bench fixture's shape: `stat1` carries only the index row,
    /// and the decision sits one LogEst unit either side of the scan
    /// cost (`BETWEEN 1000 AND 60000`: 211 vs 210, scan; `> 50000`: 210
    /// vs 210, tie, seek) -- both as sqlite3 3.53.4 decides them.
    #[test]
    fn bench_fixture_decisions_including_the_tie_match_the_oracle() {
        let keys: [(i64, u64, u64); 24] = [
            (11110, 8, 92215),
            (12561, 9, 104257),
            (22222, 8, 184443),
            (28471, 9, 236309),
            (33333, 8, 276665),
            (39477, 9, 327658),
            (44162, 9, 366545),
            (44444, 8, 368886),
            (52102, 9, 432448),
            (55555, 8, 461108),
            (62976, 9, 522701),
            (63682, 9, 528560),
            (63921, 9, 530545),
            (65417, 9, 542962),
            (66623, 9, 552971),
            (66666, 9, 553329),
            (71514, 9, 593566),
            (77778, 9, 645558),
            (77892, 9, 646503),
            (79981, 9, 663842),
            (88889, 9, 737780),
            (89489, 9, 742760),
            (89778, 9, 745159),
            (92234, 9, 765544),
        ];
        let samples = keys
            .iter()
            .map(|&(x, eq, lt)| Stat4Sample {
                key: vec![Value::Integer(x), Value::Integer(0)],
                n_eq: vec![eq, 1],
                n_lt: vec![lt, lt],
                n_dlt: vec![lt / 9, lt],
            })
            .collect();
        let stats = Stats::from_stat1_rows(vec![(
            Some("bench_data_x".to_string()),
            "830000 9".to_string(),
        )])
        .with_stat4_samples("bench_data_x", samples);
        let index = IndexSchema {
            name: "bench_data_x".to_string(),
            unique: false,
            columns: vec![crate::codegen::row::IndexedColumn {
                name: "x".to_string(),
                desc: false,
                collation: crate::value::Collation::Binary,
            }],
            root_page: 3,
        };
        let schema = TableSchema {
            name: "bench_data".to_string(),
            root_page: 2,
            columns: ["id", "n", "x", "f", "s", "bucket"]
                .iter()
                .map(|c| (*c).to_string())
                .collect(),
            column_types: ["INTEGER", "INTEGER", "INTEGER", "REAL", "TEXT", "INTEGER"]
                .iter()
                .map(|c| (*c).to_string())
                .collect(),
            rowid_alias: Some(0),
            indexes: vec![index.clone()],
            ..Default::default()
        };
        let (lo, hi) = (bound(1000, true), bound(60000, true));
        assert!(!range_seek_beats_scan(
            &schema,
            &index,
            Some(RangeBound {
                value: Some(lo.0.clone()),
                inclusive: true
            }),
            Some(RangeBound {
                value: Some(hi.0.clone()),
                inclusive: true
            }),
            false,
            &stats
        ));
        let lo = bound(50000, false);
        assert!(range_seek_beats_scan(
            &schema,
            &index,
            Some(RangeBound {
                value: Some(lo.0.clone()),
                inclusive: false
            }),
            None,
            false,
            &stats
        ));
    }

    // -----------------------------------------------------------------
    // MC/DC vectors for #498's decisions (`mcdc__<id>__vN`).
    // -----------------------------------------------------------------

    fn sample(a: i64, eq: u64, lt: u64, dlt: u64) -> Stat4Sample {
        Stat4Sample {
            key: vec![Value::Integer(a), Value::Integer(a)],
            n_eq: vec![eq, 1],
            n_lt: vec![lt, lt],
            n_dlt: vec![dlt, lt],
        }
    }

    // codegen_row_planner_new_2cd7c98f (`IndexSamples::new`): `is_last || dlt_changes`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_new_2cd7c98f__v1_the_final_sample_always_counts() {
        // One sample, stat1 (1000 rows, avg_eq 5): counted as the last one;
        // 100 * (1000 - 1) / (20000 - 100) = 5.
        let s = IndexSamples::new(vec![sample(10, 1, 100, 50)], Some((1000, 5)));
        assert_eq!(s.avg_eq, 5);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_new_2cd7c98f__v2_a_sample_before_a_distinct_count_change_counts() {
        // Two samples with different `ndlt`: both contribute their 300
        // duplicates; 100 * (1000 - 600) / (20000 - 200) = 2.
        let s = IndexSamples::new(
            vec![sample(10, 300, 100, 10), sample(20, 300, 500, 20)],
            Some((1000, 5)),
        );
        assert_eq!(s.avg_eq, 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_new_2cd7c98f__v3_a_sample_sharing_its_distinct_count_is_skipped() {
        // Same `ndlt` as its successor: only the final sample contributes;
        // 100 * (1000 - 300) / (20000 - 100) = 3.
        let s = IndexSamples::new(
            vec![sample(10, 300, 100, 10), sample(20, 300, 500, 10)],
            Some((1000, 5)),
        );
        assert_eq!(s.avg_eq, 3);
    }

    // codegen_row_planner_new_83cc800c (`IndexSamples::new`):
    // `n_dist100 > n_sum100 && sum_eq < n_row`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_new_83cc800c__v1_more_distinct_keys_than_samples_and_rows_to_spare_averages(
    ) {
        let s = IndexSamples::new(vec![sample(10, 1, 100, 50)], Some((1000, 5)));
        assert_eq!(s.avg_eq, 5);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_new_83cc800c__v2_no_distinct_keys_beyond_the_samples_falls_back_to_one(
    ) {
        // avg_eq 1000 on 1000 rows: 100 distinct-hundredths, equal to the
        // one sample's 100 -- not more, so no average is formed.
        let s = IndexSamples::new(vec![sample(10, 1, 100, 50)], Some((1000, 1000)));
        assert_eq!(s.avg_eq, 1);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_new_83cc800c__v3_samples_accounting_for_every_row_fall_back_to_one(
    ) {
        // The single sample's 10 duplicates are the whole 10-row index.
        let s = IndexSamples::new(vec![sample(10, 10, 0, 0)], Some((10, 1)));
        assert_eq!(s.avg_eq, 1);
    }

    // codegen_row_planner_key_stats_123c5b02 (`key_stats`):
    // `res == Ordering::Equal || i_min >= i_sample`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_key_stats_123c5b02__v1_an_exact_sample_hit_returns_its_counts() {
        let s = fixture_stats();
        let samples = s.index_samples("ia").unwrap();
        // 1368 is the 7th sample (index 6): 1367 rows below, 1 equal.
        assert_eq!(
            samples.key_stats(&Value::Integer(1368), false),
            (1367, 1, 6)
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_key_stats_123c5b02__v2_a_probe_between_samples_interpolates_after_the_bisection(
    ) {
        let s = fixture_stats();
        let samples = s.index_samples("ia").unwrap();
        // 20 sits before the first sample (132 rows below it): a third of
        // that gap rounding down, two thirds rounding up.
        assert_eq!(samples.key_stats(&Value::Integer(20), false), (44, 1, 0));
        assert_eq!(samples.key_stats(&Value::Integer(20), true), (88, 1, 0));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_key_stats_123c5b02__v3_a_probe_past_every_sample_keeps_bisecting_to_the_end(
    ) {
        let s = fixture_stats();
        let samples = s.index_samples("ia").unwrap();
        // Larger than the last sample (4047 rows below it, 1 equal): the
        // gap to the index's 4096 rows is 48, a third of it is 16.
        assert_eq!(
            samples.key_stats(&Value::Integer(9000), false),
            (4064, 1, 24)
        );
    }

    // codegen_row_planner_column_size_est_c843518e:
    // `TEXT || BLOB || ANY`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_column_size_est_c843518e__v1_text_is_five() {
        assert_eq!(column_size_est("text"), 5);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_column_size_est_c843518e__v2_blob_is_five() {
        assert_eq!(column_size_est("BLOB"), 5);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_column_size_est_c843518e__v3_any_is_five() {
        assert_eq!(column_size_est("Any"), 5);
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_column_size_est_c843518e__v4_a_numeric_or_unknown_type_is_one() {
        assert_eq!(column_size_est("REAL"), 1);
        assert_eq!(column_size_est("CHAR"), 1);
    }

    fn lit(v: &Value, inclusive: bool) -> Option<RangeBound> {
        Some(RangeBound {
            value: Some(v.clone()),
            inclusive,
        })
    }
    fn opaque(inclusive: bool) -> Option<RangeBound> {
        Some(RangeBound {
            value: None,
            inclusive,
        })
    }

    // codegen_row_planner_estimate_range_rows_4eb45b36:
    // `lower_left.is_none() || upper_left.is_none()` (a bound was probed)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_4eb45b36__v1_a_probed_lower_bound_uses_the_histogram(
    ) {
        let s = fixture_stats();
        let v = Value::Integer(4000);
        assert_eq!(
            estimate_range_rows(s.index_samples("ia"), log_est(4096), lit(&v, false), None),
            67
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_4eb45b36__v2_a_probed_upper_bound_uses_the_histogram(
    ) {
        let s = fixture_stats();
        let v = Value::Integer(20);
        // 0..89 rows: the upper probe rounds up to 88 and includes the
        // one equal row.
        assert_eq!(
            estimate_range_rows(s.index_samples("ia"), log_est(4096), None, lit(&v, true)),
            log_est(89)
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_4eb45b36__v3_two_opaque_bounds_ignore_the_histogram(
    ) {
        let s = fixture_stats();
        // Neither bound can be probed: sqlite3's 1/64 default even though
        // samples exist.
        assert_eq!(
            estimate_range_rows(
                s.index_samples("ia"),
                log_est(4096),
                opaque(true),
                opaque(true)
            ),
            log_est(4096) - 60
        );
    }

    // codegen_row_planner_estimate_range_rows_04823cf0:
    // `lwr_idx.is_some() && lwr_idx == upr_idx` (both bounds in one gap)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_04823cf0__v1_both_bounds_in_the_same_gap_take_a_quarter(
    ) {
        let s = fixture_stats();
        let (lo, hi) = (Value::Integer(10), Value::Integer(20));
        // 44..89 rows -> log_est(45) = 55, minus 20 for the shared gap.
        assert_eq!(
            estimate_range_rows(
                s.index_samples("ia"),
                log_est(4096),
                lit(&lo, true),
                lit(&hi, true)
            ),
            35
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_04823cf0__v2_no_lower_probe_means_no_shared_gap(
    ) {
        let s = fixture_stats();
        let hi = Value::Integer(20);
        assert_eq!(
            estimate_range_rows(s.index_samples("ia"), log_est(4096), None, lit(&hi, true)),
            log_est(89)
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_04823cf0__v3_bounds_in_different_gaps_keep_the_full_estimate(
    ) {
        let s = fixture_stats();
        let (lo, hi) = (Value::Integer(10), Value::Integer(3900));
        assert_eq!(
            estimate_range_rows(
                s.index_samples("ia"),
                log_est(4096),
                lit(&lo, true),
                lit(&hi, true)
            ),
            118
        );
    }

    // codegen_row_planner_estimate_range_rows_cccf006f:
    // `lower_left.is_some() && upper_left.is_some()` (closed range default)
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_cccf006f__v1_two_unprobed_bounds_keep_one_sixty_fourth(
    ) {
        assert_eq!(
            estimate_range_rows(None, log_est(4096), opaque(true), opaque(true)),
            log_est(4096) - 60
        );
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_cccf006f__v2_an_unprobed_lower_bound_alone_keeps_a_quarter(
    ) {
        assert_eq!(
            estimate_range_rows(None, log_est(4096), opaque(false), None),
            log_est(4096) - 20
        );
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__codegen_row_planner_estimate_range_rows_cccf006f__v3_an_unprobed_upper_bound_alone_keeps_a_quarter(
    ) {
        assert_eq!(
            estimate_range_rows(None, log_est(4096), None, opaque(false)),
            log_est(4096) - 20
        );
    }
}
