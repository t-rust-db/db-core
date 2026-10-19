// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! sqlite3's core text and miscellaneous functions (`func.c`,
//! `printf.c`) that the first slices didn't port (#651): `char`,
//! `concat`, `concat_ws`, `format`/`printf`, `likely`/`unlikely`/
//! `likelihood`, `octet_length`, `random`, `randomblob`, `substring`,
//! `unicode`, `unistr`, and the build-introspection functions. sqlite3
//! 3.53.4 is the oracle (`make check-row-functions-oracle`).
//!
//! `changes()`, `total_changes()` and `last_insert_rowid()` are listed
//! here so a statement naming them compiles, but the row VM answers them
//! from the connection's counters before the registry is consulted
//! (`vm::row::vm`, `Opcode::Function`); called without a connection they
//! are an error.
//!
//! Three report on the build rather than on SQL, so they describe
//! db-core, not sqlite3: `sqlite_source_id()` is db-core's version,
//! `sqlite_compileoption_used`/`_get` see no compile options, and
//! `sqlite_offset()` is NULL (db-core has no row byte offsets to report).

use std::hash::{BuildHasher, Hasher};

use super::{as_text, FunctionError, ScalarFn, MAX_LENGTH};
use crate::value::Value;

/// The function named `name` (lowercase) at `arity`, if this family has
/// it.
pub(super) fn lookup(name: &str, arity: usize) -> Option<ScalarFn> {
    let f: ScalarFn = match (name, arity) {
        ("char", _) => char_fn,
        ("concat", n) if n >= 1 => concat,
        ("concat_ws", n) if n >= 2 => concat_ws,
        ("format" | "printf", n) if n >= 1 => printf,
        ("likely" | "unlikely", 1) => first,
        ("likelihood", 2) => likelihood,
        ("octet_length", 1) => octet_length,
        ("random", 0) => random,
        ("randomblob", 1) => randomblob,
        ("substring", 2 | 3) => super::substr,
        ("unicode", 1) => unicode,
        ("unistr", 1) => unistr,
        ("sqlite_source_id", 0) => {
            |_| Ok(Value::Text(format!("db-core {}", crate::VERSION).into()))
        }
        ("sqlite_compileoption_used", 1) => compileoption_used,
        ("sqlite_compileoption_get", 1) => |_| Ok(Value::Null),
        ("sqlite_offset", 1) => |_| Ok(Value::Null),
        ("changes" | "total_changes" | "last_insert_rowid", 0) => |_| {
            Err(FunctionError::Sql(
                "changes(), total_changes() and last_insert_rowid() need a connection".into(),
            ))
        },
        _ => return None,
    };
    Some(f)
}

fn text(s: String) -> Value {
    Value::Text(s.into())
}

/// `char(X1, ..., XN)`: the characters with those code points. sqlite3
/// writes an out-of-range code point as U+FFFD; a surrogate it writes as
/// three bytes that are not UTF-8, which read back as three U+FFFD.
fn char_fn(args: &[Value]) -> Result<Value, FunctionError> {
    let mut out = String::new();
    for arg in args {
        let code = crate::coerce::cast_to_integer(arg);
        match u32::try_from(code).ok().and_then(char::from_u32) {
            Some(c) => out.push(c),
            None if (0xD800..=0xDFFF).contains(&code) => out.push_str("\u{FFFD}\u{FFFD}\u{FFFD}"),
            None => out.push('\u{FFFD}'),
        }
    }
    Ok(text(out))
}

/// `concat(X, ...)`: the arguments' text, NULLs skipped.
fn concat(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(text(args.iter().map(as_text).collect()))
}

/// `concat_ws(SEP, X, ...)`: the non-NULL arguments joined by `SEP`;
/// NULL when `SEP` is.
fn concat_ws(args: &[Value]) -> Result<Value, FunctionError> {
    let Some((sep, rest)) = args.split_first() else {
        return Ok(Value::Null);
    };
    if matches!(sep, Value::Null) {
        return Ok(Value::Null);
    }
    let parts: Vec<String> = rest
        .iter()
        .filter(|v| !matches!(v, Value::Null))
        .map(as_text)
        .collect();
    Ok(text(parts.join(&as_text(sep))))
}

/// `likely(X)`/`unlikely(X)`: `X`, a planner hint only.
fn first(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(args.first().cloned().unwrap_or(Value::Null))
}

/// `likelihood(X, P)`: `X`; `P` must be a number between 0.0 and 1.0
/// (sqlite3 checks it when preparing; here, when the row reaches it).
fn likelihood(args: &[Value]) -> Result<Value, FunctionError> {
    let p = match args.get(1) {
        Some(Value::Real(r)) => Some(*r),
        #[allow(
            clippy::cast_precision_loss,
            reason = "only compared against 0.0 and 1.0"
        )]
        Some(Value::Integer(i)) => Some(*i as f64),
        _ => None,
    };
    if !p.is_some_and(|p| (0.0..=1.0).contains(&p)) {
        return Err(FunctionError::Sql(
            "second argument to likelihood() must be a constant between 0.0 and 1.0".into(),
        ));
    }
    first(args)
}

/// `octet_length(X)`: the bytes of `X`'s text (or of the blob).
fn octet_length(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(match args.first() {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Blob(b)) => Value::Integer(crate::value::len_to_i64(b.len())),
        Some(v) => Value::Integer(crate::value::len_to_i64(as_text(v).len())),
    })
}

/// 64 random bits: std's per-process random keys, stirred by a counter
/// so successive calls differ. Not cryptographic (sqlite3's is, but
/// `random()` promises only "pseudo-random").
fn random_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(CALLS.fetch_add(1, Ordering::Relaxed));
    hasher.finish()
}

/// `random()`: a random 64-bit signed integer.
fn random(_args: &[Value]) -> Result<Value, FunctionError> {
    Ok(Value::Integer(i64::from_ne_bytes(
        random_u64().to_ne_bytes(),
    )))
}

/// `randomblob(N)`: `N` random bytes; fewer than 1 is 1.
fn randomblob(args: &[Value]) -> Result<Value, FunctionError> {
    let n = args
        .first()
        .map_or(1, crate::coerce::cast_to_integer)
        .max(1);
    let n = usize::try_from(n).unwrap_or(1);
    if n > MAX_LENGTH {
        return Err(FunctionError::TooBig);
    }
    let mut bytes = Vec::with_capacity(n);
    while bytes.len() < n {
        bytes.extend(random_u64().to_ne_bytes());
    }
    bytes.truncate(n);
    Ok(Value::Blob(bytes.into()))
}

/// `unicode(X)`: the code point of `X`'s first character; NULL for NULL
/// or empty.
fn unicode(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(match args.first() {
        None | Some(Value::Null) => Value::Null,
        Some(v) => as_text(v)
            .chars()
            .next()
            .map_or(Value::Null, |c| Value::Integer(i64::from(u32::from(c)))),
    })
}

/// `unistr(X)`: `X` with sqlite3's escapes decoded -- `\XXXX`, `\uXXXX`,
/// `\+XXXXXX`, `\UXXXXXXXX` (hex code points) and `\\`. Any other
/// backslash is an error.
fn unistr(args: &[Value]) -> Result<Value, FunctionError> {
    let s = match args.first() {
        None | Some(Value::Null) => return Ok(Value::Null),
        Some(v) => as_text(v),
    };
    let invalid = || FunctionError::Sql("invalid Unicode escape".into());
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits = match chars.peek() {
            Some('\\') => {
                chars.next();
                out.push('\\');
                continue;
            }
            Some('u') => {
                chars.next();
                4
            }
            Some('+') => {
                chars.next();
                6
            }
            Some('U') => {
                chars.next();
                8
            }
            _ => 4,
        };
        let mut code = 0_u32;
        for _ in 0..digits {
            let d = chars
                .next()
                .and_then(|d| d.to_digit(16))
                .ok_or_else(invalid)?;
            code = code
                .checked_mul(16)
                .and_then(|c| c.checked_add(d))
                .ok_or_else(invalid)?;
        }
        out.push(char::from_u32(code).ok_or_else(invalid)?);
    }
    Ok(text(out))
}

/// `sqlite_compileoption_used(X)`: db-core has no sqlite3 compile
/// options, so 0 (NULL for NULL).
fn compileoption_used(args: &[Value]) -> Result<Value, FunctionError> {
    Ok(match args.first() {
        None | Some(Value::Null) => Value::Null,
        Some(_) => Value::Integer(0),
    })
}

/// `printf(FORMAT, ...)`/`format(...)`: sqlite3's `printf.c`. NULL for a
/// NULL format; a conversion it does not know (or a format that ends
/// inside one) stops the output there. The result is NULL when it is
/// empty and no conversion ran (`printf('')`, `printf('%y')`), as
/// sqlite3's never-allocated buffer is.
fn printf(args: &[Value]) -> Result<Value, FunctionError> {
    let Some(format) = args.first() else {
        return Ok(Value::Null);
    };
    if matches!(format, Value::Null) {
        return Ok(Value::Null);
    }
    let format = as_text(format);
    let mut rest = args.get(1..).unwrap_or_default().iter();
    let mut out = String::new();
    let mut converted = false;
    let finish = |out: String, converted: bool| {
        if out.is_empty() && !converted {
            Value::Null
        } else {
            text(out)
        }
    };
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek().is_none() {
            out.push('%');
            break;
        }
        let Some(spec) = Spec::parse(&mut chars, &mut rest) else {
            return Ok(finish(out, converted));
        };
        if spec.too_big() {
            return Err(FunctionError::TooBig);
        }
        let arg = if spec.takes_arg() { rest.next() } else { None };
        if out.len().saturating_add(spec.size_bound(arg)) > MAX_LENGTH {
            return Err(FunctionError::TooBig);
        }
        let Some(piece) = spec.convert(arg) else {
            return Ok(finish(out, false));
        };
        converted = true;
        out.push_str(&piece);
        if out.len() > MAX_LENGTH {
            return Err(FunctionError::TooBig);
        }
    }
    Ok(finish(out, converted))
}

/// The English ordinal suffix of `n`.
fn ordinal_suffix(n: u64) -> &'static str {
    match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    }
}

/// A printf argument as an integer: sqlite3's `sqlite3_value_int64`
/// (a numeric prefix of text or a blob's bytes, a REAL truncated and
/// clamped, NULL or missing 0).
fn int_arg(v: Option<&Value>) -> i64 {
    match v {
        None | Some(Value::Null) => 0,
        Some(Value::Blob(b)) => crate::coerce::cast_to_integer(&Value::Text(
            String::from_utf8_lossy(b).into_owned().into(),
        )),
        Some(v) => crate::coerce::cast_to_integer(v),
    }
}

/// A printf argument as a double, as [`int_arg`] reads integers.
fn real_arg(v: Option<&Value>) -> f64 {
    let numeric = match v {
        None | Some(Value::Null) => return 0.0,
        Some(Value::Integer(i)) => Value::Integer(*i),
        Some(Value::Real(r)) => Value::Real(*r),
        Some(other) => crate::coerce::coerce_text_to_numeric(&as_text(other)),
    };
    match numeric {
        #[allow(
            clippy::cast_precision_loss,
            reason = "a %f/%e/%g argument is a double, as in sqlite3"
        )]
        Value::Integer(i) => i as f64,
        Value::Real(r) => r,
        _ => 0.0,
    }
}

/// `v`'s bits as an unsigned integer (`%u`, `%x`, `%o`).
fn unsigned(v: i64) -> u128 {
    u128::from(u64::from_ne_bytes(v.to_ne_bytes()))
}

/// One `%` conversion: flags, width and precision (`*` read from the
/// arguments), and the conversion character.
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    zero: bool,
    alt: bool,
    comma: bool,
    bang: bool,
    width: usize,
    precision: Option<usize>,
    conversion: char,
}

impl Spec {
    /// Parses the spec after a `%`; `None` when the format ends inside it.
    fn parse(
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        args: &mut std::slice::Iter<'_, Value>,
    ) -> Option<Spec> {
        let mut spec = Spec {
            left: false,
            plus: false,
            space: false,
            zero: false,
            alt: false,
            comma: false,
            bang: false,
            width: 0,
            precision: None,
            conversion: '%',
        };
        loop {
            match *chars.peek()? {
                '-' => spec.left = true,
                '+' => spec.plus = true,
                ' ' => spec.space = true,
                '0' => spec.zero = true,
                '#' => spec.alt = true,
                ',' => spec.comma = true,
                '!' => spec.bang = true,
                _ => break,
            }
            chars.next();
        }
        if chars.peek() == Some(&'*') {
            chars.next();
            let w = star_arg(args.next());
            spec.left |= w < 0;
            // `INT_MIN` has no absolute value: no width.
            spec.width = if w == i32::MIN {
                0
            } else {
                usize::try_from(w.unsigned_abs()).unwrap_or(0)
            };
        } else {
            spec.width = digits(chars);
        }
        if chars.peek() == Some(&'.') {
            chars.next();
            spec.precision = if chars.peek() == Some(&'*') {
                chars.next();
                // A negative precision counts as its absolute value, and
                // `INT_MIN` as none.
                match star_arg(args.next()) {
                    i32::MIN => None,
                    p => usize::try_from(p.unsigned_abs()).ok(),
                }
            } else {
                Some(digits(chars))
            };
        }
        while chars.peek() == Some(&'l') {
            chars.next();
        }
        spec.conversion = chars.next()?;
        Some(spec)
    }

    /// Whether the output is sure to pass [`MAX_LENGTH`]: a width past
    /// it, or a precision past it where precision is a minimum length
    /// (digits, `%c`'s repeat count) rather than a cut or capped.
    fn too_big(&self) -> bool {
        let precision_pads = !matches!(
            self.conversion,
            's' | 'z' | 'q' | 'Q' | 'w' | 'f' | 'e' | 'E' | 'g' | 'G'
        );
        self.width > MAX_LENGTH
            || (precision_pads && self.precision.is_some_and(|p| p > MAX_LENGTH))
    }

    /// At least as many bytes as the conversion of `arg` can produce,
    /// for the conversions whose output can far outgrow the width and
    /// precision [`Self::too_big`] checks -- `%c` of a multibyte
    /// character, `%q` doubling quotes, `%,d` adding commas -- so that
    /// their output is refused before it is built; 0 for the others.
    fn size_bound(&self, arg: Option<&Value>) -> usize {
        let body = match self.conversion {
            'c' => {
                let width = arg
                    .and_then(|v| as_text(v).chars().next())
                    .map_or(1, char::len_utf8);
                self.precision.unwrap_or(1).max(1).saturating_mul(width)
            }
            'q' | 'Q' | 'w' => {
                let text = arg.map(as_text).unwrap_or_default();
                let quote = if self.conversion == 'w' { '"' } else { '\'' };
                let kept = self
                    .precision
                    .map_or(text.len(), |p| p.saturating_mul(4).min(text.len()));
                let quotes = text.matches(quote).count();
                kept.saturating_add(quotes).saturating_add(2)
            }
            'd' | 'i' | 'r' | 'u' | 'o' | 'x' | 'X' | 'p' if self.comma => {
                let digits = self.precision.unwrap_or(0).max(64);
                digits.saturating_add(digits / 3).saturating_add(4)
            }
            _ => return 0,
        };
        body.max(self.width)
    }

    /// Whether the conversion consumes an argument.
    fn takes_arg(&self) -> bool {
        !matches!(self.conversion, '%' | 'n')
    }

    /// The conversion of `arg`, or `None` for a conversion sqlite3 does
    /// not know (which stops the output).
    fn convert(&self, arg: Option<&Value>) -> Option<String> {
        Some(match self.conversion {
            '%' => self.pad("%", 1),
            'n' => String::new(),
            'd' | 'i' => {
                let v = int_arg(arg);
                self.integer(v < 0, u128::from(v.unsigned_abs()), 10, "")
            }
            'r' => {
                let v = int_arg(arg);
                let suffix = ordinal_suffix(v.unsigned_abs());
                self.integer(v < 0, u128::from(v.unsigned_abs()), 10, suffix)
            }
            'u' => self.integer(false, unsigned(int_arg(arg)), 10, ""),
            'o' => self.integer(false, unsigned(int_arg(arg)), 8, ""),
            'x' => self.integer(false, unsigned(int_arg(arg)), 16, ""),
            'X' | 'p' => self
                .integer(false, unsigned(int_arg(arg)), 16, "")
                .to_ascii_uppercase(),
            'f' | 'e' | 'E' | 'g' | 'G' => self.real(real_arg(arg)),
            'c' => {
                // The first character, repeated `precision` times; NULL
                // or empty is a NUL character.
                let c = arg
                    .filter(|v| !matches!(v, Value::Null))
                    .and_then(|v| as_text(v).chars().next())
                    .unwrap_or('\0');
                let n = self.precision.unwrap_or(1).max(1);
                let body: String = std::iter::repeat_n(c, n).collect();
                let len = body.len();
                self.pad(&body, len)
            }
            's' | 'z' => {
                let s = arg.map(as_text).unwrap_or_default();
                let (s, len) = self.cut(&s);
                self.pad(&s, len)
            }
            'q' | 'Q' | 'w' => {
                let quote = if self.conversion == 'w' { '"' } else { '\'' };
                let (s, len) = match arg {
                    None | Some(Value::Null) => self.cut(if self.conversion == 'Q' {
                        "NULL"
                    } else {
                        "(NULL)"
                    }),
                    Some(v) => {
                        // Width counts the cut bytes, not the U+FFFD that
                        // stands in for a dangling one.
                        let (s, cut_len) = self.cut(&as_text(v));
                        let stand_in = s.len().saturating_sub(cut_len);
                        let escaped = s.replace(quote, &format!("{quote}{quote}"));
                        let s = if self.conversion == 'Q' {
                            format!("'{escaped}'")
                        } else {
                            escaped
                        };
                        let len = s.len().saturating_sub(stand_in);
                        (s, len)
                    }
                };
                self.pad(&s, len)
            }
            _ => return None,
        })
    }

    /// `body` (`len` bytes wide, or characters with `!`) padded to the
    /// width with spaces, on the right with `-`.
    fn pad(&self, body: &str, len: usize) -> String {
        let len = if self.bang { body.chars().count() } else { len };
        let fill = " ".repeat(self.width.saturating_sub(len));
        if self.left {
            format!("{body}{fill}")
        } else {
            format!("{fill}{body}")
        }
    }

    /// `s` cut to the precision, and the width it counts for: bytes (a
    /// character cut in half reads back as U+FFFD, as sqlite3's dangling
    /// bytes do), or characters with `!`.
    fn cut(&self, s: &str) -> (String, usize) {
        let Some(p) = self.precision else {
            return (s.to_string(), s.len());
        };
        if self.bang {
            let cut: String = s.chars().take(p).collect();
            let len = cut.len();
            return (cut, len);
        }
        if p >= s.len() {
            return (s.to_string(), s.len());
        }
        let mut end = p;
        while !s.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let mut cut = s.get(..end).unwrap_or_default().to_string();
        if end < p {
            cut.push('\u{FFFD}');
        }
        (cut, p)
    }

    /// An integer conversion of `magnitude` in `radix`, as sqlite3's
    /// `etRADIX`: precision is the minimum digit count, and zero padding
    /// raises it to the width less the sign (even with `-`). `,` groups
    /// the padded digits; the `#` prefix (`0x`, `0`) and an ordinal
    /// `suffix` come on top -- the suffix counts toward both the width and
    /// the precision.
    fn integer(&self, negative: bool, magnitude: u128, radix: u32, suffix: &str) -> String {
        let signed = radix == 10 && self.conversion != 'u';
        let sign = if negative {
            "-"
        } else if self.plus && signed {
            "+"
        } else if self.space && signed {
            " "
        } else {
            ""
        };
        let mut precision = self.precision.unwrap_or(0);
        if self.zero {
            precision = precision.max(self.width.saturating_sub(sign.len()));
        }
        precision = precision.saturating_sub(suffix.len());
        let mut digits = to_radix(magnitude, radix);
        if digits.len() < precision {
            digits = format!(
                "{}{digits}",
                "0".repeat(precision.saturating_sub(digits.len()))
            );
        }
        if self.comma && radix == 10 {
            digits = group_thousands(&digits);
        }
        let prefix = match (self.alt, radix) {
            (true, 16) if magnitude != 0 => "0x",
            (true, 8) if magnitude != 0 => "0",
            _ => "",
        };
        let body = format!("{sign}{prefix}{digits}{suffix}");
        let len = body.len();
        self.pad(&body, len)
    }

    /// A `%f`/`%e`/`%g` conversion. The value is rounded once, half away
    /// from zero, to the digits the conversion shows but at most 16
    /// significant digits (18 with `!`), and zero-filled past that, as
    /// sqlite3's `sqlite3FpDecode` does. `!` also drops trailing zeros
    /// but keeps one decimal (`1.0`, `2.5e+00`).
    fn real(&self, x: f64) -> String {
        let negative = x.is_sign_negative() && x != 0.0 && !x.is_nan();
        let (exact, exp) = decode(x);
        let precision = self.precision.unwrap_or(6).min(FP_PRECISION_LIMIT);
        let p = i64::try_from(precision).unwrap_or(i64::MAX);
        let shown = match self.conversion {
            'f' => exp.saturating_add(1).saturating_add(p),
            'e' | 'E' => p.saturating_add(1),
            _ => p.max(1),
        };
        let (digits, exp) = round(&exact, exp, shown.min(if self.bang { 18 } else { 16 }));
        let mut body = match self.conversion {
            'f' => self.fixed(&digits, exp, precision),
            'e' | 'E' => self.scientific(&digits, exp, precision),
            _ => self.general(&digits, exp, precision),
        };
        if self.bang {
            body = alternate_form_2(&body);
        }
        if matches!(self.conversion, 'E' | 'G') {
            body = body.to_ascii_uppercase();
        }
        let sign = if negative {
            "-"
        } else if self.plus {
            "+"
        } else if self.space {
            " "
        } else {
            ""
        };
        let len = sign.len().saturating_add(body.len());
        let fill = self.width.saturating_sub(len);
        if self.left {
            format!("{sign}{body}{}", " ".repeat(fill))
        } else if self.zero {
            format!("{sign}{}{body}", "0".repeat(fill))
        } else {
            format!("{}{sign}{body}", " ".repeat(fill))
        }
    }

    /// `%f` with `precision` decimals.
    fn fixed(&self, digits: &[u8], exp: i64, precision: usize) -> String {
        let keep = exp
            .saturating_add(1)
            .saturating_add(i64::try_from(precision).unwrap_or(i64::MAX));
        let (digits, exp) = round(digits, exp, keep);
        // Digit `i` has weight 10^(exp - i).
        let at = |i: i64| -> char {
            usize::try_from(i)
                .ok()
                .and_then(|i| digits.get(i))
                .map_or('0', |d| char::from(b'0'.saturating_add(*d)))
        };
        let mut int_part = String::new();
        if exp < 0 {
            int_part.push('0');
        } else {
            for i in 0..=exp {
                int_part.push(at(i));
            }
        }
        let mut frac = String::new();
        for k in 1..=i64::try_from(precision).unwrap_or(0) {
            frac.push(at(exp.saturating_add(k)));
        }
        if self.comma {
            int_part = group_thousands(&int_part);
        }
        if frac.is_empty() {
            if self.alt {
                format!("{int_part}.")
            } else {
                int_part
            }
        } else {
            format!("{int_part}.{frac}")
        }
    }

    /// `%e` with `precision` mantissa decimals.
    fn scientific(&self, digits: &[u8], exp: i64, precision: usize) -> String {
        let keep = i64::try_from(precision)
            .unwrap_or(i64::MAX)
            .saturating_add(1);
        let (digits, exp) = round(digits, exp, keep);
        let mut mantissa: String = digits
            .iter()
            .take(precision.saturating_add(1))
            .map(|d| char::from(b'0'.saturating_add(*d)))
            .collect();
        while mantissa.len() < precision.saturating_add(1) {
            mantissa.push('0');
        }
        let (head, tail) = mantissa.split_at(1);
        let dot = if tail.is_empty() && !self.alt {
            ""
        } else {
            "."
        };
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{head}{dot}{tail}e{sign}{:02}", exp.unsigned_abs())
    }

    /// `%g`: `%e` when the exponent is below -4 or at least the
    /// precision (0 counts as 1), else `%f`; trailing zeros dropped
    /// unless `#`.
    fn general(&self, digits: &[u8], exp: i64, precision: usize) -> String {
        let p = precision.max(1);
        let p_i = i64::try_from(p).unwrap_or(i64::MAX);
        let (rounded, rexp) = round(digits, exp, p_i);
        let s = if rexp < -4 || rexp >= p_i {
            self.scientific(&rounded, rexp, p.saturating_sub(1))
        } else {
            let decimals = p_i.saturating_sub(1).saturating_sub(rexp);
            self.fixed(&rounded, rexp, usize::try_from(decimals).unwrap_or(0))
        };
        if self.alt {
            return s;
        }
        let (mantissa, exponent) = split_exponent(&s);
        let mut mantissa = mantissa.to_string();
        if mantissa.contains('.') {
            while mantissa.ends_with('0') {
                mantissa.pop();
            }
            if mantissa.ends_with('.') {
                mantissa.pop();
            }
        }
        format!("{mantissa}{exponent}")
    }
}

/// `s` split before its exponent (`e`), if any.
fn split_exponent(s: &str) -> (&str, &str) {
    match s.find('e') {
        Some(i) => s.split_at(i),
        None => (s, ""),
    }
}

/// sqlite3's alternate form 2 (the `!` flag): trailing zeros dropped,
/// but at least one decimal kept.
fn alternate_form_2(s: &str) -> String {
    let (mantissa, exponent) = split_exponent(s);
    let mut mantissa = mantissa.to_string();
    if mantissa.contains('.') {
        while mantissa.ends_with('0') {
            mantissa.pop();
        }
        if mantissa.ends_with('.') {
            mantissa.push('0');
        }
    } else {
        mantissa.push_str(".0");
    }
    format!("{mantissa}{exponent}")
}

/// A width or precision written as digits: sqlite3 accumulates it in an
/// unsigned 32-bit int and keeps the low 31 bits (`%5000000000s` is
/// 705032704 wide).
fn digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> usize {
    let mut n = 0_u32;
    while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
        chars.next();
        n = n.wrapping_mul(10).wrapping_add(d);
    }
    usize::try_from(n & 0x7fff_ffff).unwrap_or(0)
}

/// A `*` width or precision: sqlite3 reads the argument as a C `int`.
fn star_arg(v: Option<&Value>) -> i32 {
    let [a, b, c, d, ..] = int_arg(v).to_le_bytes();
    i32::from_le_bytes([a, b, c, d])
}

/// `SQLITE_FP_PRECISION_LIMIT`: a `%f`/`%e`/`%g` precision past it counts
/// as it.
const FP_PRECISION_LIMIT: usize = 100_000_000;

/// `n` in `radix`, lowercase.
fn to_radix(mut n: u128, radix: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    let r = u128::from(radix);
    while n > 0 {
        let d = u32::try_from(n.checked_rem(r).unwrap_or(0)).unwrap_or(0);
        out.push(char::from_digit(d, radix).unwrap_or('0'));
        n = n.checked_div(r).unwrap_or(0);
    }
    out.iter().rev().collect()
}

/// `digits` with a `,` between each group of three from the right.
fn group_thousands(digits: &str) -> String {
    let n = digits.len();
    let mut out = String::with_capacity(n.saturating_add(n / 3));
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (n.saturating_sub(i)) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `|x|`'s exact decimal digits `d0 d1 ...` (no leading zero, 80
/// significant -- far past where any conversion rounds) and the exponent
/// of `d0`. Infinity reads as 1e301 and NaN as 0, as sqlite3's printf
/// shows them.
fn decode(x: f64) -> (Vec<u8>, i64) {
    let ax = if x.is_nan() {
        0.0
    } else if x.is_infinite() {
        1e301
    } else {
        x.abs()
    };
    if ax == 0.0 {
        return (vec![0], 0);
    }
    let s = format!("{ax:.79e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((s.as_str(), "0"));
    let digits = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b.saturating_sub(b'0'))
        .collect();
    (digits, exp.parse().unwrap_or(0))
}

/// `digits` (weight of the first `10^exp`) rounded half away from zero to
/// the first `keep` digits; returns the kept digits and their exponent,
/// which a carry raises by one. Digits past the input are zeros, so
/// rounding already-rounded digits again changes nothing.
fn round(digits: &[u8], exp: i64, keep: i64) -> (Vec<u8>, i64) {
    let Ok(keep) = usize::try_from(keep) else {
        return (vec![0], exp);
    };
    let mut kept: Vec<u8> = digits.iter().take(keep).copied().collect();
    let mut exp = exp;
    if digits.get(keep).is_some_and(|d| *d >= 5) {
        let mut i = kept.len();
        loop {
            if i == 0 {
                kept.insert(0, 1);
                exp = exp.saturating_add(1);
                kept.truncate(keep.max(1));
                break;
            }
            i = i.saturating_sub(1);
            match kept.get_mut(i) {
                Some(d) if *d == 9 => *d = 0,
                Some(d) => {
                    *d = d.saturating_add(1);
                    break;
                }
                None => break,
            }
        }
    }
    if kept.is_empty() {
        return (vec![0], exp);
    }
    (kept, exp)
}

#[cfg(test)]
mod tests {
    use super::super::call;
    use crate::value::Value;

    fn t(s: &str) -> Value {
        Value::Text(s.to_string().into())
    }

    fn p(args: &[Value]) -> Value {
        call("printf", args).unwrap()
    }

    /// #651: past `MAX_LENGTH` is "string or blob too big", decided
    /// before the output is built; widths and precisions wrap to 31 bits.
    #[test]
    fn printf_and_randomblob_past_max_length_are_too_big() {
        use super::super::FunctionError;
        let i = Value::Integer;
        for args in [
            vec![t("%1000000001d"), i(1)],
            vec![t("%.1000000001x"), i(1)],
            vec![t("%*d"), i(1_000_000_001), i(1)],
            vec![t("%.*c"), i(1_000_000_001), t("x")],
        ] {
            assert_eq!(
                call("printf", &args),
                Err(FunctionError::TooBig),
                "{args:?}"
            );
        }
        assert_eq!(
            call("randomblob", &[i(1_000_000_001)]),
            Err(FunctionError::TooBig)
        );
        // Refused before building: multibyte `%c`, `%,d` grouping.
        assert_eq!(
            call("printf", &[t("%.600000000c"), t("é")]),
            Err(FunctionError::TooBig)
        );
        assert_eq!(
            call("printf", &[t("%,.900000000d"), i(1)]),
            Err(FunctionError::TooBig)
        );
        assert_eq!(
            p(&[t("%,d|%Q|%.3c|"), i(1234567), t("it's"), t("é")]),
            t("1,234,567|'it''s'|ééé|")
        );
        assert_eq!(p(&[t("%4294967298d|"), i(1)]), t(" 1|"));
        assert_eq!(p(&[t("%.5000000000s|"), t("abc")]), t("abc|"));
        assert_eq!(p(&[t("%*d|"), i(-2_147_483_648), i(1)]), t("1|"));
        assert_eq!(p(&[t("%.*d|"), i(-3), i(1)]), t("001|"));
    }

    /// Expectations from sqlite3 3.53.4.
    #[test]
    fn printf_matches_sqlite3() {
        let i = Value::Integer;
        let r = Value::Real;
        for (args, want) in [
            (
                vec![t("%d|%i|%u"), i(42), i(-7), i(-1)],
                "42|-7|18446744073709551615",
            ),
            (
                vec![
                    t("%5d|%-5d|%05d|%+d|% d"),
                    i(42),
                    i(42),
                    i(42),
                    i(42),
                    i(42),
                ],
                "   42|42   |00042|+42| 42",
            ),
            (vec![t("%d"), t("12abc")], "12"),
            (
                vec![
                    t("%f|%.2f|%10.3f|%-10.1f|"),
                    r(3.14158),
                    r(2.5),
                    r(1.0),
                    r(-1.25),
                ],
                "3.141580|2.50|     1.000|-1.3      |",
            ),
            (
                vec![t("%e|%E|%.3e"), r(12345.678), r(0.00012), r(1.5)],
                "1.234568e+04|1.200000E-04|1.500e+00",
            ),
            (
                vec![t("%g|%G|%g|%g"), i(100_000), r(1e-5), r(1e20), r(0.0001)],
                "100000|1E-05|1e+20|0.0001",
            ),
            (
                vec![
                    t("%x|%X|%o|%#x|%#o|%#x"),
                    i(255),
                    i(255),
                    i(8),
                    i(255),
                    i(8),
                    i(0),
                ],
                "ff|FF|10|0xff|010|0",
            ),
            (
                vec![t("%x|%c|%c"), i(-1), t("hello"), i(65)],
                "ffffffffffffffff|h|6",
            ),
            (
                vec![
                    t("%s|%10s|%-10s|%.2s"),
                    t("abc"),
                    t("abc"),
                    t("abc"),
                    t("abcdef"),
                ],
                "abc|       abc|abc       |ab",
            ),
            (
                vec![
                    t("%q|%Q|%Q|%w"),
                    t("it's"),
                    t("it's"),
                    Value::Null,
                    t("a\"b"),
                ],
                "it''s|'it''s'|NULL|a\"\"b",
            ),
            (
                vec![t("%,d|%,d|%,.2f"), i(1_234_567), i(-1234), r(1_234_567.891)],
                "1,234,567|-1,234|1,234,567.89",
            ),
            (
                vec![t("%*d|%-*d|%.*f"), i(5), i(1), i(5), i(2), i(2), r(3.14158)],
                "    1|2    |3.14",
            ),
            (vec![t("%!.3g|%!g"), r(2.0), r(1e30)], "2.0|1.0e+30"),
            (
                vec![t("%.20f|%.0f|%.0f"), r(0.1), r(2.5), r(3.5)],
                "0.10000000000000000000|3|4",
            ),
            (
                vec![t("%.2e|%.0e|%#.0e|%#.0f"), r(9.995), i(5), i(5), i(5)],
                "9.99e+00|5e+00|5.e+00|5.",
            ),
            (
                vec![
                    t("%.3d|%+.3e|%08.3f|%-08d|"),
                    i(7),
                    r(1.5),
                    r(-3.14158),
                    i(42),
                ],
                "007|+1.500e+00|-003.142|00000042|",
            ),
            (
                vec![
                    t("%5s|%.2s|%!5s|%!.2s|"),
                    t("éé"),
                    t("éé"),
                    t("éé"),
                    t("éé"),
                ],
                " éé|é|   éé|éé|",
            ),
            (
                vec![t("%r %r %r %r"), i(1), i(2), i(23), i(11)],
                "1st 2nd 23rd 11th",
            ),
            (vec![t("%5%|%-5%|")], "    %|%    |"),
            (
                vec![t("%.0g|%g|%#g"), r(0.5), r(0.0), r(0.0)],
                "0.5|0|0.00000",
            ),
            (
                vec![t("%Q|%q|%s|%d|%f|"), Value::Null],
                "NULL|(NULL)||0|0.000000|",
            ),
            (vec![t("ab%yc")], "ab"),
            (vec![t("%")], "%"),
        ] {
            assert_eq!(p(&args), t(want), "{args:?}");
        }
        assert_eq!(p(&[t("%y")]), Value::Null);
        assert_eq!(p(&[Value::Null]), Value::Null);
        assert_eq!(call("format", &[t("%s %s"), i(1)]).unwrap(), t("1 "));
    }

    #[test]
    fn text_functions_match_sqlite3() {
        let i = Value::Integer;
        for (name, args, want) in [
            ("char", vec![i(72), i(105)], t("Hi")),
            ("char", vec![i(0x1F600)], t("😀")),
            ("char", vec![i(-1)], t("\u{FFFD}")),
            ("char", vec![t("66")], t("B")),
            (
                "concat",
                vec![
                    i(1),
                    Value::Real(2.5),
                    Value::Blob(b"A".to_vec().into()),
                    Value::Null,
                    t("z"),
                ],
                t("12.5Az"),
            ),
            (
                "concat_ws",
                vec![t(","), t("a"), Value::Null, t("b"), t("")],
                t("a,b,"),
            ),
            ("concat_ws", vec![Value::Null, t("a")], Value::Null),
            ("octet_length", vec![t("é")], i(2)),
            ("octet_length", vec![Value::Real(1.5)], i(3)),
            ("unicode", vec![i(65)], i(54)),
            ("unicode", vec![t("")], Value::Null),
            ("unistr", vec![t("a\\u0041\\\\b")], t("aA\\b")),
            ("unistr", vec![t("\\+01F600x\\0041")], t("😀xA")),
            ("substring", vec![t("hello"), i(-2)], t("lo")),
            ("likely", vec![t("x")], t("x")),
            ("likelihood", vec![i(5), Value::Real(0.25)], i(5)),
        ] {
            assert_eq!(call(name, &args).unwrap(), want, "{name}({args:?})");
        }
        assert!(call("unistr", &[t("\\x")]).is_err());
        assert!(call("likelihood", &[i(1), i(2)]).is_err());
        let blob = call("randomblob", &[i(-5)]).unwrap();
        assert!(matches!(blob, Value::Blob(b) if b.len() == 1));
        assert!(matches!(call("random", &[]).unwrap(), Value::Integer(_)));
        assert!(call("changes", &[]).is_err());
    }
}
