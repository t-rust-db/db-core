// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for db-core's five root modules -- `types`, `value`,
//! `compare`, `coerce`, `functions` -- which had zero `tests/unit`
//! references before this (db-core#223): coverage came only
//! incidentally through whichever executor happened to call them.
//! `Collation` in particular was only ever reached via the `vm::row`
//! re-export, never `db_core::value::Collation` directly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use db_core::coerce::{
    bit_and, bit_not, bit_or, cast_to_integer, checked_add, checked_div, checked_mul, checked_rem,
    checked_sub, coerce_text_to_numeric, concat, shift_left, shift_right,
};
use db_core::compare::compare;
use db_core::functions::{call, glob_match, like_match, FunctionError};
use db_core::types::{Literal, Value as TypesValue};
use db_core::value::{compare_text, format_real, Collation, Value};

#[test]
fn types_literal_converts_to_value() {
    assert_eq!(TypesValue::from(Literal::Int(5)), TypesValue::Int(5));
    assert_eq!(
        TypesValue::from(Literal::Str("x".into())),
        TypesValue::Str("x".into())
    );
    assert_eq!(
        TypesValue::from(Literal::Float(1.5)),
        TypesValue::Float(1.5)
    );
}

#[test]
fn value_collation_and_format_real_are_reachable_from_the_crate_root() {
    assert_eq!(
        compare_text("ABC", "abc", Collation::NoCase),
        std::cmp::Ordering::Equal,
    );
    assert_ne!(
        compare_text("ABC", "abc", Collation::Binary),
        std::cmp::Ordering::Equal,
    );
    assert_eq!(format_real(1.0), "1.0");
    let v = Value::Integer(5);
    assert_eq!(v, Value::Integer(5));
}

#[test]
fn compare_orders_values_under_a_collation() {
    assert_eq!(
        compare(&Value::Integer(1), &Value::Integer(2), Collation::Binary),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        compare(
            &Value::Text("ABC".into()),
            &Value::Text("abc".into()),
            Collation::NoCase
        ),
        std::cmp::Ordering::Equal
    );
}

#[test]
fn coerce_arithmetic_and_bitwise_helpers() {
    assert_eq!(
        coerce_text_to_numeric("42abc"),
        Value::Integer(42) // leading numeric prefix
    );
    assert_eq!(
        checked_add(&Value::Integer(1), &Value::Integer(2)),
        Value::Integer(3)
    );
    assert_eq!(
        checked_sub(&Value::Integer(5), &Value::Integer(2)),
        Value::Integer(3)
    );
    assert_eq!(
        checked_mul(&Value::Integer(3), &Value::Integer(4)),
        Value::Integer(12)
    );
    assert_eq!(
        checked_div(&Value::Integer(10), &Value::Integer(2)),
        Value::Integer(5)
    );
    assert_eq!(
        checked_rem(&Value::Integer(10), &Value::Integer(3)),
        Value::Integer(1)
    );
    assert_eq!(cast_to_integer(&Value::Real(3.9)), 3);
    assert_eq!(
        bit_and(&Value::Integer(0b110), &Value::Integer(0b011)),
        Value::Integer(0b010)
    );
    assert_eq!(
        bit_or(&Value::Integer(0b100), &Value::Integer(0b001)),
        Value::Integer(0b101)
    );
    assert_eq!(bit_not(&Value::Integer(0)), Value::Integer(-1));
    assert_eq!(
        shift_left(&Value::Integer(1), &Value::Integer(3)),
        Value::Integer(8)
    );
    assert_eq!(
        shift_right(&Value::Integer(8), &Value::Integer(3)),
        Value::Integer(1)
    );
    assert_eq!(
        concat(&Value::Text("a".into()), &Value::Text("b".into())),
        Value::Text("ab".into())
    );
}

#[test]
fn functions_call_dispatches_like_glob_and_reports_unknown() {
    assert_eq!(
        call("upper", &[Value::Text("abc".into())]).unwrap(),
        Value::Text("ABC".into())
    );
    assert!(like_match("abc", "a%", None));
    assert!(!like_match("abc", "x%", None));
    assert!(glob_match("abc", "a*"));
    assert!(!glob_match("abc", "x*"));

    let err = call("no_such_function", &[]).unwrap_err();
    match err {
        FunctionError::Unknown { name, arity } => {
            assert_eq!(name, "no_such_function");
            assert_eq!(arity, 0);
        }
        other => panic!("expected FunctionError::Unknown, got {other:?}"),
    }
}
