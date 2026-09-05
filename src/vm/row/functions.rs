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
//! **Third slice** (db-core#90): `substr`/`trim`/`ltrim`/`rtrim`/
//! `replace`, and `like`/`glob` with their recursive pattern matchers
//! (`like_match`/`glob_match`, exposed for reuse by the `LIKE`/`GLOB`
//! operators, not just the scalar functions). This closes the gap
//! against sqlite-rs's `vdbe::functions` entirely -- there is no
//! `printf`/date-time family in sqlite-rs's own registry to still
//! port. sqlite-rs's `vdbe::result`/`vdbe::arithmetic` (the other two
//! files db-core#90 named) needed no porting at all: every opcode
//! either backs already has a `super::vm` dispatch arm (`Integer`/
//! `Int64`/`Real`/`Blob`/`Null`/`Variable`/`String8`/`Copy`/
//! `MakeRecord`/`ResultRow`, and `Add`/`Subtract`/`Multiply`/`Divide`/
//! `Remainder`/`BitAnd`/`BitOr`/`ShiftLeft`/`ShiftRight`/`Concat`/
//! `Not`/`BitNot`).

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

/// `substr(x, y[, z])`: `y` (1-based, negative counts from the end)
/// selects the starting character/byte, `z` (defaulting to "the
/// rest") the count; a negative `z` extends backward from `y` instead
/// of forward. Operates on bytes for a BLOB argument, characters for
/// everything else (`CAST(x AS TEXT)` first).
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn substr(args: &[Value]) -> Result<Value, FunctionError> {
    if matches!(args[1], Value::Null) || args.get(2).is_some_and(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    if matches!(args[0], Value::Null) {
        return Ok(Value::Null);
    }
    let mut p1 = super::coerce::cast_to_integer(&args[1]);
    let (mut p2, neg_p2) = match args.get(2) {
        Some(z) => {
            let raw = super::coerce::cast_to_integer(z);
            if raw < 0 {
                (raw.saturating_neg(), true)
            } else {
                (raw, false)
            }
        }
        None => (i64::MAX / 2, false),
    };

    let blob = match &args[0] {
        Value::Blob(b) => Some(b),
        _ => None,
    };
    let len: i64 = if let Some(b) = blob {
        b.len() as i64
    } else if p1 < 0 {
        as_text(&args[0]).chars().count() as i64
    } else {
        0
    };

    if p1 < 0 {
        p1 = p1.saturating_add(len);
        if p1 < 0 {
            p2 = p2.saturating_add(p1);
            if p2 < 0 {
                p2 = 0;
            }
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 = p1.saturating_sub(1);
    } else if p2 > 0 {
        p2 = p2.saturating_sub(1);
    }

    if neg_p2 {
        p1 = p1.saturating_sub(p2);
        if p1 < 0 {
            p2 = p2.saturating_add(p1);
            p1 = 0;
        }
    }
    let p1 = p1.max(0) as usize;
    let p2 = p2.max(0) as usize;

    if let Some(b) = blob {
        let start = p1.min(b.len());
        let end = start.saturating_add(p2).min(b.len());
        Ok(Value::Blob(b[start..end].to_vec().into()))
    } else {
        let text = as_text(&args[0]);
        let out: String = text.chars().skip(p1).take(p2).collect();
        Ok(Value::Text(out.into()))
    }
}

/// `trim`/`ltrim`/`rtrim`'s second argument: the charset to strip,
/// defaulting to a single space.
fn trim_charset(args: &[Value]) -> String {
    args.get(1).map_or(" ".to_string(), as_text)
}

fn trim_fn(args: &[Value]) -> Result<Value, FunctionError> {
    if matches!(args[0], Value::Null) {
        return Ok(Value::Null);
    }
    let charset: Vec<char> = trim_charset(args).chars().collect();
    let s = as_text(&args[0]);
    Ok(Value::Text(
        s.trim_matches(|c| charset.contains(&c)).to_string().into(),
    ))
}

fn ltrim_fn(args: &[Value]) -> Result<Value, FunctionError> {
    if matches!(args[0], Value::Null) {
        return Ok(Value::Null);
    }
    let charset: Vec<char> = trim_charset(args).chars().collect();
    let s = as_text(&args[0]);
    Ok(Value::Text(
        s.trim_start_matches(|c| charset.contains(&c))
            .to_string()
            .into(),
    ))
}

fn rtrim_fn(args: &[Value]) -> Result<Value, FunctionError> {
    if matches!(args[0], Value::Null) {
        return Ok(Value::Null);
    }
    let charset: Vec<char> = trim_charset(args).chars().collect();
    let s = as_text(&args[0]);
    Ok(Value::Text(
        s.trim_end_matches(|c| charset.contains(&c))
            .to_string()
            .into(),
    ))
}

fn replace_fn(args: &[Value]) -> Result<Value, FunctionError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let s = as_text(&args[0]);
    let from = as_text(&args[1]);
    let to = as_text(&args[2]);
    if from.is_empty() {
        return Ok(Value::Text(s.into()));
    }
    Ok(Value::Text(s.replace(&from, &to).into()))
}

/// SQLite `LIKE`: `%` matches any run of characters, `_` matches
/// exactly one, everything else (case-insensitively, ASCII-only)
/// matches itself -- or, if `escape` is set, `escape` followed by `%`/
/// `_`/`escape` matches that character literally. Exposed (not just
/// the `like()` scalar function) so codegen's `LIKE` operator can call
/// it directly without going through the function-call machinery.
pub fn like_match(text: &str, pattern: &str, escape: Option<char>) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    like_rec(&t, &p, escape, 0, 0)
}

fn like_rec(t: &[char], p: &[char], escape: Option<char>, mut ti: usize, mut pi: usize) -> bool {
    loop {
        if pi == p.len() {
            return ti == t.len();
        }
        let pc = p[pi];
        if Some(pc) == escape && pi.saturating_add(1) < p.len() {
            let literal = p[pi.saturating_add(1)];
            if ti >= t.len() || !ascii_eq(t[ti], literal) {
                return false;
            }
            ti = ti.saturating_add(1);
            pi = pi.saturating_add(2);
            continue;
        }
        match pc {
            '%' => {
                // Collapse consecutive '%' (a run behaves as one).
                while pi < p.len() && p[pi] == '%' {
                    pi = pi.saturating_add(1);
                }
                if pi == p.len() {
                    return true;
                }
                for start in ti..=t.len() {
                    if like_rec(t, p, escape, start, pi) {
                        return true;
                    }
                }
                return false;
            }
            '_' => {
                if ti >= t.len() {
                    return false;
                }
                ti = ti.saturating_add(1);
                pi = pi.saturating_add(1);
            }
            _ => {
                if ti >= t.len() || !ascii_eq(t[ti], pc) {
                    return false;
                }
                ti = ti.saturating_add(1);
                pi = pi.saturating_add(1);
            }
        }
    }
}

fn ascii_eq(a: char, b: char) -> bool {
    a.eq_ignore_ascii_case(&b)
}

/// SQLite `GLOB`: case-sensitive, `*` = any run, `?` = any one char,
/// `[...]`/`[^...]` character classes (with `-` ranges). Exposed for
/// the same reason as [`like_match`].
pub fn glob_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    glob_rec(&t, &p, 0, 0)
}

fn glob_rec(t: &[char], p: &[char], mut ti: usize, mut pi: usize) -> bool {
    loop {
        if pi == p.len() {
            return ti == t.len();
        }
        match p[pi] {
            '*' => {
                while pi < p.len() && p[pi] == '*' {
                    pi = pi.saturating_add(1);
                }
                if pi == p.len() {
                    return true;
                }
                for start in ti..=t.len() {
                    if glob_rec(t, p, start, pi) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ti >= t.len() {
                    return false;
                }
                ti = ti.saturating_add(1);
                pi = pi.saturating_add(1);
            }
            '[' => {
                let Some((matches, next_pi)) = glob_class(p, pi, t.get(ti).copied()) else {
                    return false;
                };
                if ti >= t.len() || !matches {
                    return false;
                }
                ti = ti.saturating_add(1);
                pi = next_pi;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                ti = ti.saturating_add(1);
                pi = pi.saturating_add(1);
            }
        }
    }
}

/// Parses a `[...]`/`[^...]` class starting at `p[start]` (`p[start]
/// == '['`); returns whether `c` matched and the index just past the
/// `]`.
fn glob_class(p: &[char], start: usize, c: Option<char>) -> Option<(bool, usize)> {
    let mut i = start.saturating_add(1);
    let negate = p.get(i) == Some(&'^');
    if negate {
        i = i.saturating_add(1);
    }
    let class_start = i;
    let mut matched = false;
    loop {
        if i >= p.len() {
            return None; // unterminated class: treat as no match
        }
        if p[i] == ']' && i > class_start {
            i = i.saturating_add(1);
            break;
        }
        if i.saturating_add(2) < p.len()
            && p[i.saturating_add(1)] == '-'
            && p[i.saturating_add(2)] != ']'
        {
            let (lo, hi) = (p[i], p[i.saturating_add(2)]);
            if let Some(c) = c {
                if c >= lo && c <= hi {
                    matched = true;
                }
            }
            i = i.saturating_add(3);
        } else {
            if Some(p[i]) == c {
                matched = true;
            }
            i = i.saturating_add(1);
        }
    }
    Some((matched != negate && c.is_some(), i))
}

/// `like(pattern, text[, escape])` -- note SQLite's argument order is
/// (pattern, text), the reverse of the `text LIKE pattern` syntax.
fn like_fn(args: &[Value]) -> Result<Value, FunctionError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let escape = match args.get(2) {
        Some(e) => as_text(e).chars().next(),
        None => None,
    };
    let pattern = as_text(&args[0]);
    let text = as_text(&args[1]);
    Ok(Value::Integer(i64::from(like_match(
        &text, &pattern, escape,
    ))))
}

/// `glob(pattern, text)` -- same reversed argument order as `like()`.
fn glob_fn(args: &[Value]) -> Result<Value, FunctionError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let pattern = as_text(&args[0]);
    let text = as_text(&args[1]);
    Ok(Value::Integer(i64::from(glob_match(&text, &pattern))))
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
        ("substr", 2 | 3) => Some(substr),
        ("trim", 1 | 2) => Some(trim_fn),
        ("ltrim", 1 | 2) => Some(ltrim_fn),
        ("rtrim", 1 | 2) => Some(rtrim_fn),
        ("replace", 3) => Some(replace_fn),
        ("like", 2 | 3) => Some(like_fn),
        ("glob", 2) => Some(glob_fn),
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
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

    #[test]
    fn substr_negative_and_zero_index_rules() {
        assert_eq!(
            v(
                "substr",
                &[Value::Text("hello".to_string().into()), Value::Integer(-3)]
            ),
            Value::Text("llo".to_string().into())
        );
        assert_eq!(
            v(
                "substr",
                &[Value::Text("hello".to_string().into()), Value::Integer(0)]
            ),
            Value::Text("hello".to_string().into())
        );
        assert_eq!(
            v(
                "substr",
                &[
                    Value::Text("hello".to_string().into()),
                    Value::Integer(2),
                    Value::Integer(-1)
                ]
            ),
            Value::Text("h".to_string().into())
        );
        assert_eq!(
            v(
                "substr",
                &[
                    Value::Text("hello".to_string().into()),
                    Value::Integer(-100),
                    Value::Integer(2)
                ]
            ),
            Value::Text(String::new().into())
        );
        assert_eq!(v("substr", &[Value::Null, Value::Integer(1)]), Value::Null);
    }

    #[test]
    fn substr_operates_on_bytes_for_a_blob() {
        assert_eq!(
            v(
                "substr",
                &[
                    Value::Blob(vec![1, 2, 3, 4, 5].into()),
                    Value::Integer(2),
                    Value::Integer(2)
                ]
            ),
            Value::Blob(vec![2, 3].into())
        );
    }

    #[test]
    fn trim_ltrim_rtrim_default_to_whitespace_or_use_given_charset() {
        assert_eq!(
            v("trim", &[Value::Text("  hi  ".to_string().into())]),
            Value::Text("hi".to_string().into())
        );
        assert_eq!(
            v("ltrim", &[Value::Text("  hi  ".to_string().into())]),
            Value::Text("hi  ".to_string().into())
        );
        assert_eq!(
            v("rtrim", &[Value::Text("  hi  ".to_string().into())]),
            Value::Text("  hi".to_string().into())
        );
        assert_eq!(
            v(
                "trim",
                &[
                    Value::Text("xxhixx".to_string().into()),
                    Value::Text("x".to_string().into())
                ]
            ),
            Value::Text("hi".to_string().into())
        );
        assert_eq!(v("trim", &[Value::Null]), Value::Null);
    }

    #[test]
    fn replace_substitutes_all_occurrences_and_handles_empty_from() {
        assert_eq!(
            v(
                "replace",
                &[
                    Value::Text("banana".to_string().into()),
                    Value::Text("a".to_string().into()),
                    Value::Text("o".to_string().into())
                ]
            ),
            Value::Text("bonono".to_string().into())
        );
        assert_eq!(
            v(
                "replace",
                &[
                    Value::Text("hi".to_string().into()),
                    Value::Text(String::new().into()),
                    Value::Text("x".to_string().into())
                ]
            ),
            Value::Text("hi".to_string().into())
        );
        assert_eq!(
            v(
                "replace",
                &[
                    Value::Null,
                    Value::Text("a".to_string().into()),
                    Value::Null
                ]
            ),
            Value::Null
        );
    }

    #[test]
    fn like_and_glob_match_oracle_semantics() {
        let t = |s: &str| Value::Text(s.to_string().into());
        // LIKE is ASCII case-insensitive; GLOB is case-sensitive.
        assert_eq!(v("like", &[t("abc"), t("ABC")]), Value::Integer(1));
        assert_eq!(v("glob", &[t("abc"), t("ABC")]), Value::Integer(0));
        assert_eq!(v("like", &[t("a%b"), t("axxb")]), Value::Integer(1));
        // ESCAPE makes the following wildcard literal.
        assert_eq!(
            v("like", &[t("a\\%b"), t("a%b"), t("\\")]),
            Value::Integer(1)
        );
        // GLOB character classes, including negation.
        assert_eq!(v("glob", &[t("a[^b]c"), t("abc")]), Value::Integer(0));
        assert_eq!(v("glob", &[t("a[^b]c"), t("axc")]), Value::Integer(1));
        assert_eq!(v("glob", &[t("a?c"), t("abc")]), Value::Integer(1));
        assert_eq!(v("like", &[t("x"), Value::Null]), Value::Null);
    }

    #[test]
    fn like_match_and_glob_match_are_directly_callable() {
        assert!(like_match("abc", "a%c", None));
        assert!(like_match("abc", "a_c", None));
        assert!(!like_match("abcd", "a_c", None));
        assert!(glob_match("abc", "a*c"));
        assert!(!glob_match("ABC", "abc"));
    }

    #[test]
    fn glob_range_class_matches_inclusive_bounds() {
        assert!(glob_match("m", "[a-z]"));
        assert!(!glob_match("M", "[a-z]"));
        assert!(glob_match("5", "[0-9]"));
    }
}
