//! Writer unit tests for private helpers; the round trips through the
//! reader, error paths and the property test are in `tests/parquet_writer.rs`.

use super::*;

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
