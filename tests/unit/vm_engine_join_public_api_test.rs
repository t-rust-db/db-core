// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `vm::engine`'s pure helpers -- `finalize`,
//! `semi_filter`, `bounded_scan_limit` -- and `vm::join`'s shared
//! infrastructure -- `JoinHashTable`, `should_emit` -- which had zero
//! `tests/unit` references before this (db-core#223): only reached
//! transitively through `vm::engine::run`/`vm::batch::Vm`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::collections::HashSet;

use db_core::vm::batch::{AggPart, Batch, Instruction, JoinKind, Opcode, Program, Value};
use db_core::vm::engine::{bounded_scan_limit, finalize, semi_filter};
use db_core::vm::join::{should_emit, JoinHashTable};

#[test]
fn finalize_merges_groups_sorts_and_limits() {
    let rows = vec![
        vec![Value::Str("a".into()), Value::Int(1)],
        vec![Value::Str("b".into()), Value::Int(5)],
        vec![Value::Str("a".into()), Value::Int(2)],
    ];
    let out = finalize(
        &[AggPart::GroupKey, AggPart::Sum],
        1,
        false,
        None,
        None,
        rows,
    )
    .unwrap();
    // Group "a" merges two rows (1 + 2 = 3, as `f64`); group "b" has only
    // one row, so it never goes through the merge step and keeps its
    // original `Int`.
    assert_eq!(out.len(), 2);
    assert!(out.contains(&vec![Value::Str("a".into()), Value::Float(3.0)]));
    assert!(out.contains(&vec![Value::Str("b".into()), Value::Int(5)]));
}

#[test]
fn finalize_applies_order_by_then_limit() {
    let rows = vec![
        vec![Value::Int(3)],
        vec![Value::Int(1)],
        vec![Value::Int(2)],
    ];
    let out = finalize(&[], 0, false, Some((0, false)), Some(2), rows).unwrap();
    assert_eq!(out, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn semi_filter_keeps_only_rows_whose_key_is_allowed() {
    let batch = Batch::new(3)
        .with_column("id", vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        .with_column(
            "name",
            vec![
                Value::Str("a".into()),
                Value::Str("b".into()),
                Value::Str("c".into()),
            ],
        );
    let allowed: HashSet<String> = ["1".to_string(), "3".to_string()].into_iter().collect();
    let filtered = semi_filter(&batch, "id", &allowed).unwrap();
    assert_eq!(filtered.num_rows, 2);
    assert_eq!(
        filtered.columns.get("id").unwrap(),
        &vec![Value::Int(1), Value::Int(3)]
    );
}

#[test]
fn bounded_scan_limit_recognizes_a_bare_limit_plan_and_declines_others() {
    let bare_limit = Program::new(vec![
        Instruction::new(Opcode::LoadColumn {
            reg: 0,
            column: "id".into(),
        }),
        Instruction::new(Opcode::Emit {
            registers: vec![0].into(),
        }),
        Instruction::new(Opcode::Combine {
            agg_parts: vec![].into(),
            num_group_keys: 0,
            distinct: false,
        }),
        Instruction::new(Opcode::Limit { n: 5 }),
    ]);
    assert_eq!(bounded_scan_limit(&bare_limit), Some(5));

    let with_sort = Program::new(vec![
        Instruction::new(Opcode::Combine {
            agg_parts: vec![].into(),
            num_group_keys: 0,
            distinct: false,
        }),
        Instruction::new(Opcode::Sort {
            col: 0,
            descending: false,
        }),
        Instruction::new(Opcode::Limit { n: 5 }),
    ]);
    assert_eq!(bounded_scan_limit(&with_sort), None);

    let with_filter = Program::new(vec![
        Instruction::new(Opcode::Filter { predicate: 0 }),
        Instruction::new(Opcode::Combine {
            agg_parts: vec![].into(),
            num_group_keys: 0,
            distinct: false,
        }),
        Instruction::new(Opcode::Limit { n: 5 }),
    ]);
    assert_eq!(bounded_scan_limit(&with_filter), None);
}

#[test]
fn join_hash_table_inserts_and_probes_a_multimap() {
    let mut table: JoinHashTable<i64, &str> = JoinHashTable::new();
    table.insert(1, "a");
    table.insert(1, "b");
    table.insert(2, "c");

    assert_eq!(table.len(), 3);
    assert!(table.contains_key(&1));
    assert!(!table.contains_key(&3));
    let mut all_for_one = table.get_all(&1);
    all_for_one.sort_unstable();
    assert_eq!(all_for_one, vec![&"a", &"b"]);
    assert_eq!(table.get(&2), Some(&"c"));
}

#[test]
fn should_emit_matches_every_join_kind_semantics() {
    assert!(should_emit(JoinKind::Inner, true, true));
    assert!(!should_emit(JoinKind::Inner, true, false));
    assert!(should_emit(JoinKind::Left, false, false));
    assert!(should_emit(JoinKind::Right, false, true));
    assert!(should_emit(JoinKind::Full, false, false));
    assert!(should_emit(JoinKind::Semi, true, false));
    assert!(!should_emit(JoinKind::Semi, false, false));
    assert!(should_emit(JoinKind::Anti, false, false));
    assert!(!should_emit(JoinKind::Anti, true, false));
}
