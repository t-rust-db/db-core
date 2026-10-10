//! Column-chunk statistics: `min_value`/`max_value` as PLAIN-encoded bytes
//! of the chunk's smallest and largest non-null value, and the null count.
//! What row-group pruning reads, so a wrong bound here is a skipped match.
//!
//! Orders follow the Parquet type-defined orders: signed for integers,
//! `false < true` for booleans, unsigned byte-wise for UTF-8 strings (the
//! same as `str`'s `Ord`), and IEEE order for floats with NaN ignored and
//! zero bounds widened to `-0.0`/`+0.0`.

use super::ColumnValues;

/// Statistics for one column chunk.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ChunkStats {
    pub(crate) min: Option<Vec<u8>>,
    pub(crate) max: Option<Vec<u8>>,
    pub(crate) null_count: usize,
}

pub(crate) fn chunk_stats(values: &ColumnValues) -> ChunkStats {
    match values {
        ColumnValues::Boolean(v) => ordered(v.iter().copied(), |b| vec![u8::from(*b)]),
        ColumnValues::Int32(v) => ordered(v.iter().copied(), |x| x.to_le_bytes().to_vec()),
        ColumnValues::Int64(v) | ColumnValues::TimestampMicros(v) => {
            ordered(v.iter().copied(), |x| x.to_le_bytes().to_vec())
        }
        ColumnValues::Float(v) => real(v.iter().copied()),
        ColumnValues::Double(v) => real(v.iter().copied()),
        ColumnValues::Utf8(v) => ordered(v.iter().map(Option::as_deref), |s| s.as_bytes().to_vec()),
    }
}

fn ordered<T: Ord + Copy>(
    values: impl Iterator<Item = Option<T>>,
    encode: impl Fn(&T) -> Vec<u8>,
) -> ChunkStats {
    let mut null_count = 0usize;
    let mut bounds: Option<(T, T)> = None;
    for value in values {
        match value {
            None => null_count = null_count.saturating_add(1),
            Some(x) => {
                bounds = Some(match bounds {
                    None => (x, x),
                    Some((lo, hi)) => (lo.min(x), hi.max(x)),
                });
            }
        }
    }
    ChunkStats {
        min: bounds.as_ref().map(|(lo, _)| encode(lo)),
        max: bounds.as_ref().map(|(_, hi)| encode(hi)),
        null_count,
    }
}

/// A float type the statistics can order and encode.
trait Real: Copy {
    fn widen(self) -> f64;
    fn le_bytes(self) -> Vec<u8>;
    fn zero(negative: bool) -> Self;
}

impl Real for f32 {
    fn widen(self) -> f64 {
        f64::from(self)
    }
    fn le_bytes(self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
    fn zero(negative: bool) -> Self {
        if negative {
            -0.0
        } else {
            0.0
        }
    }
}

impl Real for f64 {
    fn widen(self) -> f64 {
        self
    }
    fn le_bytes(self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
    fn zero(negative: bool) -> Self {
        if negative {
            -0.0
        } else {
            0.0
        }
    }
}

/// Floats: NaN is skipped (a chunk of only NaN has no bounds), and a zero
/// bound is written as `-0.0` for min and `+0.0` for max, so a reader
/// comparing either zero against the bounds never prunes it.
fn real<T: Real>(values: impl Iterator<Item = Option<T>>) -> ChunkStats {
    let mut null_count = 0usize;
    let mut bounds: Option<(T, T)> = None;
    for value in values {
        let Some(x) = value else {
            null_count = null_count.saturating_add(1);
            continue;
        };
        if x.widen().is_nan() {
            continue;
        }
        bounds = Some(match bounds {
            None => (x, x),
            Some((lo, hi)) => (
                if x.widen() < lo.widen() { x } else { lo },
                if x.widen() > hi.widen() { x } else { hi },
            ),
        });
    }
    let zero_widened = |v: T, negative: bool| {
        if v.widen() == 0.0 {
            T::zero(negative)
        } else {
            v
        }
    };
    ChunkStats {
        min: bounds.map(|(lo, _)| zero_widened(lo, true).le_bytes()),
        max: bounds.map(|(_, hi)| zero_widened(hi, false).le_bytes()),
        null_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_use_signed_order_and_count_nulls() {
        let s = chunk_stats(&ColumnValues::Int64(vec![
            Some(3),
            None,
            Some(-9),
            Some(7),
            None,
        ]));
        assert_eq!(s.min, Some((-9i64).to_le_bytes().to_vec()));
        assert_eq!(s.max, Some(7i64.to_le_bytes().to_vec()));
        assert_eq!(s.null_count, 2);
    }

    #[test]
    fn all_null_has_no_bounds() {
        let s = chunk_stats(&ColumnValues::Int32(vec![None, None]));
        assert_eq!(
            s,
            ChunkStats {
                min: None,
                max: None,
                null_count: 2
            }
        );
    }

    #[test]
    fn booleans_and_timestamps() {
        let s = chunk_stats(&ColumnValues::Boolean(vec![Some(true), Some(false)]));
        assert_eq!((s.min, s.max), (Some(vec![0]), Some(vec![1])));
        let s = chunk_stats(&ColumnValues::TimestampMicros(vec![Some(5)]));
        assert_eq!(s.min, Some(5i64.to_le_bytes().to_vec()));
    }

    #[test]
    fn strings_compare_as_unsigned_bytes() {
        // 'é' (0xc3 0xa9) sorts after 'z' byte-wise; "" is the smallest.
        let s = chunk_stats(&ColumnValues::Utf8(vec![
            Some("z".into()),
            Some("é".into()),
            Some(String::new()),
            None,
        ]));
        assert_eq!(s.min, Some(Vec::new()));
        assert_eq!(s.max, Some("é".as_bytes().to_vec()));
        assert_eq!(s.null_count, 1);
    }

    #[test]
    fn floats_skip_nan_and_widen_zero_bounds() {
        let s = chunk_stats(&ColumnValues::Double(vec![
            Some(f64::NAN),
            Some(0.0),
            Some(-0.0),
        ]));
        assert_eq!(s.min, Some((-0.0f64).to_le_bytes().to_vec()));
        assert_eq!(s.max, Some(0.0f64.to_le_bytes().to_vec()));
        let s = chunk_stats(&ColumnValues::Float(vec![
            Some(2.5),
            Some(-1.0),
            None,
            Some(f32::NAN),
        ]));
        assert_eq!(s.min, Some((-1.0f32).to_le_bytes().to_vec()));
        assert_eq!(s.max, Some(2.5f32.to_le_bytes().to_vec()));
        assert_eq!(s.null_count, 1);
        let s = chunk_stats(&ColumnValues::Float(vec![Some(f32::NAN)]));
        assert_eq!((s.min, s.max), (None, None));
    }

    #[test]
    fn infinities_are_ordinary_bounds() {
        let s = chunk_stats(&ColumnValues::Double(vec![
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
        ]));
        assert_eq!(s.min, Some(f64::NEG_INFINITY.to_le_bytes().to_vec()));
        assert_eq!(s.max, Some(f64::INFINITY.to_le_bytes().to_vec()));
    }
}
