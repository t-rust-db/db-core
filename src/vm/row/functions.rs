//! Scalar function set (db-core#64), ported from sqlite-rs's
//! `vdbe::functions` -- pure `fn(&[Value]) -> Result<Value,
//! FunctionError>` implementations plus a name+arity registry, backing
//! `Opcode::Function`.
//!
//! **First slice only**: `abs`/`length`/`upper`/`lower`/`coalesce`/
//! `ifnull`/`nullif`/`typeof`. Deliberately deferred: `like`/`glob`
//! (~150 lines of pattern matching), `substr`/`replace`/`trim` family,
//! `hex`/`unhex`/`quote`, scalar `min`/`max`, `round`, `sign`,
//! `instr`, `zeroblob`, `iif` -- later slices, per db-core#64's own
//! scope note.

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
