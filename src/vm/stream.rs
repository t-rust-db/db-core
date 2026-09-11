// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `vm::stream` -- the opcodes `vm::batch` has no notion of (ADR 0018
//! §Opcodes). `Prune` (segment selection before any row is materialized)
//! and `Window`/`RangeAgg`/`Watermark` (#308, range-vector aggregation)
//! are implemented; `Emit`/standing queries are #309.
//!
//! A stream [`Program`] is a prologue (`Prune`) around a `Body` reused
//! verbatim from [`crate::vm::batch`], plus an optional epilogue
//! (`Window`+`RangeAgg`, `Watermark`) for range-vector queries:
//! `codegen::stream::compile` builds one, `engine::stream::StreamEngine`
//! drives the prologue itself (selecting segments) before handing the
//! body to `vm::batch::Vm::execute`, then the epilogue (if any) to
//! [`run_epilogue`].

use std::time::Duration;

use crate::vm::batch::Value;

/// How far back from EOF a stream query reaches (ADR 0018 §Scope and
/// retention). Per query; retention (what the ring holds hot) is the
/// ring's own business.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    /// A duration back from the latest observed/event time.
    Time(Duration),
    /// The last `n` lines.
    Lines(u64),
    /// The last `n` bytes.
    Bytes(u64),
    /// Every held/reachable segment; well-defined only for non-blocking
    /// queries in follow mode (ADR 0018 §Planner point 1).
    All,
}

/// A segment-skipping predicate `Prune` selects on: a leaf `codegen::
/// stream::compile` lifted out of `WHERE`/`SINCE`/`UNTIL` because
/// `storage::stream::Segment` can answer it without materializing a row
/// (minmax for time ranges, dictionaries for `=`).
#[derive(Debug, Clone, PartialEq)]
pub enum IndexPred {
    /// Event-time range `[lo, hi)`, nanoseconds since epoch.
    TimeRange {
        /// Inclusive lower bound.
        lo: i64,
        /// Exclusive upper bound.
        hi: i64,
    },
    /// `column = value` on a dictionary-encoded Tier-3/Tier-2b column.
    DictEq {
        /// The column name.
        column: String,
        /// The literal value compared against.
        value: String,
    },
}

/// Segment selection before any row is materialized (ADR 0018 §Opcodes).
#[derive(Debug, Clone, PartialEq)]
pub struct Prune {
    /// The scope this query is bounded to.
    pub scope: Scope,
    /// Index-pushable predicates, all of which must hold (AND) for a
    /// segment to survive pruning.
    pub preds: Vec<IndexPred>,
}

/// A compiled stream query: a `Prune` prologue plus a `vm::batch::Program`
/// body reused verbatim (ADR 0018's core claim -- filter/project/
/// aggregate/sort/limit need no stream-specific reimplementation), plus
/// an optional range-vector epilogue (#308).
#[derive(Debug, Clone)]
pub struct Program {
    /// Segment-selection prologue.
    pub prune: Prune,
    /// The residual query, compiled by `codegen::batch::compile` and run
    /// unchanged per surviving segment. For a range-vector query (#308)
    /// this projects just `(event_ts, value)` -- the epilogue does the
    /// actual bucketing/reduction, not `vm::batch`.
    pub body: crate::vm::batch::Program,
    /// Present only for a range-vector query (`count_over_time`/`rate`/
    /// `*_over_time`): tumbling windows over `body`'s output.
    pub epilogue: Option<Epilogue>,
}

/// One tumbling window spec: `size` wide, stepped by `size` (hopping --
/// `step < size` -- has no SQL surface yet; `key`ed grouping likewise; both
/// flagged in ADR 0018 §Opcodes as in scope for `Window` but not built
/// here since #308's grammar only exposes a plain `RANGE <duration>`,
/// #308 scope note).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// Window width.
    pub size: Duration,
    /// Step between window starts (`== size` for every #308 query: no
    /// hopping-window SQL surface exists yet).
    pub step: Duration,
}

/// A range-vector function's reduction, applied per window
/// (`AggPart`-style semantics, but over a window's rows rather than a
/// segment's -- no cross-window merge is needed since a window is
/// self-contained by construction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeAggFunc {
    /// `count_over_time`: rows in the window.
    Count,
    /// `sum_over_time`: sum of the window's values.
    Sum,
    /// `avg_over_time`: mean of the window's values.
    Avg,
    /// `min_over_time`: smallest value in the window.
    Min,
    /// `max_over_time`: largest value in the window.
    Max,
    /// `rate`: `count_over_time` divided by the window's `size` in
    /// seconds (Loki's `rate`: events per second over the range).
    Rate,
}

impl RangeAggFunc {
    /// Parses `count_over_time`/`rate`/`sum_over_time`/`avg_over_time`/
    /// `min_over_time`/`max_over_time`, case-insensitively; `None` for
    /// anything else.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "count_over_time" => Some(Self::Count),
            "sum_over_time" => Some(Self::Sum),
            "avg_over_time" => Some(Self::Avg),
            "min_over_time" => Some(Self::Min),
            "max_over_time" => Some(Self::Max),
            "rate" => Some(Self::Rate),
            _ => None,
        }
    }
}

/// Close windows at `max(event_ts) - grace` (ADR 0018 §Opcodes): a row
/// older than the closed boundary when it arrives is late, and
/// [`run_epilogue`] routes it to a side output instead of a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Watermark {
    /// How far behind the latest seen event time a window stays open.
    pub grace: Duration,
}

/// `Window`+`RangeAgg` (and, if present, `Watermark`) as one epilogue
/// unit -- `RangeAgg` has no independent meaning without a `Window` to
/// bucket by, so `codegen::stream` always emits both together.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Epilogue {
    /// Tumbling-window bucketing.
    pub window: Window,
    /// The per-window reduction.
    pub func: RangeAggFunc,
    /// Late-line routing, if the query asked for it. #308 always sets a
    /// grace of one window `size` when absent from the SQL surface -- no
    /// `WATERMARK` clause exists yet to make grace independently
    /// tunable.
    pub watermark: Watermark,
}

/// One row surviving `body`: the event timestamp `RangeAgg` buckets by,
/// and the raw value it reduces (an integer/float column for `sum`/`avg`/
/// `min`/`max`; ignored -- may be [`Value::Null`] -- for `count`/`rate`).
pub type EpilogueRow = (i64, Value);

/// One windowed result: `(window_start_ns, reduced_value)`.
pub type WindowResult = (i64, Value);

/// Buckets `rows` into `epilogue.window`-wide tumbling windows keyed by
/// event timestamp, reduces each bucket with `epilogue.func`, and
/// returns `(on_time, late)`: `on_time` is every window whose bucket
/// closed no earlier than `max(event_ts) - watermark.grace` (ADR 0018:
/// "close windows at `max(event_ts) - grace`"), sorted by window start;
/// `late` is every row that landed in an already-closed window, in
/// input order, untouched by any reduction (the side output ADR 0018
/// calls for, rather than silently dropped or merged into the wrong
/// window).
#[must_use]
pub fn run_epilogue(
    rows: &[EpilogueRow],
    epilogue: &Epilogue,
) -> (Vec<WindowResult>, Vec<EpilogueRow>) {
    let size_ns = duration_ns(epilogue.window.size);
    if size_ns <= 0 {
        return (Vec::new(), rows.to_vec());
    }
    let Some(max_ts) = rows.iter().map(|(ts, _)| *ts).max() else {
        return (Vec::new(), Vec::new());
    };
    let watermark = max_ts.saturating_sub(duration_ns(epilogue.watermark.grace));

    let mut buckets: std::collections::BTreeMap<i64, Vec<Value>> =
        std::collections::BTreeMap::new();
    let mut late = Vec::new();
    for &(ts, ref value) in rows {
        let window_start = ts.div_euclid(size_ns).saturating_mul(size_ns);
        let window_end = window_start.saturating_add(size_ns);
        if window_end <= watermark {
            late.push((ts, value.clone()));
        } else {
            buckets.entry(window_start).or_default().push(value.clone());
        }
    }

    let results = buckets
        .into_iter()
        .map(|(start, values)| {
            (
                start,
                reduce_window(epilogue.func, &values, epilogue.window.size),
            )
        })
        .collect();
    (results, late)
}

fn duration_ns(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

fn reduce_window(func: RangeAggFunc, values: &[Value], size: Duration) -> Value {
    match func {
        RangeAggFunc::Count => Value::Int(i64::try_from(values.len()).unwrap_or(i64::MAX)),
        RangeAggFunc::Rate => {
            let secs = size.as_secs_f64();
            if secs <= 0.0 {
                Value::Null
            } else {
                Value::Float(values.len() as f64 / secs)
            }
        }
        RangeAggFunc::Sum => Value::Float(values.iter().filter_map(Value::as_f64).sum()),
        RangeAggFunc::Avg => {
            let nums: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
            if nums.is_empty() {
                Value::Null
            } else {
                Value::Float(nums.iter().sum::<f64>() / nums.len() as f64)
            }
        }
        RangeAggFunc::Min => values
            .iter()
            .filter_map(Value::as_f64)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.min(v)))
            })
            .map_or(Value::Null, Value::Float),
        RangeAggFunc::Max => values
            .iter()
            .filter_map(Value::as_f64)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            })
            .map_or(Value::Null, Value::Float),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test code fails fast (db-core#230)")]
mod tests {
    use super::*;

    fn secs(n: i64) -> i64 {
        n.saturating_mul(1_000_000_000)
    }

    fn epilogue(func: RangeAggFunc, size_secs: i64, grace_secs: i64) -> Epilogue {
        Epilogue {
            window: Window {
                size: Duration::from_secs(size_secs as u64),
                step: Duration::from_secs(size_secs as u64),
            },
            func,
            watermark: Watermark {
                grace: Duration::from_secs(grace_secs as u64),
            },
        }
    }

    #[test]
    fn count_over_time_buckets_into_tumbling_windows() {
        let rows: Vec<EpilogueRow> = vec![
            (secs(1), Value::Null),
            (secs(2), Value::Null),
            (secs(11), Value::Null),
            (secs(12), Value::Null),
            (secs(13), Value::Null),
        ];
        // Grace covers the whole spread so no window closes "late"
        // relative to the batch's own newest row here -- the watermark
        // boundary itself is exercised by its own dedicated test below.
        let ep = epilogue(RangeAggFunc::Count, 10, 10);
        let (windows, late) = run_epilogue(&rows, &ep);
        assert!(late.is_empty());
        assert_eq!(windows, vec![(0, Value::Int(2)), (secs(10), Value::Int(3))]);
    }

    #[test]
    fn rate_divides_count_by_window_size_seconds() {
        let rows: Vec<EpilogueRow> = vec![(secs(0), Value::Null), (secs(1), Value::Null)];
        let ep = epilogue(RangeAggFunc::Rate, 10, 0);
        let (windows, _) = run_epilogue(&rows, &ep);
        assert_eq!(windows, vec![(0, Value::Float(0.2))]);
    }

    #[test]
    fn sum_and_avg_and_min_and_max_reduce_the_bucketed_values() {
        let rows: Vec<EpilogueRow> = vec![
            (secs(0), Value::Int(10)),
            (secs(1), Value::Int(20)),
            (secs(2), Value::Int(30)),
        ];
        assert_eq!(
            run_epilogue(&rows, &epilogue(RangeAggFunc::Sum, 10, 0)).0,
            vec![(0, Value::Float(60.0))]
        );
        assert_eq!(
            run_epilogue(&rows, &epilogue(RangeAggFunc::Avg, 10, 0)).0,
            vec![(0, Value::Float(20.0))]
        );
        assert_eq!(
            run_epilogue(&rows, &epilogue(RangeAggFunc::Min, 10, 0)).0,
            vec![(0, Value::Float(10.0))]
        );
        assert_eq!(
            run_epilogue(&rows, &epilogue(RangeAggFunc::Max, 10, 0)).0,
            vec![(0, Value::Float(30.0))]
        );
    }

    #[test]
    fn a_row_behind_the_watermark_lands_in_late_not_a_window() {
        // max_ts = 25s; grace = 5s -> watermark = 20s. The window
        // [0,10) ends at 10 <= 20, so a row landing in it is late.
        let rows: Vec<EpilogueRow> = vec![
            (secs(1), Value::Null),  // window [0,10), ends at 10 <= 20 -> late
            (secs(25), Value::Null), // window [20,30), the newest row
        ];
        let ep = epilogue(RangeAggFunc::Count, 10, 5);
        let (windows, late) = run_epilogue(&rows, &ep);
        assert_eq!(late, vec![(secs(1), Value::Null)]);
        assert_eq!(windows, vec![(secs(20), Value::Int(1))]);
    }

    #[test]
    fn no_rows_yields_no_windows_and_no_late_lines() {
        assert_eq!(
            run_epilogue(&[], &epilogue(RangeAggFunc::Count, 10, 0)),
            (Vec::new(), Vec::new())
        );
    }
}
