//! Spike for db-core#183 (a #130 "cheap proof" child): does replacing
//! `Opcode::GroupReduce`'s per-row `to_string`/`join`/`HashMap<String,
//! usize>` group key with a typed, per-`Value`-variant `Hash` key (the
//! same trick `Opcode::HashBuild`/`HashProbe`'s `JoinKey`, `src/vm/batch.rs`,
//! already uses) actually deliver the 5x-20x #130 hypothesizes for this
//! opcode?
//!
//! This spike makes NO production code changes (mirrors #141's convention).
//! It reimplements both key strategies standalone, against the exact same
//! synthetic `Vec<Value>` key columns `GroupReduce`'s real implementation
//! would see, and times grouping `N` rows into `G` groups under both.
//!
//! `GroupKey` here is deliberately NOT `JoinKey`: `JoinKey`'s `PartialEq`
//! treats any key containing `Null` as never equal to anything (correct
//! for join equality, `NULL != NULL`) -- wrong for `GROUP BY`, which groups
//! `NULL`s together. `GroupKey` uses `Value`'s own derived `PartialEq`
//! (`Null == Null`, a fieldless variant) instead.
//!
//! # Running
//!
//! Debug builds under-count allocator overhead relative to a real
//! `--release` build, so this spike is `#[ignore]`d by default:
//!
//! ```sh
//! cargo test --release --test 01_groupreduce_key_hashing -- --ignored --nocapture
//! # or: make -C tests/spike/002_groupreduce_key_hashing run
//! ```

// This spike measures raw hashing/allocation cost (that's the entire
// point of the comparison) -- the crate-wide `[lints.clippy]` bar (#82)
// would otherwise force bounds checks and checked arithmetic that
// change the very thing being timed. Mirrors 001_neon_batch_kernels'
// own rationale for this same allow-list.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use db_core::vm::batch::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::time::{Duration, Instant};

/// A `GroupReduce` key: like `JoinKey` in shape, but `Null == Null`
/// (`GROUP BY` semantics), not `JoinKey`'s NULL-poisoned join semantics.
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

/// Builds `num_rows` rows over `num_key_cols` key columns, cycling through
/// `cardinality` distinct group identities -- shaped like a real
/// `GROUP BY a, b` over integer/string columns at a fixed selectivity.
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

/// Today's actual `GroupReduce` key strategy, verbatim: clone each key
/// `Value` per row, `to_string` each, `join` with a NUL separator, hash
/// the resulting `String`.
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

/// The proposed strategy: a typed, per-variant `Hash`/`Eq` key -- same
/// `Vec<Value>` clone, no stringification.
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

fn time_it<F: FnMut() -> usize>(mut f: F, iterations: u32) -> (Duration, usize) {
    let start = Instant::now();
    let mut last = 0;
    for _ in 0..iterations {
        last = black_box(f());
    }
    (start.elapsed(), last)
}

#[test]
fn stringify_and_typed_keys_agree_on_group_count() {
    // Correctness check (runs under `cargo test`, no --ignored needed):
    // both strategies must produce the same number of distinct groups,
    // including when a key column carries NULLs.
    let mut cols = synthetic_key_columns(1000, 2, 37);
    cols[0][5] = Value::Null;
    cols[0][19] = Value::Null; // two NULL rows -- must land in one group
    let stringify = group_by_stringify(&cols, 1000);
    let typed = group_by_typed(&cols, 1000);
    assert_eq!(stringify, typed);
}

#[test]
#[ignore = "perf spike -- run explicitly with --release --ignored --nocapture"]
fn groupreduce_key_hashing_stringify_vs_typed() {
    const ROW_COUNTS: [usize; 3] = [10_000, 100_000, 1_000_000];
    const CARDINALITY_FRACTIONS: [(&str, usize); 3] = [
        ("low (100 groups)", 100),
        ("medium (1% of rows)", 0),
        ("high (unique)", 0),
    ];
    const NUM_KEY_COLS: usize = 2;

    println!(
        "\n{:>10} {:>22} {:>12} {:>14} {:>14} {:>8}",
        "rows", "cardinality", "iterations", "stringify", "typed", "speedup"
    );

    for &rows in &ROW_COUNTS {
        for &(label, fixed_cardinality) in &CARDINALITY_FRACTIONS {
            let cardinality = match fixed_cardinality {
                0 if label.starts_with("medium") => (rows / 100).max(1),
                0 => rows, // "high (unique)"
                n => n,
            };
            let cols = synthetic_key_columns(rows, NUM_KEY_COLS, cardinality);

            // Fewer iterations for larger row counts so the whole spike
            // finishes in a few seconds, not minutes.
            let iterations = if rows >= 1_000_000 {
                3
            } else if rows >= 100_000 {
                10
            } else {
                50
            };

            let (stringify_time, stringify_groups) =
                time_it(|| group_by_stringify(&cols, rows), iterations);
            let (typed_time, typed_groups) = time_it(|| group_by_typed(&cols, rows), iterations);
            assert_eq!(stringify_groups, typed_groups);

            let speedup = stringify_time.as_secs_f64() / typed_time.as_secs_f64();
            println!(
                "{:>10} {:>22} {:>12} {:>14?} {:>14?} {:>7.2}x",
                rows,
                format!("{label} ({cardinality})"),
                iterations,
                stringify_time / iterations,
                typed_time / iterations,
                speedup
            );
        }
    }
}
