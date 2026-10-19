//! `DECIMAL`-annotated columns (#52, #53): a fixed-point value stored as an
//! `unscaled` integer plus a `scale` (digits after the decimal point),
//! preserving exact precision rather than lossily converting to `f64`.

use std::fmt;

/// An exact fixed-point value: `unscaled * 10^-scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    /// The value with the decimal point removed.
    pub unscaled: i128,
    /// Digits after the decimal point; negative scales multiply by a
    /// power of ten. Read from the file, so any `i32` is possible.
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
        // The scale comes from the file: a power of ten past the integer
        // range falls back to exponent notation instead of overflowing or
        // padding to an absurd width.
        let scale = self.scale.unsigned_abs();
        let pow10 = 10u128.checked_pow(scale);
        if self.scale <= 0 {
            let scaled = pow10
                .and_then(|p| i128::try_from(p).ok())
                .and_then(|p| self.unscaled.checked_mul(p));
            return match scaled {
                Some(v) => write!(f, "{v}"),
                None => write!(f, "{}e{scale}", self.unscaled),
            };
        }
        let sign = if self.unscaled < 0 { "-" } else { "" };
        let magnitude = self.unscaled.unsigned_abs();
        match pow10.and_then(|d| Some((magnitude.checked_div(d)?, magnitude.checked_rem(d)?))) {
            Some((whole, frac)) => {
                write!(f, "{sign}{whole}.{frac:0width$}", width = scale as usize)
            }
            None => write!(f, "{sign}{magnitude}e-{scale}"),
        }
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

    /// #657: a scale from a corrupt file must not overflow `10^scale` (it
    /// panicked in debug builds) or pad to a multi-gigabyte width.
    #[test]
    fn displays_out_of_range_scales_without_overflow() {
        let d = |unscaled, scale| Decimal { unscaled, scale }.to_string();
        // Largest exact scales still use the plain form.
        assert_eq!(d(1, 38), format!("0.{}1", "0".repeat(37)));
        assert_eq!(d(-1, 38), format!("-0.{}1", "0".repeat(37)));
        assert_eq!(d(7, -3), "7000");
        // Past 10^38 / i128 range: exponent notation.
        assert_eq!(d(5, 39), "5e-39");
        assert_eq!(d(-5, i32::MAX), format!("-5e-{}", i32::MAX));
        assert_eq!(d(5, -39), "5e39");
        assert_eq!(d(i128::MAX, -1), format!("{}e1", i128::MAX));
        assert_eq!(d(-5, i32::MIN), format!("-5e{}", 1u64 << 31));
        assert_eq!(d(0, -50), "0e50");
    }

    #[test]
    fn decodes_be_bytes_wider_than_i128_without_panic() {
        // Only the low 16 bytes survive; the point is no overflow panic.
        assert_eq!(from_be_bytes(&[0x01; 20]), from_be_bytes(&[0x01; 16]));
        assert_eq!(from_be_bytes(&[]), 0);
    }
}
