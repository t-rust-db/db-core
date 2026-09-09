//! `DECIMAL`-annotated columns (#52, #53): a fixed-point value stored as an
//! `unscaled` integer plus a `scale` (digits after the decimal point),
//! preserving exact precision rather than lossily converting to `f64`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    pub unscaled: i128,
    pub scale: i32,
}

impl Decimal {
    /// A lossy `f64` approximation, for arithmetic/comparisons where exact
    /// precision doesn't matter.
    pub fn to_f64(&self) -> f64 {
        self.unscaled as f64 / 10f64.powi(self.scale)
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale <= 0 {
            return write!(f, "{}", self.unscaled * 10i128.pow((-self.scale) as u32));
        }
        let scale = self.scale as u32;
        let divisor = 10i128.pow(scale);
        let sign = if self.unscaled < 0 { "-" } else { "" };
        let magnitude = self.unscaled.unsigned_abs();
        let whole = magnitude / divisor as u128;
        let frac = magnitude % divisor as u128;
        write!(f, "{sign}{whole}.{frac:0width$}", width = scale as usize)
    }
}

/// Decode a big-endian two's-complement integer (as used by
/// `FIXED_LEN_BYTE_ARRAY`-backed decimals) into an `i128`.
pub fn from_be_bytes(bytes: &[u8]) -> i128 {
    let negative = bytes.first().is_some_and(|b| b & 0x80 != 0);
    let mut value: i128 = if negative { -1 } else { 0 };
    for &byte in bytes {
        value = (value << 8) | byte as i128;
    }
    value
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
    fn displays_positive_decimal() {
        assert_eq!(
            Decimal {
                unscaled: 123,
                scale: 2
            }
            .to_string(),
            "1.23"
        );
        assert_eq!(
            Decimal {
                unscaled: 5,
                scale: 2
            }
            .to_string(),
            "0.05"
        );
    }

    #[test]
    fn displays_negative_decimal() {
        assert_eq!(
            Decimal {
                unscaled: -123,
                scale: 2
            }
            .to_string(),
            "-1.23"
        );
    }

    #[test]
    fn displays_zero_scale() {
        assert_eq!(
            Decimal {
                unscaled: 42,
                scale: 0
            }
            .to_string(),
            "42"
        );
    }

    #[test]
    fn to_f64_matches_unscaled_divided_by_scale() {
        assert_eq!(
            Decimal {
                unscaled: 1230,
                scale: 3
            }
            .to_f64(),
            1.23
        );
    }

    #[test]
    fn decodes_be_bytes_positive_and_negative() {
        assert_eq!(from_be_bytes(&[0x00, 0x7B]), 123);
        assert_eq!(from_be_bytes(&[0xFF, 0x85]), -123);
    }
}
