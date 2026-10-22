//! The SQL value model shared by every row-oriented layer of t-rust-db
//! (ADR 0010): `db-storage`'s `row::record` decodes on-disk records into
//! [`Value`]s, `vm::row` executes over them, and a consumer's cursor
//! adapter hands them across without conversion. One definition, no
//! feature gate, no dependencies — sqlite-rs's `record::{value,
//! collation}.rs` and `format::format_real`, ported verbatim.
//!
//! Deliberately not the same type as [`crate::vm::batch::Value`]: the two
//! `Opcode` sets have separate value models (ADR 0001, ADR 0007), and
//! `batch::Value`'s `Cow<'static, str>` shape exists for AOT-emitted
//! `const` literals, a concern the row model doesn't have.

use std::cmp::Ordering;
use std::rc::Rc;

/// A single decoded column value, per SQLite's dynamic type system.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// A signed integer, stored as 1/2/3/4/6/8 bytes on disk per the serial type.
    Integer(i64),
    /// An 8-byte IEEE 754 floating-point value.
    Real(f64),
    /// A text value, decoded according to the database's `TextEncoding`.
    Text(Rc<str>),
    /// An uninterpreted byte sequence.
    Blob(Rc<[u8]>),
}

/// The database's text encoding, from database header byte 56.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// UTF-8.
    Utf8,
    /// UTF-16 little-endian.
    Utf16Le,
    /// UTF-16 big-endian.
    Utf16Be,
}

/// A text collating function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Collation {
    /// Byte-for-byte comparison. SQLite's default.
    #[default]
    Binary,
    /// ASCII-only case folding -- NOT Unicode. `ß`/`SS` and `é`/`É` never
    /// compare equal.
    NoCase,
    /// BINARY comparison after stripping trailing spaces from both
    /// operands (not from storage).
    RTrim,
}

/// A length or count as a SQL `INTEGER`, saturating at `i64::MAX`
/// rather than wrapping negative (db-core#225 -- replaces the bare
/// `len as i64` the `cast_possible_wrap` lint now rejects). Unreachable
/// on any real allocation, but total. Lives here rather than in
/// `vm::batch` because `functions` (always compiled) needs it too, and
/// `value` is the one module every executor depends on (ADR 0010).
/// Saturating rather than erroring matches SQLite's own 64-bit `length()`.
#[inline]
pub(crate) fn len_to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Compares two strings under the given collation.
#[inline]
pub fn compare_text(a: &str, b: &str, collation: Collation) -> Ordering {
    match collation {
        Collation::Binary => a.as_bytes().cmp(b.as_bytes()),
        Collation::NoCase => a
            .as_bytes()
            .iter()
            .map(u8::to_ascii_lowercase)
            .cmp(b.as_bytes().iter().map(u8::to_ascii_lowercase)),
        Collation::RTrim => {
            let a = a.trim_end_matches(' ');
            let b = b.trim_end_matches(' ');
            a.as_bytes().cmp(b.as_bytes())
        }
    }
}

/// A decimal (`unscaled / 10^scale`) at `scale` digits after the point,
/// as DuckDB prints one: `123.45`, `-0.05`, `42` (scale 0) (#632).
pub fn format_decimal(unscaled: i128, scale: u8) -> String {
    let digits = unscaled.unsigned_abs().to_string();
    let sign = if unscaled < 0 { "-" } else { "" };
    let scale = usize::from(scale);
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale.saturating_add(1));
    let (int, frac) = padded.split_at(padded.len().saturating_sub(scale));
    format!("{sign}{int}.{frac}")
}

/// Renders a REAL the way sqlite3 3.53 turns one into text (`CAST`, `||`,
/// the shell's `-list`/`-csv` modes): `%!.17g` through `sqlite3FpDecode`,
/// which approximates past about 16 digits (`1.0/3` is
/// `0.33333333333333332`), switching to scientific notation when the
/// decimal exponent is `< -4` or `> 16`, and always keeping an explicit
/// decimal point or exponent -- a REAL never prints as a bare integer,
/// so `1.0` never becomes `1`. Infinity is `Inf`, negative zero `0.0`.
pub fn format_real(x: f64) -> String {
    if x.is_nan() {
        return "NULL".to_string();
    }
    String::from_utf8_lossy(&crate::functions::fp::format_real17(x, false)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_is_case_sensitive() {
        assert_ne!(
            compare_text("abc", "ABC", Collation::Binary),
            Ordering::Equal
        );
    }

    #[test]
    fn nocase_folds_ascii_only() {
        assert_eq!(compare_text("I", "i", Collation::NoCase), Ordering::Equal);
        assert_ne!(
            compare_text("straße", "STRASSE", Collation::NoCase),
            Ordering::Equal
        );
        assert_ne!(compare_text("é", "É", Collation::NoCase), Ordering::Equal);
    }

    #[test]
    fn rtrim_ignores_only_trailing_spaces() {
        assert_eq!(
            compare_text("abc ", "abc", Collation::RTrim),
            Ordering::Equal
        );
        assert_eq!(
            compare_text("abc", "abc  ", Collation::RTrim),
            Ordering::Equal
        );
        assert_ne!(
            compare_text(" abc", "abc", Collation::RTrim),
            Ordering::Equal
        );
    }

    #[test]
    fn decimal_prints_at_its_scale() {
        assert_eq!(format_decimal(12345, 2), "123.45");
        assert_eq!(format_decimal(-5, 2), "-0.05");
        assert_eq!(format_decimal(150, 2), "1.50");
        assert_eq!(format_decimal(42, 0), "42");
        assert_eq!(format_decimal(0, 3), "0.000");
        assert_eq!(format_decimal(i128::MIN, 0), i128::MIN.to_string());
    }

    #[test]
    fn real_matches_oracle_thresholds() {
        #[allow(clippy::approx_constant)]
        let three_point_one_four = 3.14;
        assert_eq!(format_real(three_point_one_four), "3.14");
        assert_eq!(format_real(1.0), "1.0");
        assert_eq!(format_real(2.5e300), "2.5e+300");
        assert_eq!(format_real(0.0001), "0.0001");
        assert_eq!(format_real(100000000000000.0), "100000000000000.0");
        assert_eq!(format_real(1e15), "1000000000000000.0");
        assert_eq!(format_real(1e16), "10000000000000000.0");
        assert_eq!(format_real(1e17), "1.0e+17");
        assert_eq!(format_real(1.0 / 3.0), "0.33333333333333332");
        assert_eq!(format_real(123456789012345678.0), "1.2345678901234568e+17");
        assert_eq!(format_real(0.000123456), "0.000123456");
        assert_eq!(format_real(f64::INFINITY), "Inf");
        assert_eq!(format_real(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_real(999999999999999.0), "999999999999999.0");
        assert_eq!(format_real(0.00001), "1.0e-05");
        assert_eq!(format_real(-2.5), "-2.5");
        assert_eq!(format_real(123.456), "123.456");
        assert_eq!(format_real(0.0), "0.0");
        assert_eq!(format_real(-0.0), "0.0");
    }
}
