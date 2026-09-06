//! Round 2 of the db-core#183 spike -- supersedes 01 for drawing
//! conclusions on the high-cardinality result. 01 found a real ~2-3x win
//! for the typed key at low/medium `GROUP BY` cardinality, but an
//! unexpected *regression* (0.71x) at the highest cardinality tested (1M
//! rows, 1M distinct groups) -- flagged there as possibly noise from only
//! 3 iterations at that scale. This round re-measures just that case with:
//!
//! - a warm-up pass before timing (lets allocator/`HashMap` growth
//!   patterns settle before the clock starts, so the first iteration's
//!   cold-cache/first-fault cost doesn't skew a small iteration count)
//! - many more iterations at the 1M-row scale (20 instead of 3), and a
//!   matching pass at 5M rows to see whether the regression widens,
//!   narrows, or reverses as cardinality grows further
//!
//! 01 is kept as-is rather than edited in place, so the review comment
//! that motivated round 2 stays legible against the code it was about.
//!
//! # Running
//!
//! ```sh
//! cargo test --release --test 02_groupreduce_key_hashing_adjusted -- --ignored --nocapture
//! # or: make -C tests/spike/002_arrow_optimization run-02
//! ```

// Same rationale as 01_groupreduce_key_hashing.rs: this spike measures
// raw hashing/allocation cost, so the crate-wide `[lints.clippy]` bar
// (#82) would change the very thing being timed.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use db_core::vm::batch::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Identical to 01's `GroupKey` -- `Null == Null` (`GROUP BY` semantics),
/// not `JoinKey`'s NULL-poisoned join semantics.
#[derive(Debug, Clone, PartialEq)]
struct GroupKey(Vec<Value>);

impl Eq for GroupKey {}

impl Hash for GroupKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for value in &self.0 {
            match value {
                Value::Int(v) => {
                    0u8.hash(state);
                    v.hash(state);
                }
                Value::Float(v) => {
                    1u8.hash(state);
                    v.to_bits().hash(state);
                }
                Value::Bool(v) => {
                    2u8.hash(state);
                    v.hash(state);
                }
                Value::Str(v) => {
                    3u8.hash(state);
                    v.hash(state);
                }
                Value::Null => 4u8.hash(state),
            }
        }
    }
}

/// Identical to 01's `synthetic_key_columns`.
fn synthetic_key_columns(
    num_rows: usize,
    num_key_cols: usize,
    cardinality: usize,
) -> Vec<Vec<Value>> {
    (0..num_key_cols)
        .map(|col| {
            (0..num_rows)
                .map(|row| {
                    let group = (row + col * 7) % cardinality;
                    if col.is_multiple_of(2) {
                        Value::Int(group as i64)
                    } else {
                        Value::Str(format!("group-{group}").into())
                    }
                })
                .collect()
        })
        .collect()
}

/// Identical to 01's `group_by_stringify`.
fn group_by_stringify(key_columns: &[Vec<Value>], num_rows: usize) -> usize {
    let mut group_index: HashMap<String, usize> = HashMap::new();
    let mut num_groups = 0usize;
    for row in 0..num_rows {
        let key: Vec<Value> = key_columns.iter().map(|c| c[row].clone()).collect();
        let key_str = key
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\u{0}");
        group_index.entry(key_str).or_insert_with(|| {
            let id = num_groups;
            num_groups += 1;
            id
        });
    }
    num_groups
}

/// Identical to 01's `group_by_typed`.
fn group_by_typed(key_columns: &[Vec<Value>], num_rows: usize) -> usize {
    let mut group_index: HashMap<GroupKey, usize> = HashMap::new();
    let mut num_groups = 0usize;
    for row in 0..num_rows {
        let key = GroupKey(key_columns.iter().map(|c| c[row].clone()).collect());
        group_index.entry(key).or_insert_with(|| {
            let id = num_groups;
            num_groups += 1;
            id
        });
    }
    num_groups
}

/// Like 01's `time_it`, but runs `warmup` untimed passes first.
fn time_it_warmed<F: FnMut() -> usize>(
    mut f: F,
    warmup: u32,
    iterations: u32,
) -> (Duration, usize) {
    for _ in 0..warmup {
        black_box(f());
    }
    let start = Instant::now();
    let mut last = 0;
    for _ in 0..iterations {
        last = black_box(f());
    }
    (start.elapsed(), last)
}

#[test]
fn stringify_and_typed_keys_agree_on_group_count() {
    // Same correctness check as round 1, kept here too so this file is
    // self-contained and can be run/read independently of 01.
    let mut cols = synthetic_key_columns(1000, 2, 37);
    cols[0][5] = Value::Null;
    cols[0][19] = Value::Null;
    let stringify = group_by_stringify(&cols, 1000);
    let typed = group_by_typed(&cols, 1000);
    assert_eq!(stringify, typed);
}

#[test]
#[ignore = "perf spike -- run explicitly with --release --ignored --nocapture"]
fn high_cardinality_regression_with_more_iterations_and_warmup() {
    const NUM_KEY_COLS: usize = 2;
    const WARMUP: u32 = 3;
    const ITERATIONS: u32 = 20;

    println!(
        "\n{:>10} {:>16} {:>12} {:>14} {:>14} {:>8}",
        "rows", "cardinality", "iterations", "stringify", "typed", "speedup"
    );

    // 1M rows, unique keys: round 1's flagged case, rerun with 20
    // iterations (vs. 3) and a 3-iteration warm-up.
    for &rows in &[1_000_000usize, 5_000_000usize] {
        let cardinality = rows; // every row its own group
        let cols = synthetic_key_columns(rows, NUM_KEY_COLS, cardinality);

        let (stringify_time, stringify_groups) =
            time_it_warmed(|| group_by_stringify(&cols, rows), WARMUP, ITERATIONS);
        let (typed_time, typed_groups) =
            time_it_warmed(|| group_by_typed(&cols, rows), WARMUP, ITERATIONS);
        assert_eq!(stringify_groups, typed_groups);

        let speedup = stringify_time.as_secs_f64() / typed_time.as_secs_f64();
        println!(
            "{:>10} {:>16} {:>12} {:>14?} {:>14?} {:>7.2}x",
            rows,
            "unique",
            ITERATIONS,
            stringify_time / ITERATIONS,
            typed_time / ITERATIONS,
            speedup
        );
    }
}
