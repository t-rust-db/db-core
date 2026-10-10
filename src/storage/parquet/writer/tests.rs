//! Writer unit tests for private helpers; the round trips through the
//! reader, error paths and the property test are in `tests/parquet_writer.rs`.

use super::*;

/// Deterministic xorshift64 for the encoder tests' random cases (the
/// `proptest` macros are outside the qualified subset, so randomized tests
/// under `src/` draw from this instead).
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform-ish in `0..n` (`n > 0`).
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn column_values_helpers() {
    for t in [
        ColumnType::Boolean,
        ColumnType::Int32,
        ColumnType::Int64,
        ColumnType::Float,
        ColumnType::Double,
        ColumnType::Utf8,
        ColumnType::TimestampMicros,
    ] {
        let mut v = ColumnValues::empty(t);
        assert_eq!(v.column_type(), t);
        assert!(v.is_empty());
        v.append(ColumnValues::Boolean(vec![Some(true)]));
        assert_eq!(v.len(), usize::from(t == ColumnType::Boolean));
    }
}
