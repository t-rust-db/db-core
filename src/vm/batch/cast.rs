// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `CAST(x AS VARCHAR|INTEGER|BIGINT|DOUBLE)` in the column dialect, with
//! DuckDB's answers (#948, ADR 0030). The engine rewrites a cast to one of
//! the `__cast_*` calls below (a TIMESTAMP's own casts are #1046's); the VM
//! answers them here, on the batch value, before the scalar functions'
//! conversion would turn a DECIMAL into a REAL or a BOOLEAN into 0/1.
//!
//! A cast DuckDB refuses is an error with DuckDB's message, never NULL.
//! `CAST(double AS VARCHAR)` is refused here too: DuckDB prints a double in
//! a format of its own (`100.0`, `1e+20`), not yet matched.

use std::borrow::Cow;

use super::Value;

use crate::functions::{CAST_BIGINT_FN, CAST_DOUBLE_FN, CAST_INTEGER_FN, CAST_VARCHAR_FN};

/// `name(args)` for one row when `name` is a `__cast_*` call: `None` for
/// any other call. The error is DuckDB's message.
pub(super) fn cast(name: &str, args: &[&[Value]], row: usize) -> Option<Result<Value, String>> {
    let to: fn(&Value) -> Result<Value, String> = match name {
        CAST_VARCHAR_FN => to_varchar,
        CAST_INTEGER_FN => |v| to_int(v, Width::Int32),
        CAST_BIGINT_FN => |v| to_int(v, Width::Int64),
        CAST_DOUBLE_FN => to_double,
        _ => return None,
    };
    let [col] = args else {
        return None;
    };
    Some(to(col.get(row)?))
}

/// The DuckDB name of `v`'s type, for an error message.
pub(super) fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "BIGINT",
        Value::Float(_) => "DOUBLE",
        Value::Bool(_) => "BOOLEAN",
        Value::Str(_) => "VARCHAR",
        Value::Null => "NULL",
        Value::Map(_) => "MAP",
        Value::List(_) => "LIST",
        Value::Blob(_) | Value::Bytes(_) => "BLOB",
        Value::Decimal { .. } => "DECIMAL",
    }
}

fn unimplemented(v: &Value, to: &str) -> String {
    format!(
        "Conversion Error: Unimplemented type for cast ({} -> {to})",
        type_name(v)
    )
}

/// `CAST(x AS VARCHAR)`: text as DuckDB prints the value. A BLOB's bytes
/// outside printable ASCII, and `\`, print as `\xHH`; it is not a UTF-8
/// decode (that is `decode`).
fn to_varchar(v: &Value) -> Result<Value, String> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Str(s) => Value::Str(s.clone()),
        Value::Int(_) | Value::Bool(_) | Value::Decimal { .. } => {
            Value::Str(Cow::Owned(v.to_string()))
        }
        Value::Blob(bytes) => Value::Str(Cow::Owned(escape_blob(bytes))),
        Value::Float(_) | Value::Map(_) | Value::List(_) | Value::Bytes(_) => {
            return Err(unimplemented(v, "VARCHAR"))
        }
    })
}

/// A BLOB as DuckDB's `CAST(blob AS VARCHAR)` prints it.
fn escape_blob(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x20..0x7f).contains(&b) && b != b'\\' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("\\x{b:02X}"));
        }
    }
    out
}

/// An integer cast's target: DuckDB's `INTEGER` is 32-bit.
#[derive(Clone, Copy)]
enum Width {
    Int32,
    Int64,
}

impl Width {
    /// DuckDB's internal name, as its messages print it.
    fn name(self) -> &'static str {
        match self {
            Width::Int32 => "INT32",
            Width::Int64 => "INT64",
        }
    }

    fn holds(self, n: i128) -> bool {
        match self {
            Width::Int32 => i32::try_from(n).is_ok(),
            Width::Int64 => i64::try_from(n).is_ok(),
        }
    }
}

/// `CAST(x AS INTEGER|BIGINT)`. Text is parsed as DuckDB does (trimmed,
/// a sign, `0x` hex, `_` between digits, a fraction or an exponent rounded
/// half away from zero); a double rounds half to even; a decimal half away
/// from zero; a boolean is 1 or 0. Out of range is DuckDB's error.
fn to_int(v: &Value, width: Width) -> Result<Value, String> {
    let n: i128 = match v {
        Value::Null => return Ok(Value::Null),
        Value::Int(n) => i128::from(*n),
        Value::Bool(b) => i128::from(*b),
        Value::Str(s) => {
            let not_a_number = || {
                format!(
                    "Conversion Error: Could not convert string '{s}' to {}",
                    width.name()
                )
            };
            let n = parse_int(s).ok_or_else(not_a_number)?;
            if !width.holds(n) {
                return Err(not_a_number());
            }
            n
        }
        Value::Float(f) => {
            let r = f.round_ties_even();
            if !r.is_finite() || r < -1e38 || r > 1e38 {
                return Err(out_of_range(v, width));
            }
            // In range of i128 by the check above; `as` saturates anyway.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "finite and within +-1e38, which i128 holds"
            )]
            let n = r as i128;
            n
        }
        Value::Decimal { unscaled, scale } => {
            let p = super::pow10(u32::from(*scale)).ok_or_else(|| out_of_range(v, width))?;
            let (q, r) = (
                unscaled
                    .checked_div(p)
                    .ok_or_else(|| out_of_range(v, width))?,
                unscaled
                    .checked_rem(p)
                    .ok_or_else(|| out_of_range(v, width))?,
            );
            let half_or_more = r
                .checked_abs()
                .and_then(|r| r.checked_mul(2))
                .is_none_or(|r2| r2 >= p);
            if half_or_more {
                q.checked_add(unscaled.signum())
                    .ok_or_else(|| out_of_range(v, width))?
            } else {
                q
            }
        }
        Value::Blob(_) | Value::Bytes(_) | Value::Map(_) | Value::List(_) => {
            let to = match width {
                Width::Int32 => "INTEGER",
                Width::Int64 => "BIGINT",
            };
            return Err(unimplemented(v, to));
        }
    };
    if !width.holds(n) {
        return Err(out_of_range(v, width));
    }
    i64::try_from(n)
        .map(Value::Int)
        .map_err(|_| out_of_range(v, width))
}

fn out_of_range(v: &Value, width: Width) -> String {
    format!(
        "Conversion Error: Type {} with value {v} can't be cast because the value is out of \
         range for the destination type {}",
        type_name(v),
        width.name()
    )
}

/// Text as DuckDB's integer cast reads it; `None` if it is no number.
fn parse_int(text: &str) -> Option<i128> {
    let t = text.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let magnitude = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).ok()?
    } else {
        parse_decimal_rounded(&strip_underscores(digits)?)?
    };
    if negative {
        magnitude.checked_neg()
    } else {
        Some(magnitude)
    }
}

/// `digits` without `_`, which DuckDB allows only between two digits.
fn strip_underscores(digits: &str) -> Option<String> {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'_' {
            let before = i.checked_sub(1).and_then(|j| bytes.get(j));
            let after = i.checked_add(1).and_then(|j| bytes.get(j));
            if !(before.is_some_and(u8::is_ascii_digit) && after.is_some_and(u8::is_ascii_digit)) {
                return None;
            }
        } else {
            out.push(char::from(b));
        }
    }
    Some(out)
}

/// An unsigned `int[.frac][e[+-]exp]` rounded half away from zero (the
/// caller applies the sign); `None` if it is no number or past `i128`.
fn parse_decimal_rounded(t: &str) -> Option<i128> {
    let (mantissa, exp) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (t, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // All the digits, and where the point falls after the exponent.
    let all = format!("{int}{frac}");
    let point = i64::try_from(int.len()).ok()?.checked_add(i64::from(exp))?;
    let point = usize::try_from(point.max(0)).ok()?;
    // Past 39 integer digits no `i128` holds it: refused before the zeros
    // are written out (`1e999999999` would allocate them).
    if point > 39 {
        return None;
    }
    let (whole, rest) = if point >= all.len() {
        let zeros = point.checked_sub(all.len())?;
        (format!("{all}{}", "0".repeat(zeros)), String::new())
    } else {
        (all.get(..point)?.to_string(), all.get(point..)?.to_string())
    };
    let whole: i128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let round_up = rest.bytes().next().is_some_and(|d| d >= b'5');
    if round_up {
        whole.checked_add(1)
    } else {
        Some(whole)
    }
}

/// `CAST(x AS DOUBLE)`: text trimmed and read as a double (`inf`, `nan`,
/// and an overflow to infinity, as DuckDB); a number converted.
fn to_double(v: &Value) -> Result<Value, String> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Float(f) => Value::Float(*f),
        #[allow(
            clippy::cast_precision_loss,
            reason = "DuckDB's BIGINT -> DOUBLE rounds the same way"
        )]
        Value::Int(n) => Value::Float(*n as f64),
        Value::Bool(b) => Value::Float(if *b { 1.0 } else { 0.0 }),
        Value::Decimal { unscaled, scale } => {
            Value::Float(super::decimal_to_f64(*unscaled, *scale))
        }
        Value::Str(s) => {
            let fail = || format!("Conversion Error: Could not convert string '{s}' to DOUBLE");
            // `_` between two digits, as for the integer cast (`1_000.5`).
            let text = strip_underscores(s.trim()).ok_or_else(fail)?;
            Value::Float(text.parse::<f64>().map_err(|_| fail())?)
        }
        Value::Blob(_) | Value::Bytes(_) | Value::Map(_) | Value::List(_) => {
            return Err(unimplemented(v, "DOUBLE"))
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn s(t: &str) -> Value {
        Value::Str(Cow::Owned(t.to_string()))
    }

    fn dec(unscaled: i128, scale: u8) -> Value {
        Value::Decimal { unscaled, scale }
    }

    fn blob(b: &[u8]) -> Value {
        Value::Blob(Arc::from(b))
    }

    fn int32(v: &Value) -> Result<Value, String> {
        to_int(v, Width::Int32)
    }

    fn int64(v: &Value) -> Result<Value, String> {
        to_int(v, Width::Int64)
    }

    #[test]
    fn only_a_cast_call_with_one_argument_is_answered() {
        let col = [s("7")];
        assert_eq!(cast(CAST_INTEGER_FN, &[&col], 0), Some(Ok(Value::Int(7))));
        assert_eq!(cast("upper", &[&col], 0), None);
        assert_eq!(cast(CAST_INTEGER_FN, &[&col, &col], 0), None);
        assert_eq!(cast(CAST_INTEGER_FN, &[&col], 5), None);
    }

    #[test]
    fn varchar_prints_values_as_duckdb() {
        assert_eq!(to_varchar(&Value::Int(7)), Ok(s("7")));
        assert_eq!(to_varchar(&dec(150, 2)), Ok(s("1.50")));
        assert_eq!(to_varchar(&Value::Bool(true)), Ok(s("true")));
        assert_eq!(to_varchar(&s("x")), Ok(s("x")));
        assert_eq!(to_varchar(&Value::Null), Ok(Value::Null));
        assert_eq!(to_varchar(&blob(b"10")), Ok(s("10")));
        assert!(to_varchar(&Value::Float(1.5))
            .unwrap_err()
            .contains("DOUBLE -> VARCHAR"));
        assert!(to_varchar(&Value::List(Arc::new(Vec::new())))
            .unwrap_err()
            .contains("LIST -> VARCHAR"));
    }

    #[test]
    fn a_blob_prints_non_printable_bytes_and_backslash_escaped() {
        assert_eq!(
            escape_blob(&[0x00, b'A', 0x0a, 0x7f, 0x80, 0xe9]),
            "\\x00A\\x0A\\x7F\\x80\\xE9"
        );
        assert_eq!(escape_blob(b"\\"), "\\x5C");
        assert_eq!(escape_blob("é".as_bytes()), "\\xC3\\xA9");
    }

    #[test]
    fn text_casts_to_an_integer_as_duckdb_reads_it() {
        assert_eq!(int32(&s(" 10 ")), Ok(Value::Int(10)));
        assert_eq!(int64(&s(" +7 ")), Ok(Value::Int(7)));
        assert_eq!(int32(&s("2.5")), Ok(Value::Int(3)));
        assert_eq!(int32(&s("-2.5")), Ok(Value::Int(-3)));
        assert_eq!(int32(&s("2.4")), Ok(Value::Int(2)));
        assert_eq!(int32(&s("1e2")), Ok(Value::Int(100)));
        assert_eq!(int32(&s("1.5e1")), Ok(Value::Int(15)));
        assert_eq!(int32(&s("25e-1")), Ok(Value::Int(3)));
        assert_eq!(int32(&s(".5")), Ok(Value::Int(1)));
        assert_eq!(int32(&s("0x10")), Ok(Value::Int(16)));
        assert_eq!(int32(&s("-0X10")), Ok(Value::Int(-16)));
        assert_eq!(int32(&s("1_000")), Ok(Value::Int(1000)));
        assert_eq!(int32(&s("1_0.5_0")), Ok(Value::Int(11)));
        assert_eq!(int32(&s("+1.5")), Ok(Value::Int(2)));
        assert_eq!(int64(&s("3000000000")), Ok(Value::Int(3_000_000_000)));
        for bad in [
            "",
            "abc",
            "1__0",
            "_1",
            "1_",
            ".",
            "1e",
            "0xg",
            "--1",
            "1e999999999",
            "1e40",
        ] {
            assert_eq!(
                int32(&s(bad)),
                Err(format!(
                    "Conversion Error: Could not convert string '{bad}' to INT32"
                )),
                "{bad:?}"
            );
        }
        assert_eq!(
            int32(&s("3000000000")),
            Err("Conversion Error: Could not convert string '3000000000' to INT32".to_string())
        );
    }

    #[test]
    fn numbers_cast_to_an_integer_with_duckdbs_rounding() {
        assert_eq!(int32(&Value::Float(2.5)), Ok(Value::Int(2)));
        assert_eq!(int32(&Value::Float(3.5)), Ok(Value::Int(4)));
        assert_eq!(int32(&dec(155, 2)), Ok(Value::Int(2)));
        assert_eq!(int32(&dec(-155, 2)), Ok(Value::Int(-2)));
        assert_eq!(int32(&dec(149, 2)), Ok(Value::Int(1)));
        assert_eq!(int32(&Value::Bool(true)), Ok(Value::Int(1)));
        assert_eq!(int32(&Value::Null), Ok(Value::Null));
        assert!(int32(&Value::Int(3_000_000_000))
            .unwrap_err()
            .contains("out of range for the destination type INT32"));
        assert!(int64(&Value::Float(f64::INFINITY))
            .unwrap_err()
            .contains("INT64"));
        assert!(int64(&Value::Float(1e20)).unwrap_err().contains("INT64"));
        assert_eq!(
            int32(&blob(b"1")),
            Err("Conversion Error: Unimplemented type for cast (BLOB -> INTEGER)".to_string())
        );
        assert!(int64(&blob(b"1")).unwrap_err().contains("BLOB -> BIGINT"));
    }

    #[test]
    fn text_and_numbers_cast_to_a_double() {
        assert_eq!(to_double(&s(" 1.5 ")), Ok(Value::Float(1.5)));
        assert_eq!(to_double(&s("1_000")), Ok(Value::Float(1000.0)));
        assert_eq!(to_double(&s("1_0.5_0")), Ok(Value::Float(10.5)));
        assert_eq!(to_double(&s("1.5e1_0")), Ok(Value::Float(1.5e10)));
        assert_eq!(to_double(&s("+1.5")), Ok(Value::Float(1.5)));
        assert_eq!(to_double(&s("-inf")), Ok(Value::Float(f64::NEG_INFINITY)));
        for bad in ["1__0", "_1", "1_", "0x10"] {
            assert!(to_double(&s(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(to_double(&s("inf")), Ok(Value::Float(f64::INFINITY)));
        assert_eq!(to_double(&s("1e400")), Ok(Value::Float(f64::INFINITY)));
        assert!(matches!(to_double(&s("nan")), Ok(Value::Float(f)) if f.is_nan()));
        assert_eq!(to_double(&Value::Int(7)), Ok(Value::Float(7.0)));
        assert_eq!(to_double(&dec(150, 2)), Ok(Value::Float(1.5)));
        assert_eq!(to_double(&Value::Bool(false)), Ok(Value::Float(0.0)));
        assert_eq!(to_double(&Value::Float(2.0)), Ok(Value::Float(2.0)));
        assert_eq!(to_double(&Value::Null), Ok(Value::Null));
        assert_eq!(
            to_double(&s("abc")),
            Err("Conversion Error: Could not convert string 'abc' to DOUBLE".to_string())
        );
        assert!(to_double(&blob(b"1"))
            .unwrap_err()
            .contains("BLOB -> DOUBLE"));
    }

    // vm_batch_cast_escape_blob_af56d636: `(0x20..0x7f).contains(&b) &&
    // b != b'\\'` -- printable ASCII other than a backslash prints as is.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_escape_blob_af56d636__v1_a_printable_byte_prints_as_is() {
        assert_eq!(escape_blob(b"A"), "A");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_escape_blob_af56d636__v2_a_control_byte_is_escaped() {
        assert_eq!(escape_blob(b"\n"), "\\x0A");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_escape_blob_af56d636__v3_a_backslash_is_escaped() {
        assert_eq!(escape_blob(b"\\"), "\\x5C");
    }

    // vm_batch_cast_to_int_7ba4f02a: `!r.is_finite() || r < -1e38 ||
    // r > 1e38` -- a double out of any integer's reach.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_to_int_7ba4f02a__v1_infinity_is_out_of_range() {
        assert!(int64(&Value::Float(f64::INFINITY)).is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_to_int_7ba4f02a__v2_far_below_is_out_of_range() {
        assert!(int64(&Value::Float(-1e39)).is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_to_int_7ba4f02a__v3_far_above_is_out_of_range() {
        assert!(int64(&Value::Float(1e39)).is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_to_int_7ba4f02a__v4_a_double_in_range_rounds() {
        assert_eq!(int64(&Value::Float(-2.5)), Ok(Value::Int(-2)));
    }

    // vm_batch_cast_parse_decimal_rounded_403b5d82: `(int.is_empty() &&
    // frac.is_empty()) || int has a non-digit || frac has a non-digit`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_parse_decimal_rounded_403b5d82__v1_digits_both_sides_read() {
        assert_eq!(parse_decimal_rounded("12.5"), Some(13));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_parse_decimal_rounded_403b5d82__v2_a_bare_point_is_no_number() {
        assert_eq!(parse_decimal_rounded("."), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_parse_decimal_rounded_403b5d82__v3_a_fraction_alone_reads() {
        assert_eq!(parse_decimal_rounded(".5"), Some(1));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_parse_decimal_rounded_403b5d82__v4_a_letter_before_the_point_is_no_number(
    ) {
        assert_eq!(parse_decimal_rounded("1a.5"), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_batch_cast_parse_decimal_rounded_403b5d82__v5_a_letter_after_the_point_is_no_number(
    ) {
        assert_eq!(parse_decimal_rounded("1.5a"), None);
    }
}
