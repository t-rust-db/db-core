//! Type affinity: the five-way storage preference steered by a column's
//! declared type (<https://www.sqlite.org/datatype3.html> §3.1). Ported
//! from sqlite-rs's `vdbe::affinity` (ADR 0008).

use super::value::{format_real, Value};

/// A column's storage-class preference, derived from its declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// Values are stored/compared as text; only well-formed numeric text
    /// is coerced when compared against a numeric value.
    Text,
    /// Non-integer numeric preference: coerces well-formed numeric text
    /// but never forces integers into floating point.
    Numeric,
    /// Numeric preference favoring integral storage.
    Integer,
    /// Numeric preference that additionally forces integer values into
    /// floating point.
    Real,
    /// No coercion is applied; values are stored/compared as given.
    Blob,
}

/// Derives affinity from a declared type string per the datatype3.html
/// substring rules, checked in order: INT, then CHAR/CLOB/TEXT, then
/// BLOB or no declared type, then REAL/FLOA/DOUB, else NUMERIC.
pub fn affinity_of(declared_type: &str) -> Affinity {
    let upper = declared_type.to_ascii_uppercase();
    if upper.contains("INT") {
        Affinity::Integer
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        Affinity::Text
    } else if upper.contains("BLOB") || upper.is_empty() {
        Affinity::Blob
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

impl Affinity {
    /// SQLite's own affinity byte codes (`expr.h`'s `SQLITE_AFF_*`,
    /// e.g. `'D'` for INTEGER) -- the P4 wire format for compare opcodes.
    pub fn to_p4_byte(self) -> u8 {
        match self {
            Affinity::Blob => b'A',
            Affinity::Text => b'B',
            Affinity::Numeric => b'C',
            Affinity::Integer => b'D',
            Affinity::Real => b'E',
        }
    }

    /// Inverse of [`Affinity::to_p4_byte`]; an unrecognized byte
    /// (should not occur for programs this codegen emits) defaults to
    /// `Blob`, matching a `None` P4's no-op behavior.
    pub fn from_p4_byte(byte: u8) -> Affinity {
        match byte {
            b'B' => Affinity::Text,
            b'C' => Affinity::Numeric,
            b'D' => Affinity::Integer,
            b'E' => Affinity::Real,
            _ => Affinity::Blob,
        }
    }

    /// NUMERIC/INTEGER/REAL are SQLite's "numeric affinities" --
    /// `sqlite3IsNumericAffinity`. TEXT/BLOB are not.
    pub fn is_numeric(self) -> bool {
        matches!(self, Affinity::Numeric | Affinity::Integer | Affinity::Real)
    }
}

/// The affinity applied to a comparison's two operands, derived from
/// each operand's own affinity per SQLite's `comparisonAffinity`
/// (`expr.c`): if both operands have an affinity, numeric wins when
/// either is numeric, else no affinity (BLOB) is applied; if only one
/// operand has an affinity, that one wins; if neither does, no affinity
/// is applied.
pub fn comparison_affinity(lhs: Option<Affinity>, rhs: Option<Affinity>) -> Affinity {
    match (lhs, rhs) {
        (Some(a), Some(b)) => {
            if a.is_numeric() || b.is_numeric() {
                Affinity::Numeric
            } else {
                Affinity::Blob
            }
        }
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => Affinity::Blob,
    }
}

/// Converts a well-formed numeric-text literal to its lossless numeric
/// representation for NUMERIC/INTEGER/REAL affinities. TEXT and BLOB
/// affinities never convert.
///
/// REAL affinity additionally forces an `Integer` value into floating
/// point (datatype3.html §2.1: "a column with REAL affinity works like a
/// column with NUMERIC affinity except that it forces integer values
/// into floating point representation").
pub fn apply_affinity(value: &mut Value, affinity: Affinity) {
    if affinity == Affinity::Blob {
        return;
    }
    if affinity == Affinity::Text {
        // TEXT affinity converts NUMERIC-storage-class values to their
        // text rendering -- the opposite direction of every other
        // affinity's text->numeric coercion below, and it never touches
        // BLOB (that's the shared early return above) or an
        // already-Text value.
        match value {
            Value::Integer(i) => *value = Value::Text(i.to_string().into()),
            Value::Real(r) => *value = Value::Text(format_real(*r).into()),
            Value::Text(_) | Value::Blob(_) | Value::Null => {}
        }
        return;
    }
    if let Value::Text(s) = value {
        if let Some(numeric) = parse_well_formed_number(s) {
            *value = numeric;
        }
    }
    if affinity == Affinity::Real {
        if let Value::Integer(i) = *value {
            #[allow(clippy::cast_precision_loss)]
            let real = i as f64;
            *value = Value::Real(real);
        }
    }
}

/// Parses `s` as a number only if the *entire* trimmed string is a valid
/// numeric literal (unlike coercion's longest-prefix rule).
fn parse_well_formed_number(s: &str) -> Option<Value> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        return Some(Value::Integer(i));
    }
    if let Ok(r) = trimmed.parse::<f64>() {
        if r.is_finite() {
            return Some(Value::Real(r));
        }
    }
    None
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
    fn affinity_of_matches_oracle_declared_types() {
        let cases = [
            ("INTEGER", Affinity::Integer),
            ("INT", Affinity::Integer),
            ("TINYINT", Affinity::Integer),
            ("UNSIGNED BIG INT", Affinity::Integer),
            ("INT2", Affinity::Integer),
            ("TEXT", Affinity::Text),
            ("CHARACTER(20)", Affinity::Text),
            ("VARCHAR(255)", Affinity::Text),
            ("VARYING CHARACTER(255)", Affinity::Text),
            ("NCHAR(55)", Affinity::Text),
            ("NATIVE CHARACTER(70)", Affinity::Text),
            ("NVARCHAR(100)", Affinity::Text),
            ("CLOB", Affinity::Text),
            ("BLOB", Affinity::Blob),
            ("", Affinity::Blob),
            ("REAL", Affinity::Real),
            ("DOUBLE", Affinity::Real),
            ("DOUBLE PRECISION", Affinity::Real),
            ("FLOAT", Affinity::Real),
            ("NUMERIC", Affinity::Numeric),
            ("DECIMAL(10,5)", Affinity::Numeric),
            ("BOOLEAN", Affinity::Numeric),
            ("DATE", Affinity::Numeric),
            ("DATETIME", Affinity::Numeric),
            // "POINT" contains the substring "INT" -- a documented SQLite
            // divergence trap: it gets INTEGER affinity, not NUMERIC.
            ("POINT", Affinity::Integer),
            ("STRING", Affinity::Numeric),
        ];
        for (declared, expected) in cases {
            assert_eq!(
                affinity_of(declared),
                expected,
                "affinity_of({declared:?}) mismatch"
            );
        }
    }

    #[test]
    fn text_affinity_never_converts_numeric_text() {
        let mut value = Value::Text("1.5".to_string().into());
        apply_affinity(&mut value, Affinity::Text);
        assert!(matches!(value, Value::Text(_)));
    }

    #[test]
    fn every_non_text_non_blob_affinity_converts_numeric_text() {
        for affinity in [Affinity::Numeric, Affinity::Integer, Affinity::Real] {
            let mut value = Value::Text("1.5".to_string().into());
            apply_affinity(&mut value, affinity);
            assert!(!matches!(value, Value::Text(_)), "{affinity:?}");
        }
    }

    #[test]
    fn blob_affinity_never_converts() {
        let mut value = Value::Text("1.5".to_string().into());
        apply_affinity(&mut value, Affinity::Blob);
        assert!(matches!(value, Value::Text(_)));
    }
}
