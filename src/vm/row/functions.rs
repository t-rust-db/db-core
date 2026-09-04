//! Scalar function set (db-core#64), ported from sqlite-rs's
//! `vdbe::functions` -- pure `fn(&[Value]) -> Result<Value,
//! FunctionError>` implementations plus a name+arity registry, backing
//! `Opcode::Function`.
//!
//! **First slice** (db-core#64): `abs`/`length`/`upper`/`lower`/
//! `coalesce`/`ifnull`/`nullif`/`typeof`.
//!
//! **Second slice** (db-core#68): `sign`/`zeroblob`/`iif`/scalar
//! `min`/`max`/`sqlite_version`/`round`/`hex`/`unhex`/`instr`/`quote`.
//!
//! Deliberately still deferred: `like`/`glob` (~150 lines of custom
//! recursive pattern matchers, a distinct sub-feature) and the
//! `substr`/`trim`/`ltrim`/`rtrim`/`replace` family (multi-arg arity,
//! byte/char-index edge cases) -- later slices.

use std::cmp::Ordering;

use super::compare::compare;
use super::value::{Collation, Value};

/// The ways a scalar function call can fail to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionError {
    /// No registered function matches `name` at the given `arity`.
    Unknown { name: String, arity: usize },
    /// An arithmetic result overflowed `i64`.
    IntegerOverflow,
}

impl std::fmt::Display for FunctionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FunctionError::Unknown { name, arity } => {
                write!(f, "unknown function {name} with {arity} argument(s)")
            }
            FunctionError::IntegerOverflow => write!(f, "integer overflow"),
        }
    }
}

impl std::error::Error for FunctionError {}

/// Renders `v` the way `CAST(v AS TEXT)` would, for `length()` on
/// non-blob arguments -- an integer/real's *text* representation, not
/// its storage bytes.
fn as_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => super::value::format_real(*r),
        Value::Text(s) => s.to_string(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

fn value_f64(v: &Value) -> f64 {
    match v {
        Value::Integer(i) => *i as f64,
        Value::Real(r) => *r,
        Value::Text(s) => match super::coerce::coerce_text_to_numeric(s) {
            Value::Integer(i) => i as f64,
            Value::Real(r) => r,
            _ => 0.0,
        },
        Value::Null | Value::Blob(_) => 0.0,
    }
}

fn length(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(match &args[0] {
        Value::Null => Value::Null,
        Value::Blob(b) => Value::Integer(b.len() as i64),
        Value::Text(s) => Value::Integer(s.chars().count() as i64),
        other => Value::Integer(as_text(other).chars().count() as i64),
    })
}

fn upper(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(match &args[0] {
        Value::Null => Value::Null,
        Value::Text(s) => Value::Text(s.to_ascii_uppercase().into()),
        other => other.clone(),
    })
}

fn lower(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(match &args[0] {
        Value::Null => Value::Null,
        Value::Text(s) => Value::Text(s.to_ascii_lowercase().into()),
        other => other.clone(),
    })
}

fn abs(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(match &args[0] {
        Value::Null => Value::Null,
        Value::Integer(i) => Value::Integer(i.checked_abs().ok_or(FunctionError::IntegerOverflow)?),
        Value::Real(r) => Value::Real(r.abs()),
        // Text/blob arguments always coerce through the REAL path --
        // even a clean integer-looking string like '5' yields REAL
        // 5.0, matching sqlite-rs's own oracle-verified behavior.
        other => Value::Real(value_f64(other).abs()),
    })
}

fn coalesce(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(args
        .iter()
        .find(|v| !matches!(v, Value::Null))
        .cloned()
        .unwrap_or(Value::Null))
}

fn nullif(args: &[Value]) -> Result<Value, FunctionError> {
    let (a, b) = (&args[0], &args[1]);
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(a.clone());
    }
    if compare(a, b, Collation::Binary) == Ordering::Equal {
        Ok(Value::Null)
    } else {
        Ok(a.clone())
    }
}

fn typeof_fn(args: &[Value]) -> Result<Value, FunctionError> {
    let s = match &args[0] {
        Value::Null => "null",
        Value::Integer(_) => "integer",
        Value::Real(_) => "real",
        Value::Text(_) => "text",
        Value::Blob(_) => "blob",
    };
    Ok(Value::Text(s.to_string().into()))
}

fn sqlite_version(_args: &[Value]) -> Result<Value, FunctionError> {
    Ok(Value::Text("3.53.4".to_string().into()))
}

fn hex(args: &[Value]) -> Result<Value, FunctionError> {
    let bytes: Vec<u8> = match &args[0] {
        Value::Blob(b) => b.to_vec(),
        other => as_text(other).into_bytes(),
    };
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        out.push_str(&format!("{b:02X}"));
    }
    Ok(Value::Text(out.into()))
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c.saturating_sub(b'0')),
        b'a'..=b'f' => Some(c.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(c.saturating_sub(b'A').saturating_add(10)),
        _ => None,
    }
}

fn unhex(args: &[Value]) -> Result<Value, FunctionError> {
    if matches!(args[0], Value::Null) {
        return Ok(Value::Null);
    }
    let text = as_text(&args[0]);
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Ok(Value::Null);
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let (Some(hi), Some(lo)) = (hex_digit(pair[0]), hex_digit(pair[1])) else {
            return Ok(Value::Null);
        };
        out.push((hi << 4) | lo);
    }
    Ok(Value::Blob(out.into()))
}

/// Renders a blob the way `sqlite3`'s `quote()` does: `X'` + uppercase
/// hex + `'`.
fn format_blob(b: &[u8]) -> String {
    let mut s = String::with_capacity(3usize.saturating_add(b.len().saturating_mul(2)));
    s.push_str("X'");
    for byte in b {
        s.push_str(&format!("{byte:02X}"));
    }
    s.push('\'');
    s
}

fn sql_quote_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len().saturating_add(2));
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

fn quote(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(Value::Text(match &args[0] {
        Value::Null => "NULL".to_string().into(),
        Value::Integer(i) => i.to_string().into(),
        Value::Real(r) => super::value::format_real(*r).into(),
        Value::Text(s) => sql_quote_text(s).into(),
        Value::Blob(b) => format_blob(b).into(),
    }))
}

fn scalar_min(args: &[Value]) -> Result<Value, FunctionError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    Ok(args
        .iter()
        .min_by(|a, b| compare(a, b, Collation::Binary))
        .cloned()
        .unwrap_or(Value::Null))
}

fn scalar_max(args: &[Value]) -> Result<Value, FunctionError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    Ok(args
        .iter()
        .max_by(|a, b| compare(a, b, Collation::Binary))
        .cloned()
        .unwrap_or(Value::Null))
}

/// Half-away-from-zero rounding to `digits` decimal places, always
/// returning REAL (matches SQLite's `round()`, which never returns
/// INTEGER even for a whole-number result).
fn round_fn(args: &[Value]) -> Result<Value, FunctionError> {
    if matches!(args[0], Value::Null) || matches!(args.get(1), Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let x = value_f64(&args[0]);
    let digits = args
        .get(1)
        .map_or(0, super::coerce::cast_to_integer)
        .clamp(0, 30);
    #[allow(clippy::cast_precision_loss)]
    let scale = 10f64.powi(digits as i32);
    let scaled = x * scale;
    let rounded = if scaled >= 0.0 {
        (scaled + 0.5).floor()
    } else {
        (scaled - 0.5).ceil()
    };
    Ok(Value::Real(rounded / scale))
}

fn sign(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(match &args[0] {
        Value::Null => Value::Null,
        other => {
            let n = value_f64(other);
            Value::Integer(if n > 0.0 {
                1
            } else if n < 0.0 {
                -1
            } else {
                0
            })
        }
    })
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn instr(args: &[Value]) -> Result<Value, FunctionError> {
    if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
        return Ok(Value::Null);
    }
    let pos = if let Value::Blob(hay) = &args[0] {
        match &args[1] {
            Value::Blob(b) => find_bytes(hay, b),
            other => find_bytes(hay, as_text(other).as_bytes()),
        }
    } else {
        let haystack = as_text(&args[0]);
        let needle = as_text(&args[1]);
        haystack
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(haystack.len()))
            .position(|i| haystack[i..].starts_with(&needle))
    };
    Ok(Value::Integer(
        pos.map_or(0, |p| (p as i64).saturating_add(1)),
    ))
}

/// SQLite's default `SQLITE_MAX_LENGTH` -- the largest blob/string this
/// build will materialize. Bounds `zeroblob()` so a huge requested size
/// returns a clamped result instead of an unbounded allocation.
const MAX_BLOB_LEN: i64 = 1_000_000_000;

#[allow(clippy::cast_sign_loss)]
fn zeroblob(args: &[Value]) -> Result<Value, FunctionError> {
    let n = super::coerce::cast_to_integer(&args[0]).clamp(0, MAX_BLOB_LEN);
    Ok(Value::Blob(vec![0u8; n as usize].into()))
}

fn iif(args: &[Value]) -> Result<Value, FunctionError> {
    let cond = match &args[0] {
        Value::Null => false,
        Value::Integer(i) => *i != 0,
        Value::Real(r) => *r != 0.0,
        Value::Text(s) => match super::coerce::coerce_text_to_numeric(s) {
            Value::Integer(i) => i != 0,
            Value::Real(r) => r != 0.0,
            _ => false,
        },
        Value::Blob(_) => false,
    };
    Ok(if cond {
        args[1].clone()
    } else {
        args[2].clone()
    })
}

type ScalarFn = fn(&[Value]) -> Result<Value, FunctionError>;

/// Dispatches `name(args)` by name and arity into this module's
/// registry, per `Opcode::Function`.
pub fn call(name: &str, args: &[Value]) -> Result<Value, FunctionError> {
    let arity = args.len();
    let f: Option<ScalarFn> = match (name.to_ascii_lowercase().as_str(), arity) {
        ("length", 1) => Some(length),
        ("upper", 1) => Some(upper),
        ("lower", 1) => Some(lower),
        ("abs", 1) => Some(abs),
        ("coalesce", n) if n >= 2 => Some(coalesce),
        ("ifnull", 2) => Some(coalesce),
        ("nullif", 2) => Some(nullif),
        ("typeof", 1) => Some(typeof_fn),
        ("sqlite_version", 0) => Some(sqlite_version),
        ("hex", 1) => Some(hex),
        ("unhex", 1) => Some(unhex),
        ("quote", 1) => Some(quote),
        ("min", n) if n >= 1 => Some(scalar_min),
        ("max", n) if n >= 1 => Some(scalar_max),
        ("round", 1 | 2) => Some(round_fn),
        ("sign", 1) => Some(sign),
        ("instr", 2) => Some(instr),
        ("zeroblob", 1) => Some(zeroblob),
        ("iif", 3) => Some(iif),
        _ => None,
    };
    match f {
        Some(f) => f(args),
        None => Err(FunctionError::Unknown {
            name: name.to_string(),
            arity,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(name: &str, args: &[Value]) -> Value {
        call(name, args).unwrap()
    }

    #[test]
    fn length_counts_chars_for_text_bytes_for_blob() {
        assert_eq!(
            v("length", &[Value::Text("héllo".to_string().into())]),
            Value::Integer(5)
        );
        assert_eq!(
            v("length", &[Value::Blob(vec![1, 2, 3].into())]),
            Value::Integer(3)
        );
        assert_eq!(v("length", &[Value::Null]), Value::Null);
        assert_eq!(v("length", &[Value::Integer(12345)]), Value::Integer(5));
    }

    #[test]
    fn upper_lower_are_ascii_only() {
        assert_eq!(
            v("upper", &[Value::Text("café".to_string().into())]),
            Value::Text("CAFé".to_string().into())
        );
        // ASCII-only: the non-ASCII É is untouched by lower().
        assert_eq!(
            v("lower", &[Value::Text("CAFÉ".to_string().into())]),
            Value::Text("cafÉ".to_string().into())
        );
        assert_eq!(v("upper", &[Value::Integer(5)]), Value::Integer(5));
        assert_eq!(v("upper", &[Value::Null]), Value::Null);
    }

    #[test]
    fn abs_handles_every_value_kind() {
        assert_eq!(v("abs", &[Value::Integer(-5)]), Value::Integer(5));
        assert_eq!(v("abs", &[Value::Real(-1.5)]), Value::Real(1.5));
        assert_eq!(
            v("abs", &[Value::Text("-5".to_string().into())]),
            Value::Real(5.0)
        );
        assert_eq!(v("abs", &[Value::Null]), Value::Null);
    }

    #[test]
    fn abs_overflow_errors_instead_of_wrapping() {
        assert_eq!(
            call("abs", &[Value::Integer(i64::MIN)]),
            Err(FunctionError::IntegerOverflow)
        );
    }

    #[test]
    fn coalesce_and_ifnull_are_the_null_propagation_exception() {
        assert_eq!(
            v("coalesce", &[Value::Null, Value::Null, Value::Integer(7)]),
            Value::Integer(7)
        );
        assert_eq!(v("coalesce", &[Value::Null, Value::Null]), Value::Null);
        assert_eq!(
            v("ifnull", &[Value::Null, Value::Integer(7)]),
            Value::Integer(7)
        );
        assert_eq!(
            v("ifnull", &[Value::Integer(1), Value::Integer(7)]),
            Value::Integer(1)
        );
    }

    #[test]
    fn nullif_returns_null_on_equal_else_first_arg() {
        assert_eq!(
            v("nullif", &[Value::Integer(1), Value::Integer(1)]),
            Value::Null
        );
        assert_eq!(
            v("nullif", &[Value::Integer(1), Value::Integer(2)]),
            Value::Integer(1)
        );
        assert_eq!(v("nullif", &[Value::Null, Value::Integer(2)]), Value::Null);
    }

    #[test]
    fn typeof_names_every_storage_class() {
        assert_eq!(
            v("typeof", &[Value::Null]),
            Value::Text("null".to_string().into())
        );
        assert_eq!(
            v("typeof", &[Value::Integer(1)]),
            Value::Text("integer".to_string().into())
        );
        assert_eq!(
            v("typeof", &[Value::Real(1.0)]),
            Value::Text("real".to_string().into())
        );
        assert_eq!(
            v("typeof", &[Value::Text(String::new().into())]),
            Value::Text("text".to_string().into())
        );
        assert_eq!(
            v("typeof", &[Value::Blob(Vec::new().into())]),
            Value::Text("blob".to_string().into())
        );
    }

    #[test]
    fn round_half_away_from_zero() {
        assert_eq!(v("round", &[Value::Real(2.5)]), Value::Real(3.0));
        assert_eq!(v("round", &[Value::Real(-2.5)]), Value::Real(-3.0));
    }

    #[test]
    fn round_clamps_digits_and_propagates_null_digits() {
        assert_eq!(v("round", &[Value::Real(1.5), Value::Null]), Value::Null);
        let Value::Real(r) = v("round", &[Value::Real(1.5), Value::Integer(40)]) else {
            panic!("expected real");
        };
        assert!((r - 1.5).abs() < 1e-9, "digits clamped to 30, got {r}");
    }

    #[test]
    fn min_max_scalar_null_propagates() {
        assert_eq!(
            v(
                "min",
                &[Value::Integer(3), Value::Integer(1), Value::Integer(2)]
            ),
            Value::Integer(1)
        );
        assert_eq!(v("min", &[Value::Integer(1), Value::Null]), Value::Null);
        assert_eq!(v("max", &[Value::Integer(1), Value::Null]), Value::Null);
    }

    #[test]
    fn quote_escapes_single_quotes_and_renders_blob_hex() {
        assert_eq!(
            v("quote", &[Value::Text("it's".to_string().into())]),
            Value::Text("'it''s'".to_string().into())
        );
        assert_eq!(
            v("quote", &[Value::Blob(vec![0x00, 0x11].into())]),
            Value::Text("X'0011'".to_string().into())
        );
        assert_eq!(
            v("quote", &[Value::Null]),
            Value::Text("NULL".to_string().into())
        );
    }

    #[test]
    fn hex_and_unhex_roundtrip() {
        assert_eq!(
            v("hex", &[Value::Text("AB".to_string().into())]),
            Value::Text("4142".to_string().into())
        );
        assert_eq!(
            v("hex", &[Value::Integer(5)]),
            Value::Text("35".to_string().into())
        );
        assert_eq!(
            v("unhex", &[Value::Text("4142".to_string().into())]),
            Value::Blob(vec![0x41, 0x42].into())
        );
        assert_eq!(
            v("unhex", &[Value::Text("xyz".to_string().into())]),
            Value::Null
        );
    }

    #[test]
    fn iif_and_typeof() {
        assert_eq!(
            v(
                "iif",
                &[
                    Value::Integer(1),
                    Value::Text("a".to_string().into()),
                    Value::Text("b".to_string().into())
                ]
            ),
            Value::Text("a".to_string().into())
        );
        assert_eq!(
            v("typeof", &[Value::Null]),
            Value::Text("null".to_string().into())
        );
    }

    #[test]
    fn iif_treats_real_zero_coerced_text_as_falsy() {
        assert_eq!(
            v(
                "iif",
                &[
                    Value::Text("0.0".to_string().into()),
                    Value::Text("a".to_string().into()),
                    Value::Text("b".to_string().into())
                ]
            ),
            Value::Text("b".to_string().into())
        );
    }

    #[test]
    fn zeroblob_clamps_oversized_length() {
        let Value::Blob(b) = v("zeroblob", &[Value::Integer(i64::MAX)]) else {
            panic!("expected blob");
        };
        assert_eq!(b.len() as i64, MAX_BLOB_LEN);
        assert_eq!(
            v("zeroblob", &[Value::Integer(-1)]),
            Value::Blob(vec![].into())
        );
    }

    #[test]
    fn sign_reports_negative_zero_positive_and_propagates_null() {
        assert_eq!(v("sign", &[Value::Integer(-5)]), Value::Integer(-1));
        assert_eq!(v("sign", &[Value::Integer(0)]), Value::Integer(0));
        assert_eq!(v("sign", &[Value::Real(2.5)]), Value::Integer(1));
        assert_eq!(v("sign", &[Value::Null]), Value::Null);
    }

    #[test]
    fn instr_finds_substring_position_or_zero() {
        assert_eq!(
            v(
                "instr",
                &[
                    Value::Text("hello world".to_string().into()),
                    Value::Text("world".to_string().into())
                ]
            ),
            Value::Integer(7)
        );
        assert_eq!(
            v(
                "instr",
                &[
                    Value::Text("hello".to_string().into()),
                    Value::Text("xyz".to_string().into())
                ]
            ),
            Value::Integer(0)
        );
        assert_eq!(
            v(
                "instr",
                &[
                    Value::Blob(vec![1, 2, 3, 4].into()),
                    Value::Blob(vec![3, 4].into())
                ]
            ),
            Value::Integer(3)
        );
    }

    #[test]
    fn sqlite_version_returns_pinned_string() {
        assert_eq!(
            v("sqlite_version", &[]),
            Value::Text("3.53.4".to_string().into())
        );
    }

    #[test]
    fn unknown_function_or_arity_errors() {
        assert_eq!(
            call("median", &[Value::Integer(1)]),
            Err(FunctionError::Unknown {
                name: "median".to_string(),
                arity: 1
            })
        );
        assert_eq!(
            call("length", &[]),
            Err(FunctionError::Unknown {
                name: "length".to_string(),
                arity: 0
            })
        );
    }
}
