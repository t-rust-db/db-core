// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! sqlite3's math functions (`SQLITE_ENABLE_MATH_FUNCTIONS`, `func.c`)
//! (#651), with sqlite3 3.53.4's semantics: an argument is read as a
//! number only when it is one -- an INTEGER, a REAL, or text that is
//! entirely numeric -- and anything else (NULL, a blob, `'abc'`) gives
//! NULL. Every result is a REAL, except `ceil`/`floor`/`trunc` of an
//! INTEGER, which return it unchanged; a NaN result is NULL.

use super::{FunctionError, ScalarFn};
use crate::value::Value;

/// The function named `name` (lowercase) at `arity`, if this family has
/// it.
pub(super) fn lookup(name: &str, arity: usize) -> Option<ScalarFn> {
    let f: ScalarFn = match (name, arity) {
        ("acos", 1) => |a| unary(a, f64::acos),
        ("acosh", 1) => |a| unary(a, f64::acosh),
        ("asin", 1) => |a| unary(a, f64::asin),
        ("asinh", 1) => |a| unary(a, f64::asinh),
        ("atan", 1) => |a| unary(a, f64::atan),
        ("atanh", 1) => |a| unary(a, f64::atanh),
        ("cos", 1) => |a| unary(a, f64::cos),
        ("cosh", 1) => |a| unary(a, f64::cosh),
        ("sin", 1) => |a| unary(a, f64::sin),
        ("sinh", 1) => |a| unary(a, f64::sinh),
        ("tan", 1) => |a| unary(a, f64::tan),
        ("tanh", 1) => |a| unary(a, f64::tanh),
        ("exp", 1) => |a| unary(a, f64::exp),
        ("sqrt", 1) => |a| unary(a, f64::sqrt),
        ("degrees", 1) => |a| unary(a, f64::to_degrees),
        ("radians", 1) => |a| unary(a, f64::to_radians),
        // sqlite3's logarithms are NULL at and below zero, not -Inf.
        ("ln", 1) => |a| unary(a, |x| positive(x).map_or(f64::NAN, f64::ln)),
        ("log10", 1) => |a| unary(a, |x| positive(x).map_or(f64::NAN, f64::log10)),
        ("log2", 1) => |a| unary(a, |x| positive(x).map_or(f64::NAN, f64::log2)),
        ("log", 1) => |a| unary(a, |x| positive(x).map_or(f64::NAN, f64::log10)),
        ("log", 2) => log_base,
        ("ceil" | "ceiling", 1) => |a| rounding(a, f64::ceil),
        ("floor", 1) => |a| rounding(a, f64::floor),
        ("trunc", 1) => |a| rounding(a, f64::trunc),
        ("atan2", 2) => |a| binary(a, f64::atan2),
        ("pow" | "power", 2) => |a| binary(a, f64::powf),
        // `fmod`: the remainder keeps the dividend's sign; by zero, NULL.
        ("mod", 2) => |a| binary(a, |x, y| if y == 0.0 { f64::NAN } else { x % y }),
        ("pi", 0) => |_| Ok(Value::Real(std::f64::consts::PI)),
        _ => return None,
    };
    Some(f)
}

/// `v` as sqlite3 reads a math function's argument, or `None` (→ NULL).
fn number(v: &Value) -> Option<Value> {
    match v {
        Value::Integer(_) | Value::Real(_) => Some(v.clone()),
        Value::Text(s) => crate::coerce::numeric_text(s),
        Value::Null | Value::Blob(_) => None,
    }
}

/// `number(v)` as a double.
fn real(v: &Value) -> Option<f64> {
    match number(v)? {
        #[allow(
            clippy::cast_precision_loss,
            reason = "math functions compute in doubles, as sqlite3's do"
        )]
        Value::Integer(i) => Some(i as f64),
        Value::Real(r) => Some(r),
        _ => None,
    }
}

/// `x` when it is above zero.
fn positive(x: f64) -> Option<f64> {
    (x > 0.0).then_some(x)
}

/// A REAL result; NaN (a domain error) is NULL, and `-0.0` is `0.0`
/// (sqlite3 never shows a negative zero: `trunc(-0.5)` is `0.0`).
fn result(x: f64) -> Value {
    if x.is_nan() {
        Value::Null
    } else {
        Value::Real(x + 0.0)
    }
}

fn unary(args: &[Value], f: fn(f64) -> f64) -> Result<Value, FunctionError> {
    Ok(args
        .first()
        .and_then(real)
        .map_or(Value::Null, |x| result(f(x))))
}

fn binary(args: &[Value], f: fn(f64, f64) -> f64) -> Result<Value, FunctionError> {
    Ok(
        match (args.first().and_then(real), args.get(1).and_then(real)) {
            (Some(x), Some(y)) => result(f(x, y)),
            _ => Value::Null,
        },
    )
}

/// `log(B, X)`: the base-`B` logarithm of `X`; NULL unless both are above
/// zero and `B` is not 1.
fn log_base(args: &[Value]) -> Result<Value, FunctionError> {
    binary(args, |b, x| match (positive(b), positive(x)) {
        (Some(b), Some(x)) if b != 1.0 => x.ln() / b.ln(),
        _ => f64::NAN,
    })
}

/// `ceil`/`floor`/`trunc`: an INTEGER is returned as is, anything else
/// rounded as a REAL.
fn rounding(args: &[Value], f: fn(f64) -> f64) -> Result<Value, FunctionError> {
    Ok(match args.first().and_then(number) {
        Some(Value::Integer(i)) => Value::Integer(i),
        Some(Value::Real(r)) => result(f(r)),
        _ => Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::super::call;
    use crate::value::Value;

    fn t(s: &str) -> Value {
        Value::Text(s.to_string().into())
    }

    /// Expectations from sqlite3 3.53.4.
    #[test]
    fn math_functions_match_sqlite3() {
        let r = Value::Real;
        for (name, args, want) in [
            ("ceil", vec![Value::Integer(5)], Value::Integer(5)),
            ("ceil", vec![r(-1.5)], r(-1.0)),
            ("ceil", vec![t("2.1")], r(3.0)),
            ("ceil", vec![t("abc")], Value::Null),
            ("ceil", vec![t("12abc")], Value::Null),
            ("ceil", vec![t(" 7 ")], Value::Integer(7)),
            ("ceil", vec![Value::Blob(vec![1].into())], Value::Null),
            ("floor", vec![r(-1.5)], r(-2.0)),
            ("floor", vec![t("-3")], Value::Integer(-3)),
            ("trunc", vec![r(-1.7)], r(-1.0)),
            ("sqrt", vec![r(-1.0)], Value::Null),
            ("sqrt", vec![t(" 4 ")], r(2.0)),
            ("ln", vec![Value::Integer(0)], Value::Null),
            ("log", vec![Value::Integer(100)], r(2.0)),
            ("log", vec![Value::Integer(2), Value::Integer(8)], r(3.0)),
            (
                "log",
                vec![Value::Integer(1), Value::Integer(5)],
                Value::Null,
            ),
            ("log10", vec![Value::Integer(1000)], r(3.0)),
            ("log2", vec![Value::Integer(8)], r(3.0)),
            ("mod", vec![Value::Integer(-7), Value::Integer(3)], r(-1.0)),
            (
                "mod",
                vec![Value::Integer(7), Value::Integer(0)],
                Value::Null,
            ),
            (
                "pow",
                vec![Value::Integer(0), Value::Integer(-1)],
                r(f64::INFINITY),
            ),
            ("pow", vec![r(-8.0), r(1.0 / 3.0)], Value::Null),
            ("acos", vec![Value::Integer(2)], Value::Null),
            ("atanh", vec![Value::Integer(1)], r(f64::INFINITY)),
            (
                "atan2",
                vec![Value::Integer(0), Value::Integer(-1)],
                r(std::f64::consts::PI),
            ),
            ("atan2", vec![t("1"), Value::Null], Value::Null),
            ("degrees", vec![r(std::f64::consts::PI)], r(180.0)),
            ("pi", vec![], r(std::f64::consts::PI)),
        ] {
            assert_eq!(call(name, &args).unwrap(), want, "{name}({args:?})");
        }
    }
}
