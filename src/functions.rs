//! Scalar function set (db-core#64), ported from sqlite-rs's
//! `vdbe::functions` -- pure `fn(&[Value]) -> Result<Value,
//! FunctionError>` implementations plus a name+arity registry, backing
//! `Opcode::Function`. Crate-root, feature-free, no dependencies beyond
//! [`crate::value`]/[`crate::compare`]/[`crate::coerce`] (ADR 0011/#122,
//! mirroring [`crate::value`]'s own ADR 0010 hoist): `vm::row`
//! re-exports it today, and `vm::batch`/`vm::stream` can call
//! [`call`] directly once either needs scalar functions of its own.
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

// Every `args[n]` index below is provably in-bounds, as in sqlite-rs's
// `vdbe::functions`: `call()`'s registry match arms gate on exact arity
// before dispatching, so each function body only indexes positions its
// own arm guarantees are present.
#![allow(
    clippy::indexing_slicing,
    reason = "`call()` matches on exact arity before dispatching, so each body only indexes argument positions its arm guarantees"
)]

use std::cmp::Ordering;

use crate::compare::compare;
use crate::value::{len_to_i64, Collation, Value};

/// The ways a scalar function call can fail to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionError {
    /// No registered function matches `name` at the given `arity`.
    Unknown {
        /// The unrecognized function name.
        name: String,
        /// The argument count it was called with.
        arity: usize,
    },
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
        Value::Real(r) => crate::value::format_real(*r),
        Value::Text(s) => s.to_string(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

fn value_f64(v: &Value) -> f64 {
    match v {
        Value::Integer(i) => *i as f64,
        Value::Real(r) => *r,
        Value::Text(s) => match crate::coerce::coerce_text_to_numeric(s) {
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
        Value::Blob(b) => Value::Integer(len_to_i64(b.len())),
        Value::Text(s) => Value::Integer(len_to_i64(s.chars().count())),
        other => Value::Integer(len_to_i64(as_text(other).chars().count())),
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
        Value::Real(r) => crate::value::format_real(*r).into(),
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
        .map_or(0, crate::coerce::cast_to_integer)
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
            .position(|i| haystack.get(i..).is_some_and(|h| h.starts_with(&needle)))
    };
    Ok(Value::Integer(
        pos.map_or(0, |p| len_to_i64(p).saturating_add(1)),
    ))
}

/// SQLite's default `SQLITE_MAX_LENGTH` -- the largest blob/string this
/// build will materialize. Bounds `zeroblob()` so a huge requested size
/// returns a clamped result instead of an unbounded allocation.
const MAX_BLOB_LEN: i64 = 1_000_000_000;

#[allow(clippy::cast_sign_loss)]
fn zeroblob(args: &[Value]) -> Result<Value, FunctionError> {
    let n = crate::coerce::cast_to_integer(&args[0]).clamp(0, MAX_BLOB_LEN);
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
    let mut p1 = crate::coerce::cast_to_integer(&args[1]);
    let (mut p2, neg_p2) = match args.get(2) {
        Some(z) => {
            let raw = crate::coerce::cast_to_integer(z);
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
        len_to_i64(b.len())
    } else if p1 < 0 {
        len_to_i64(as_text(&args[0]).chars().count())
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
        Value::Text(s) => match crate::coerce::coerce_text_to_numeric(s) {
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

/// Every overlapping 3-byte window of `text`'s UTF-8 bytes, in order.
/// Pure byte-level windowing, not grapheme- or even char-boundary-aware
/// -- deliberately: this matches how SQLite's own FTS trigram tokenizer
/// and tools like ripgrep/tgrep define a trigram, so an index built
/// from this stays comparable with theirs. `text` shorter than 3 bytes
/// yields no trigrams. Not a SQL scalar function (no caller needs one
/// yet) -- a shared primitive for a future trigram-accelerated `LIKE`/
/// `GLOB` index here, and for sqlite-rs's `sqlgrep` (t-rust-db/sqlite-rs#34).
pub fn trigrams(text: &str) -> impl Iterator<Item = [u8; 3]> + '_ {
    text.as_bytes().windows(3).filter_map(|w| w.try_into().ok())
}

/// Packs a trigram into an `i64` b-tree rowid key (big-endian byte
/// order, zero-extended) -- the natural key for a trigram->posting-list
/// table, whether that table lives in this crate's own future index or
/// in an external cache file (`sqlgrep`'s `trigrams` table,
/// t-rust-db/sqlite-rs#34).
pub fn trigram_key(t: [u8; 3]) -> i64 {
    i64::from(t[0]) << 16 | i64::from(t[1]) << 8 | i64::from(t[2])
}

/// `json_extract(text, path)` -- late parse of embedded JSON (#307,
/// ADR 0018 "late `json_extract`" for structure the ingest-time parser
/// left as a string). Malformed JSON, a missing path or a path through
/// a non-object all return `Value::Null` rather than an error, matching
/// this module's existing null-propagation convention (`substr`,
/// `instr`); a nested object/array leaf also returns `Value::Null` --
/// re-serializing a parsed [`crate::json_path::JsonValue::Object`] back
/// to JSON text is unscoped work with no caller today ([`JsonValue::Array`]
/// keeps its raw text, so extracting an array leaf does work).
fn json_extract(args: &[Value]) -> Result<Value, FunctionError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let text = as_text(&args[0]);
    let path = as_text(&args[1]);
    let Some((root, _)) = crate::json_path::parse_value(&text) else {
        return Ok(Value::Null);
    };
    Ok(match crate::json_path::lookup(&root, &path) {
        Some(crate::json_path::JsonValue::Null | crate::json_path::JsonValue::Object(_)) | None => {
            Value::Null
        }
        Some(crate::json_path::JsonValue::Bool(b)) => Value::Integer(i64::from(b)),
        Some(crate::json_path::JsonValue::Int(i)) => Value::Integer(i),
        Some(crate::json_path::JsonValue::Float(f)) => Value::Real(f),
        Some(crate::json_path::JsonValue::Str(s)) => Value::Text(s.to_string().into()),
        Some(crate::json_path::JsonValue::Array(raw)) => Value::Text(raw.to_string().into()),
    })
}

/// `logfmt_extract(text, key)` -- late parse of a `key=value` line
/// (#307). A bare key (no `=value`) does not count as a match -- there
/// is no "present but valueless" `Value` to return; missing/malformed
/// input returns `Value::Null`.
fn logfmt_extract(args: &[Value]) -> Result<Value, FunctionError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let text = as_text(&args[0]);
    let key = as_text(&args[1]);
    let found = crate::logfmt_scan::scan(&text).find_map(|(k, v)| if k == key { v } else { None });
    Ok(match found {
        Some(v) => Value::Text(v.to_string().into()),
        None => Value::Null,
    })
}

/// `regexp_extract(text, pattern, group)` -- capture-group extraction
/// from the first match of `pattern` in `text` (#307); `group` 0 is the
/// whole match. No match, an out-of-range group or a malformed pattern
/// all return `Value::Null`.
fn regexp_extract(args: &[Value]) -> Result<Value, FunctionError> {
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let text = as_text(&args[0]);
    let pattern = as_text(&args[1]);
    let group = crate::coerce::cast_to_integer(&args[2]);
    let Ok(group) = usize::try_from(group) else {
        return Ok(Value::Null);
    };
    let Some(re) = regex_lite::compile(&pattern) else {
        return Ok(Value::Null);
    };
    Ok(match regex_lite::find(&re, &text) {
        Some(m) => match m.group(group) {
            Some(s) => Value::Text(s.to_string().into()),
            None => Value::Null,
        },
        None => Value::Null,
    })
}

/// A small hand-rolled regex subset for `regexp_extract` (#307): literal
/// characters, `.` (any char), `*`/`+`/`?` quantifiers on the preceding
/// atom, `[...]`/`[^...]` character classes (with `-` ranges, mirroring
/// [`glob_class`]'s syntax), `\` to escape a metacharacter, and `(...)`
/// capture groups numbered by position of the opening paren (group 0 is
/// the whole match). Deliberately no anchors, alternation, non-capturing
/// groups, backreferences or `\d`/`\w`/`\s` shorthands, and no
/// quantifier on a group as a whole -- ADR 0011 keeps `functions`
/// dependency-free, so this is a matcher, not the `regex` crate, and log
/// line extraction has not needed any of those yet.
mod regex_lite {
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Atom {
        Lit(char),
        Any,
        Class { negate: bool },
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Quant {
        One,
        Star,
        Plus,
        Opt,
    }

    #[derive(Debug, Clone)]
    enum ClassItem {
        Char(char),
        Range(char, char),
    }

    #[derive(Debug, Clone)]
    enum Node {
        Atom {
            atom: Atom,
            items: Vec<ClassItem>,
            quant: Quant,
        },
        GroupOpen,
        GroupClose(usize),
    }

    /// A compiled pattern: a flat node list plus the number of capture
    /// groups (excluding group 0, the whole match).
    pub struct Compiled {
        nodes: Vec<Node>,
        group_count: usize,
    }

    /// Compile `pattern`, or `None` on malformed syntax (unterminated
    /// class/group, dangling quantifier, trailing backslash).
    pub fn compile(pattern: &str) -> Option<Compiled> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut nodes = Vec::new();
        let mut i = 0;
        let mut next_group = 1usize;
        let mut open_groups: Vec<usize> = Vec::new();
        while i < chars.len() {
            match chars[i] {
                '(' => {
                    nodes.push(Node::GroupOpen);
                    open_groups.push(next_group);
                    next_group = next_group.checked_add(1)?;
                    i = i.checked_add(1)?;
                }
                ')' => {
                    let idx = open_groups.pop()?;
                    nodes.push(Node::GroupClose(idx));
                    i = i.checked_add(1)?;
                }
                '.' => {
                    i = i.checked_add(1)?;
                    let quant = take_quant(&chars, &mut i);
                    nodes.push(Node::Atom {
                        atom: Atom::Any,
                        items: Vec::new(),
                        quant,
                    });
                }
                '[' => {
                    let (negate, items, next_i) = parse_class(&chars, i)?;
                    i = next_i;
                    let quant = take_quant(&chars, &mut i);
                    nodes.push(Node::Atom {
                        atom: Atom::Class { negate },
                        items,
                        quant,
                    });
                }
                '\\' => {
                    let c = *chars.get(i.checked_add(1)?)?;
                    i = i.checked_add(2)?;
                    let quant = take_quant(&chars, &mut i);
                    nodes.push(Node::Atom {
                        atom: Atom::Lit(c),
                        items: Vec::new(),
                        quant,
                    });
                }
                c => {
                    i = i.checked_add(1)?;
                    let quant = take_quant(&chars, &mut i);
                    nodes.push(Node::Atom {
                        atom: Atom::Lit(c),
                        items: Vec::new(),
                        quant,
                    });
                }
            }
        }
        if !open_groups.is_empty() {
            return None; // unterminated group
        }
        Some(Compiled {
            nodes,
            group_count: next_group.saturating_sub(1),
        })
    }

    fn take_quant(chars: &[char], i: &mut usize) -> Quant {
        match chars.get(*i) {
            Some('*') => {
                *i = i.saturating_add(1);
                Quant::Star
            }
            Some('+') => {
                *i = i.saturating_add(1);
                Quant::Plus
            }
            Some('?') => {
                *i = i.saturating_add(1);
                Quant::Opt
            }
            _ => Quant::One,
        }
    }

    /// Parses a `[...]`/`[^...]` class starting at `chars[start]`
    /// (`chars[start] == '['`); returns `(negate, items, index just past
    /// the closing ']')`, or `None` if unterminated.
    fn parse_class(chars: &[char], start: usize) -> Option<(bool, Vec<ClassItem>, usize)> {
        let mut i = start.checked_add(1)?;
        let negate = chars.get(i) == Some(&'^');
        if negate {
            i = i.checked_add(1)?;
        }
        let class_start = i;
        let mut items = Vec::new();
        loop {
            if i >= chars.len() {
                return None;
            }
            if chars[i] == ']' && i > class_start {
                return Some((negate, items, i.checked_add(1)?));
            }
            if i.checked_add(2)? < chars.len()
                && chars[i.checked_add(1)?] == '-'
                && chars[i.checked_add(2)?] != ']'
            {
                items.push(ClassItem::Range(chars[i], chars[i.checked_add(2)?]));
                i = i.checked_add(3)?;
            } else {
                items.push(ClassItem::Char(chars[i]));
                i = i.checked_add(1)?;
            }
        }
    }

    fn class_matches(items: &[ClassItem], negate: bool, c: char) -> bool {
        let hit = items.iter().any(|item| match item {
            ClassItem::Char(x) => *x == c,
            ClassItem::Range(lo, hi) => c >= *lo && c <= *hi,
        });
        hit != negate
    }

    fn atom_matches(atom: Atom, items: &[ClassItem], c: char) -> bool {
        match atom {
            Atom::Lit(x) => x == c,
            Atom::Any => true,
            Atom::Class { negate } => class_matches(items, negate, c),
        }
    }

    /// One match: the whole-match span plus each capture group's span
    /// (character offsets into the scanned text), 1-indexed by group
    /// number.
    pub struct Match {
        text: Vec<char>,
        whole: (usize, usize),
        groups: Vec<Option<(usize, usize)>>,
    }

    impl Match {
        /// Group 0 is the whole match; group N (1-based) is the Nth
        /// capture group, or `None` if it didn't participate or `group`
        /// is out of range.
        pub fn group(&self, n: usize) -> Option<String> {
            let span = if n == 0 {
                Some(self.whole)
            } else {
                *self.groups.get(n.checked_sub(1)?)?
            };
            span.map(|(s, e)| self.text[s..e].iter().collect())
        }
    }

    /// Find the first match of `re` anywhere in `text`, trying every
    /// start offset left to right (unanchored search -- no anchors
    /// exist in this subset, so "first match" needs an explicit scan).
    pub fn find(re: &Compiled, text: &str) -> Option<Match> {
        let chars: Vec<char> = text.chars().collect();
        for start in 0..=chars.len() {
            let mut groups = vec![None; re.group_count];
            let mut open: Vec<usize> = Vec::new();
            if let Some(end) = match_seq(&re.nodes, 0, &chars, start, &mut groups, &mut open) {
                return Some(Match {
                    text: chars,
                    whole: (start, end),
                    groups,
                });
            }
        }
        None
    }

    fn match_seq(
        nodes: &[Node],
        ni: usize,
        text: &[char],
        ti: usize,
        groups: &mut Vec<Option<(usize, usize)>>,
        open: &mut Vec<usize>,
    ) -> Option<usize> {
        let Some(node) = nodes.get(ni) else {
            return Some(ti);
        };
        match node {
            Node::GroupOpen => {
                open.push(ti);
                let r = match_seq(nodes, ni.saturating_add(1), text, ti, groups, open);
                if r.is_none() {
                    open.pop();
                }
                r
            }
            Node::GroupClose(idx) => {
                let start = *open.last()?;
                open.pop();
                let slot = idx.checked_sub(1)?;
                let previous = groups.get(slot).copied().unwrap_or(None);
                if let Some(g) = groups.get_mut(slot) {
                    *g = Some((start, ti));
                }
                let r = match_seq(nodes, ni.saturating_add(1), text, ti, groups, open);
                if r.is_none() {
                    if let Some(g) = groups.get_mut(slot) {
                        *g = previous;
                    }
                    open.push(start);
                }
                r
            }
            Node::Atom { atom, items, quant } => match quant {
                Quant::One => {
                    let c = *text.get(ti)?;
                    if atom_matches(*atom, items, c) {
                        match_seq(
                            nodes,
                            ni.saturating_add(1),
                            text,
                            ti.saturating_add(1),
                            groups,
                            open,
                        )
                    } else {
                        None
                    }
                }
                Quant::Opt => try_repeat(nodes, ni, *atom, items, 0, 1, text, ti, groups, open),
                Quant::Star => try_repeat(
                    nodes,
                    ni,
                    *atom,
                    items,
                    0,
                    usize::MAX,
                    text,
                    ti,
                    groups,
                    open,
                ),
                Quant::Plus => try_repeat(
                    nodes,
                    ni,
                    *atom,
                    items,
                    1,
                    usize::MAX,
                    text,
                    ti,
                    groups,
                    open,
                ),
            },
        }
    }

    /// Greedy quantifier: collects every consecutive match of `atom`
    /// starting at `ti`, then backtracks from the longest run down to
    /// `min` repeats looking for one from which the rest of the pattern
    /// also matches.
    #[allow(clippy::too_many_arguments)]
    fn try_repeat(
        nodes: &[Node],
        ni: usize,
        atom: Atom,
        items: &[ClassItem],
        min: usize,
        max: usize,
        text: &[char],
        ti: usize,
        groups: &mut Vec<Option<(usize, usize)>>,
        open: &mut Vec<usize>,
    ) -> Option<usize> {
        let mut positions = vec![ti];
        let mut cur = ti;
        while positions.len().saturating_sub(1) < max
            && text.get(cur).is_some_and(|c| atom_matches(atom, items, *c))
        {
            cur = cur.saturating_add(1);
            positions.push(cur);
        }
        let last = positions.len().saturating_sub(1);
        if min > last {
            return None;
        }
        for k in (min..=last).rev() {
            if let Some(end) = match_seq(
                nodes,
                ni.saturating_add(1),
                text,
                positions[k],
                groups,
                open,
            ) {
                return Some(end);
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn extract(pattern: &str, text: &str, group: usize) -> Option<String> {
            find(&compile(pattern)?, text)?.group(group)
        }

        #[test]
        fn matches_literal_substring() {
            assert_eq!(extract("bc", "abcd", 0), Some("bc".to_string()));
        }

        #[test]
        fn dot_matches_any_char() {
            assert_eq!(extract("a.c", "xabcx", 0), Some("abc".to_string()));
        }

        #[test]
        fn star_matches_zero_or_more() {
            assert_eq!(extract("ab*c", "ac", 0), Some("ac".to_string()));
            assert_eq!(extract("ab*c", "abbbc", 0), Some("abbbc".to_string()));
        }

        #[test]
        fn plus_requires_at_least_one() {
            assert_eq!(extract("ab+c", "ac", 0), None);
            assert_eq!(extract("ab+c", "abc", 0), Some("abc".to_string()));
        }

        #[test]
        fn opt_matches_zero_or_one() {
            assert_eq!(extract("ab?c", "ac", 0), Some("ac".to_string()));
            assert_eq!(extract("ab?c", "abc", 0), Some("abc".to_string()));
        }

        #[test]
        fn char_class_and_range() {
            assert_eq!(
                extract("[0-9]+", "port 8080 ok", 0),
                Some("8080".to_string())
            );
            assert_eq!(extract("[^0-9]+", "8080abc", 0), Some("abc".to_string()));
        }

        #[test]
        fn capture_group_extracts_submatch() {
            assert_eq!(
                extract(r"duration=([0-9]+)ms", "duration=184ms", 1),
                Some("184".to_string())
            );
            assert_eq!(
                extract(r"duration=([0-9]+)ms", "duration=184ms", 0),
                Some("duration=184ms".to_string())
            );
        }

        #[test]
        fn no_match_returns_none() {
            assert_eq!(extract("xyz", "abc", 0), None);
        }

        #[test]
        fn escaped_metachar_is_literal() {
            assert_eq!(extract(r"a\.b", "a.b", 0), Some("a.b".to_string()));
            assert_eq!(extract(r"a\.b", "axb", 0), None);
        }

        #[test]
        fn unterminated_group_fails_to_compile() {
            assert!(compile("(abc").is_none());
        }

        #[test]
        fn unterminated_class_fails_to_compile() {
            assert!(compile("[abc").is_none());
        }

        /// MC/DC vector (obligation `functions_931`, `parse_class`'s
        /// terminator check `chars[i] == ']' && i > class_start`): leaf A
        /// false (not a `]`) -- an ordinary class member, scanning
        /// continues.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_931__v1_non_bracket_continues() {
            assert_eq!(extract("[ab]", "xaz", 0), Some("a".to_string()));
        }

        /// MC/DC vector (obligation `functions_931`): leaf A true, leaf B
        /// (past the class start) false -- a `]` as the very first class
        /// character is a literal member, not the terminator.
        /// Independence pair for B against `mcdc__functions_931__
        /// v3_bracket_past_start_terminates`.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_931__v2_leading_bracket_is_literal_member() {
            assert_eq!(extract("[]a]", "x]z", 0), Some("]".to_string()));
        }

        /// MC/DC vector (obligation `functions_931`): both leaves true --
        /// a `]` past the class start terminates the class. Independence
        /// pair for A against `mcdc__functions_931__v1_non_bracket_
        /// continues`.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_931__v3_bracket_past_start_terminates() {
            assert_eq!(extract("[a]b", "xay", 0), None);
            assert_eq!(extract("[a]b", "xaby", 0), Some("ab".to_string()));
        }

        /// MC/DC vector (obligation `functions_934`, `parse_class`'s
        /// range-detection decision `i+2 < chars.len() && chars[i+1] ==
        /// '-' && chars[i+2] != ']'`, 3 conditions): all leaves true -- a
        /// real range like `a-z`.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_934__v1_all_true_is_range() {
            assert_eq!(extract("[a-z]", "XmY", 0), Some("m".to_string()));
        }

        /// MC/DC vector (obligation `functions_934`): leaves A and B
        /// true, leaf C (`chars[i+2] != ']'`) false -- `-` immediately
        /// followed by the class terminator is not a range, so `-` is a
        /// literal member.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_934__v2_dash_before_terminator_is_literal() {
            assert_eq!(extract("[a-]", "x-y", 0), Some("-".to_string()));
        }

        /// MC/DC vector (obligation `functions_934`): leaf A true, leaf B
        /// (`chars[i+1] == '-'`) false -- no dash follows, so it's a
        /// plain class member, not a range.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_934__v3_no_dash_is_not_range() {
            assert_eq!(extract("[ab]", "xbz", 0), Some("b".to_string()));
        }

        /// MC/DC vector (obligation `functions_934`): leaf A
        /// (`i+2 < chars.len()`) false -- too close to the end of the
        /// class for a range, so treated as a literal member.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_934__v4_too_short_for_range() {
            assert_eq!(extract("[a]", "xay", 0), Some("a".to_string()));
        }

        /// MC/DC vector (obligation `functions_1106`, `try_repeat`'s
        /// collection-loop decision `positions.len() - 1 < max &&
        /// text.get(cur).is_some_and(...)`): both leaves true -- under
        /// the repeat cap and the next character still matches, so
        /// collection continues.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_1106__v1_under_cap_and_matches_continues() {
            assert_eq!(extract("ab+c", "abbbc", 0), Some("abbbc".to_string()));
        }

        /// MC/DC vector (obligation `functions_1106`): leaf A true, leaf
        /// B false -- still under the cap, but the next character
        /// doesn't match the atom, so collection stops with zero
        /// repeats.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_1106__v2_under_cap_but_no_match_stops() {
            assert_eq!(extract("ab*c", "ac", 0), Some("ac".to_string()));
        }

        /// MC/DC vector (obligation `functions_1106`): leaf A false --
        /// the repeat cap is reached, so collection stops regardless of
        /// whether another character would have matched.
        #[test]
        #[allow(non_snake_case)]
        fn mcdc__functions_1106__v3_cap_reached_stops() {
            assert_eq!(extract("ab?c", "abc", 0), Some("abc".to_string()));
        }
    }
}

type ScalarFn = fn(&[Value]) -> Result<Value, FunctionError>;

/// Every scalar function `call` dispatches, kept next to it deliberately
/// so the two can't drift apart -- adding a name here without a matching
/// arm in `call` (or vice versa) is a copy/paste error away, not a
/// silent divergence discovered later. A client-side completion popup
/// (t-rust-db/db-studio#46) consumes this instead of hand-maintaining
/// its own function-name list. Aggregate names (`COUNT`/`SUM`/...) and
/// window functions aren't included -- they're recognized by each
/// planner's own validator (e.g. `parser::column::is_known_agg_name`),
/// not this scalar registry.
pub const SCALAR_FUNCTION_NAMES: &[&str] = &[
    "length",
    "upper",
    "lower",
    "abs",
    "coalesce",
    "ifnull",
    "nullif",
    "typeof",
    "sqlite_version",
    "hex",
    "unhex",
    "quote",
    "min",
    "max",
    "round",
    "sign",
    "instr",
    "zeroblob",
    "iif",
    "substr",
    "trim",
    "ltrim",
    "rtrim",
    "replace",
    "like",
    "glob",
    "json_extract",
    "logfmt_extract",
    "regexp_extract",
];

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
        ("json_extract", 2) => Some(json_extract),
        ("logfmt_extract", 2) => Some(logfmt_extract),
        ("regexp_extract", 3) => Some(regexp_extract),
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
        assert_eq!(i64::try_from(b.len()).unwrap(), MAX_BLOB_LEN);
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

    /// MC/DC vector (obligation `functions_117`, `nullif`'s decision
    /// `matches!(a, Value::Null) || matches!(b, Value::Null)`): leaf A
    /// (`a` is NULL) true -- returns `a` (NULL) regardless of `b`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_144__v1_lhs_null_returns_lhs() {
        assert_eq!(v("nullif", &[Value::Null, Value::Integer(1)]), Value::Null);
    }

    /// MC/DC vector (obligation `functions_117`): both leaves false --
    /// the equal-value comparison actually runs. Independence pair for A
    /// against `mcdc__functions_144__v1_lhs_null_returns_lhs`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_144__v2_neither_null_compares() {
        assert_eq!(
            v("nullif", &[Value::Integer(1), Value::Integer(1)]),
            Value::Null
        );
    }

    /// MC/DC vector (obligation `functions_117`): leaf B (`b` is NULL)
    /// true, leaf A false -- returns `a` unchanged. Independence pair for
    /// B against `mcdc__functions_144__v2_neither_null_compares`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_144__v3_rhs_null_returns_lhs() {
        assert_eq!(
            v("nullif", &[Value::Integer(1), Value::Null]),
            Value::Integer(1)
        );
    }

    /// MC/DC vector (obligation `functions_243`, `round_fn`'s decision
    /// `matches!(args[0], Value::Null) || matches!(args.get(1),
    /// Some(Value::Null))`): leaf A (`args[0]` is NULL) true.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_270__v1_first_arg_null() {
        assert_eq!(v("round", &[Value::Null, Value::Integer(1)]), Value::Null);
    }

    /// MC/DC vector (obligation `functions_243`): both leaves false --
    /// rounding actually runs. Independence pair for A against
    /// `mcdc__functions_270__v1_first_arg_null`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_270__v2_neither_null_rounds() {
        assert_eq!(
            v("round", &[Value::Real(2.5), Value::Integer(0)]),
            Value::Real(3.0)
        );
    }

    /// MC/DC vector (obligation `functions_243`): leaf B (the optional
    /// second arg is present and NULL) true, leaf A false. Independence
    /// pair for B against `mcdc__functions_270__v2_neither_null_rounds`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_270__v3_second_arg_null() {
        assert_eq!(v("round", &[Value::Real(2.5), Value::Null]), Value::Null);
    }

    /// MC/DC vector (obligation `functions_286`, `instr`'s decision
    /// `matches!(args[0], Value::Null) || matches!(args[1],
    /// Value::Null)`): leaf A (`args[0]` is NULL) true.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_313__v1_haystack_null() {
        assert_eq!(
            v("instr", &[Value::Null, Value::Text("a".to_string().into())]),
            Value::Null
        );
    }

    /// MC/DC vector (obligation `functions_286`): both leaves false --
    /// the search actually runs. Independence pair for A against
    /// `mcdc__functions_313__v1_haystack_null`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_313__v2_neither_null_searches() {
        assert_eq!(
            v(
                "instr",
                &[
                    Value::Text("hello".to_string().into()),
                    Value::Text("ell".to_string().into())
                ]
            ),
            Value::Integer(2)
        );
    }

    /// MC/DC vector (obligation `functions_286`): leaf B (`args[1]` is
    /// NULL) true, leaf A false. Independence pair for B against
    /// `mcdc__functions_313__v2_neither_null_searches`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_313__v3_needle_null() {
        assert_eq!(
            v(
                "instr",
                &[Value::Text("hello".to_string().into()), Value::Null]
            ),
            Value::Null
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

    /// MC/DC vector (obligation `functions_353`, `substr`'s decision
    /// `matches!(args[1], Value::Null) || args.get(2).is_some_and(|v|
    /// matches!(v, Value::Null))`): leaf A (`y` is NULL) true -- returns
    /// NULL regardless of `z`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_353__v1_y_null_returns_null() {
        assert_eq!(
            v(
                "substr",
                &[Value::Text("abc".to_string().into()), Value::Null]
            ),
            Value::Null
        );
    }

    /// MC/DC vector (obligation `functions_353`): leaf A false, leaf B
    /// (`z` is NULL) true -- also returns NULL. Independence pair for B
    /// against `mcdc__functions_353__v3_neither_null_extracts`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_353__v2_z_null_returns_null() {
        assert_eq!(
            v(
                "substr",
                &[
                    Value::Text("abc".to_string().into()),
                    Value::Integer(1),
                    Value::Null
                ]
            ),
            Value::Null
        );
    }

    /// MC/DC vector (obligation `functions_353`): both leaves false --
    /// substr actually runs. Independence pair for A against
    /// `mcdc__functions_353__v1_y_null_returns_null`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_353__v3_neither_null_extracts() {
        assert_eq!(
            v(
                "substr",
                &[Value::Text("abc".to_string().into()), Value::Integer(1)]
            ),
            Value::Text("abc".to_string().into())
        );
    }

    /// MC/DC vector (obligation `functions_494`, `like_rec`'s decision
    /// `Some(pc) == escape && pi.saturating_add(1) < p.len()`): leaf A
    /// (`pc` is the escape char) false -- the escape branch is skipped
    /// entirely and `pc` is handled as a normal pattern char.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_494__v1_not_escape_char_skips_branch() {
        assert!(like_match("a", "a", Some('\\')));
    }

    /// MC/DC vector (obligation `functions_494`): leaf A true, leaf B
    /// (a char follows the escape) false -- the escape char is the last
    /// pattern char, so the branch is skipped and it's matched literally
    /// instead. Independence pair for B against
    /// `mcdc__functions_494__v3_escape_with_following_char_takes_branch`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_494__v2_trailing_escape_has_no_following_char() {
        assert!(like_match("\\", "\\", Some('\\')));
    }

    /// MC/DC vector (obligation `functions_494`): both leaves true -- the
    /// escape branch is taken, so the following char is matched
    /// literally. Independence pair for A against
    /// `mcdc__functions_494__v1_not_escape_char_skips_branch`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_494__v3_escape_with_following_char_takes_branch() {
        assert!(like_match("%", "\\%", Some('\\')));
        assert!(!like_match("x", "\\%", Some('\\')));
    }

    /// MC/DC vector (obligation `functions_496`, `like_rec`'s escaped-
    /// literal decision `ti >= t.len() || !ascii_eq(t[ti], literal)`):
    /// leaf A (text exhausted) true -- no match regardless of `literal`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_496__v1_text_exhausted_no_match() {
        assert!(!like_match("", "\\%", Some('\\')));
    }

    /// MC/DC vector (obligation `functions_496`): leaf A false, leaf B
    /// (char mismatch) true -- no match. Independence pair for B against
    /// `mcdc__functions_496__v3_escaped_literal_matches`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_496__v2_escaped_literal_mismatches() {
        assert!(!like_match("x", "\\%", Some('\\')));
    }

    /// MC/DC vector (obligation `functions_496`): both leaves false --
    /// the escaped literal matches, so matching continues. Independence
    /// pair for A against `mcdc__functions_496__v1_text_exhausted_no_match`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_496__v3_escaped_literal_matches() {
        assert!(like_match("%", "\\%", Some('\\')));
    }

    /// MC/DC vector (obligation `functions_506`, `like_rec`'s `%`
    /// collapse-loop decision `pi < p.len() && p[pi] == '%'`): both
    /// leaves true -- a run of two `%` collapses through one extra
    /// iteration.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_506__v1_run_of_percent_collapses() {
        assert!(like_match("axb", "a%%b", None));
    }

    /// MC/DC vector (obligation `functions_506`): leaf A true, leaf B
    /// (next pattern char is `%`) false -- the loop stops after a single
    /// `%` because the following char differs. Independence pair for B
    /// against `mcdc__functions_506__v1_run_of_percent_collapses`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_506__v2_single_percent_then_other_char() {
        assert!(like_match("axb", "a%b", None));
    }

    /// MC/DC vector (obligation `functions_506`): leaf A (more pattern
    /// left) false -- the loop stops because `%` is the last pattern
    /// char, matching the rest of the text unconditionally. Independence
    /// pair for A against `mcdc__functions_506__v2_single_percent_then_other_char`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_506__v3_trailing_percent_matches_end() {
        assert!(like_match("axyz", "a%", None));
    }

    /// MC/DC vector (obligation `functions_527`, `like_rec`'s literal
    /// char decision `ti >= t.len() || !ascii_eq(t[ti], pc)`): leaf A
    /// (text exhausted) true -- no match regardless of `pc`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_527__v1_text_exhausted_no_match() {
        assert!(!like_match("", "a", None));
    }

    /// MC/DC vector (obligation `functions_527`): leaf A false, leaf B
    /// (char mismatch) true -- no match. Independence pair for B against
    /// `mcdc__functions_527__v3_literal_matches`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_527__v2_literal_mismatches() {
        assert!(!like_match("x", "a", None));
    }

    /// MC/DC vector (obligation `functions_527`): both leaves false --
    /// the literal matches. Independence pair for A against
    /// `mcdc__functions_527__v1_text_exhausted_no_match`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_527__v3_literal_matches() {
        assert!(like_match("a", "a", None));
    }

    /// MC/DC vector (obligation `functions_557`, `glob_rec`'s `*`
    /// collapse-loop decision `pi < p.len() && p[pi] == '*'`): both
    /// leaves true -- a run of two `*` collapses through one extra
    /// iteration.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_557__v1_run_of_star_collapses() {
        assert!(glob_match("axb", "a**b"));
    }

    /// MC/DC vector (obligation `functions_557`): leaf A true, leaf B
    /// (next pattern char is `*`) false -- the loop stops after a single
    /// `*` because the following char differs. Independence pair for B
    /// against `mcdc__functions_557__v1_run_of_star_collapses`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_557__v2_single_star_then_other_char() {
        assert!(glob_match("axb", "a*b"));
    }

    /// MC/DC vector (obligation `functions_557`): leaf A (more pattern
    /// left) false -- the loop stops because `*` is the last pattern
    /// char, matching the rest of the text unconditionally. Independence
    /// pair for A against `mcdc__functions_557__v2_single_star_then_other_char`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_557__v3_trailing_star_matches_end() {
        assert!(glob_match("axyz", "a*"));
    }

    /// MC/DC vector (obligation `functions_581`, `glob_rec`'s character-
    /// class decision `ti >= t.len() || !matches`): leaf A (text
    /// exhausted) true -- no match regardless of `matches`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_581__v1_text_exhausted_no_match() {
        assert!(!glob_match("", "[a-z]"));
    }

    /// MC/DC vector (obligation `functions_581`): leaf A false, leaf B
    /// (class didn't match) true -- no match. Independence pair for B
    /// against `mcdc__functions_581__v3_class_matches`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_581__v2_class_mismatches() {
        assert!(!glob_match("Z", "[a-z]"));
    }

    /// MC/DC vector (obligation `functions_581`): both leaves false --
    /// the class matches. Independence pair for A against
    /// `mcdc__functions_581__v1_text_exhausted_no_match`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_581__v3_class_matches() {
        assert!(glob_match("m", "[a-z]"));
    }

    /// MC/DC vector (obligation `functions_588`, `glob_rec`'s literal
    /// char decision `ti >= t.len() || t[ti] != c`): leaf A (text
    /// exhausted) true -- no match regardless of `c`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_588__v1_text_exhausted_no_match() {
        assert!(!glob_match("", "a"));
    }

    /// MC/DC vector (obligation `functions_588`): leaf A false, leaf B
    /// (char mismatch) true -- no match. Independence pair for B against
    /// `mcdc__functions_588__v3_literal_matches`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_588__v2_literal_mismatches() {
        assert!(!glob_match("x", "a"));
    }

    /// MC/DC vector (obligation `functions_588`): both leaves false --
    /// the literal matches. Independence pair for A against
    /// `mcdc__functions_588__v1_text_exhausted_no_match`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_588__v3_literal_matches() {
        assert!(glob_match("a", "a"));
    }

    /// MC/DC vector (obligation `functions_613`, `glob_class`'s
    /// terminator decision `p[i] == ']' && i > class_start`): leaf A
    /// (`p[i]` is `']'`) false -- an ordinary class char, loop keeps
    /// going.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_613__v1_non_bracket_continues() {
        assert!(glob_match("a", "[ab]"));
    }

    /// MC/DC vector (obligation `functions_613`): leaf A true, leaf B
    /// (past the class start) false -- a `]` as the very first class
    /// char is a literal member, not the terminator. Independence pair
    /// for B against `mcdc__functions_613__v3_bracket_past_start_terminates`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_613__v2_leading_bracket_is_literal_member() {
        assert!(glob_match("]", "[]a]"));
    }

    /// MC/DC vector (obligation `functions_613`): both leaves true -- a
    /// `]` past the class start terminates the class. Independence pair
    /// for A against `mcdc__functions_613__v1_non_bracket_continues`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_613__v3_bracket_past_start_terminates() {
        assert!(!glob_match("a", "[a]b"));
        assert!(glob_match("ab", "[a]b"));
    }

    /// MC/DC vector (obligation `functions_617`, `glob_class`'s range-
    /// detection decision `i.saturating_add(2) < p.len() && p[i + 1] ==
    /// '-' && p[i + 2] != ']'`, 3 conditions): all leaves true -- a real
    /// range like `a-z`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_617__v1_all_true_is_range() {
        assert!(glob_match("m", "[a-z]"));
    }

    /// MC/DC vector (obligation `functions_617`): leaves A and B true,
    /// leaf C (`p[i+2] != ']'`) false -- `-` immediately followed by the
    /// class terminator is not a range, so `-` is a literal member.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_617__v2_dash_before_terminator_is_literal() {
        assert!(glob_match("-", "[a-]"));
    }

    /// MC/DC vector (obligation `functions_617`): leaf A true, leaf B
    /// (`p[i+1] == '-'`) false -- no dash follows, so it's a plain class
    /// member, not a range.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_617__v3_no_dash_is_not_range() {
        assert!(glob_match("b", "[ab]"));
    }

    /// MC/DC vector (obligation `functions_617`): leaf A (`i+2 <
    /// p.len()`) false -- too close to the end of the pattern for a
    /// range, so treated as a literal member (the class ends up
    /// unterminated here, so `glob` reports no match).
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_617__v4_too_short_for_range() {
        assert!(!glob_match("a", "[a-"));
    }

    /// MC/DC vector (obligation `functions_623`, `glob_class`'s range-
    /// bounds decision `c >= lo && c <= hi`): both leaves true -- `c`
    /// falls inside `[lo, hi]`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_623__v1_within_bounds_matches() {
        assert!(glob_match("m", "[a-z]"));
    }

    /// MC/DC vector (obligation `functions_623`): leaf A (`c >= lo`)
    /// false -- below the lower bound, no match regardless of `c <= hi`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_623__v2_below_lower_bound() {
        assert!(!glob_match("A", "[a-z]"));
    }

    /// MC/DC vector (obligation `functions_623`): leaf A true, leaf B
    /// (`c <= hi`) false -- above the upper bound, no match.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__functions_623__v3_above_upper_bound() {
        assert!(!glob_match("~", "[a-z]"));
    }

    #[test]
    fn trigrams_yields_every_overlapping_three_byte_window() {
        let got: Vec<[u8; 3]> = trigrams("abcd").collect();
        assert_eq!(got, vec![*b"abc", *b"bcd"]);
    }

    #[test]
    fn trigrams_windows_raw_utf8_bytes_not_chars() {
        // "é" is 2 UTF-8 bytes (0xC3 0xA9); trigrams windows the raw
        // bytes, so a multi-byte character contributes to more than
        // one trigram, same as ripgrep/tgrep's own byte-level trigrams.
        let text = "aébc";
        let bytes = text.as_bytes();
        assert_eq!(bytes.len(), 5);
        let got: Vec<[u8; 3]> = trigrams(text).collect();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], [bytes[0], bytes[1], bytes[2]]);
        assert_eq!(got[1], [bytes[1], bytes[2], bytes[3]]);
        assert_eq!(got[2], [bytes[2], bytes[3], bytes[4]]);
    }

    #[test]
    fn trigrams_of_under_three_bytes_is_empty() {
        assert_eq!(trigrams("").count(), 0);
        assert_eq!(trigrams("a").count(), 0);
        assert_eq!(trigrams("ab").count(), 0);
    }

    #[test]
    fn trigrams_of_exactly_three_bytes_yields_one() {
        let got: Vec<[u8; 3]> = trigrams("xyz").collect();
        assert_eq!(got, vec![*b"xyz"]);
    }

    #[test]
    fn trigram_key_packs_bytes_big_endian_and_stays_distinct() {
        assert_eq!(trigram_key(*b"abc"), trigram_key(*b"abc"));
        assert_ne!(trigram_key(*b"abc"), trigram_key(*b"abd"));
        assert_eq!(trigram_key([0, 0, 0]), 0);
        assert_eq!(trigram_key([0, 0, 1]), 1);
        assert_eq!(trigram_key([0, 1, 0]), 0x100);
        assert_eq!(trigram_key([1, 0, 0]), 0x1_0000);
    }
}
