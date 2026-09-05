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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collation {
    /// Byte-for-byte comparison. SQLite's default.
    Binary,
    /// ASCII-only case folding -- NOT Unicode. `ß`/`SS` and `é`/`É` never
    /// compare equal.
    NoCase,
    /// BINARY comparison after stripping trailing spaces from both
    /// operands (not from storage).
    RTrim,
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

/// Renders a REAL the way `sqlite3`'s `-list`/`-csv` modes do: 15
/// significant digits (`%.15g`-equivalent), switching to scientific
/// notation when the decimal exponent is `< -4` or `>= 15`, and always
/// keeping an explicit decimal point or exponent -- a REAL never prints
/// as a bare integer, so `1.0` never becomes `1`. Ported from
/// sqlite-rs's `format::format_real`.
pub fn format_real(x: f64) -> String {
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        };
    }
    if x.is_nan() {
        return "NULL".to_string();
    }

    let neg = x.is_sign_negative();
    let ax = x.abs();
    if ax.is_infinite() {
        let mag = "9.0e+999";
        return if neg {
            format!("-{mag}")
        } else {
            mag.to_string()
        };
    }

    let sci = format!("{:.14e}", ax);
    let (mantissa, exp_str) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp_str.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();

    let body = if !(-4..15).contains(&exp) {
        let mantissa_trimmed = trim_trailing_zeros(&digits[1..]);
        let mantissa_part = if mantissa_trimmed.is_empty() {
            format!("{}.0", &digits[..1])
        } else {
            format!("{}.{}", &digits[..1], mantissa_trimmed)
        };
        let exp_sign = if exp >= 0 { "+" } else { "-" };
        format!("{mantissa_part}e{exp_sign}{:02}", exp.abs())
    } else if exp >= 0 {
        let split = (exp as usize).saturating_add(1);
        let int_part = &digits[..split];
        let frac_part = trim_trailing_zeros(&digits[split..]);
        if frac_part.is_empty() {
            format!("{int_part}.0")
        } else {
            format!("{int_part}.{frac_part}")
        }
    } else {
        let leading_zeros = "0".repeat((exp.unsigned_abs() as usize).saturating_sub(1));
        let frac = trim_trailing_zeros(&digits);
        format!("0.{leading_zeros}{frac}")
    };

    if neg {
        format!("-{body}")
    } else {
        body
    }
}

fn trim_trailing_zeros(s: &str) -> &str {
    s.trim_end_matches('0')
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
    fn real_matches_oracle_thresholds() {
        #[allow(clippy::approx_constant)]
        let three_point_one_four = 3.14;
        assert_eq!(format_real(three_point_one_four), "3.14");
        assert_eq!(format_real(1.0), "1.0");
        assert_eq!(format_real(2.5e300), "2.5e+300");
        assert_eq!(format_real(0.0001), "0.0001");
        assert_eq!(format_real(100000000000000.0), "100000000000000.0");
        assert_eq!(format_real(1e15), "1.0e+15");
        assert_eq!(format_real(999999999999999.0), "999999999999999.0");
        assert_eq!(format_real(0.00001), "1.0e-05");
        assert_eq!(format_real(-2.5), "-2.5");
        assert_eq!(format_real(123.456), "123.456");
        assert_eq!(format_real(0.0), "0.0");
        assert_eq!(format_real(-0.0), "-0.0");
    }
}
