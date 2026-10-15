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
        ColumnType::TimestampNanos,
        ColumnType::Binary,
        ColumnType::UInt8,
        ColumnType::UInt16,
        ColumnType::UInt32,
        ColumnType::UInt64,
        ColumnType::Decimal {
            precision: 20,
            scale: 2,
        },
        ColumnType::FixedLenByteArray(16),
    ] {
        let mut v = ColumnValues::empty(t);
        assert!(v.is_for(t));
        assert!(v.is_empty());
        v.append(ColumnValues::Boolean(vec![Some(true)]));
        assert_eq!(v.len(), usize::from(t == ColumnType::Boolean));
    }
}

#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for the writer's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json` by
    //! `make test-mcdc`).

    use super::*;

    fn decimal_schema(precision: u8, scale: u8) -> Result<(), WriteError> {
        validate_schema(&[ColumnSpec::new(
            "d",
            ColumnType::Decimal { precision, scale },
            true,
        )])
    }

    // storage_parquet_writer_validate_schema_0dc3899e:
    // `!(1..=38).contains(&precision) || scale > precision`
    #[test]
    fn mcdc__storage_parquet_writer_validate_schema_0dc3899e__v1_precision_out_of_range_is_refused()
    {
        assert!(matches!(
            decimal_schema(39, 2),
            Err(WriteError::InvalidSchema(_))
        ));
    }

    #[test]
    fn mcdc__storage_parquet_writer_validate_schema_0dc3899e__v2_scale_above_precision_is_refused()
    {
        assert!(matches!(
            decimal_schema(5, 6),
            Err(WriteError::InvalidSchema(_))
        ));
    }

    #[test]
    fn mcdc__storage_parquet_writer_validate_schema_0dc3899e__v3_precision_and_scale_in_range_pass()
    {
        assert!(decimal_schema(38, 38).is_ok());
    }

    fn check(nullable: bool, values: Vec<Option<i32>>) -> Result<(), WriteError> {
        let spec = ColumnSpec::new("i", ColumnType::Int32, nullable);
        let rows = values.len();
        check_column(&spec, &ColumnValues::Int32(values), rows)
    }

    // storage_parquet_writer_check_column_2b1e74ad:
    // `!spec.nullable && values.validity().contains(&false)`
    #[test]
    fn mcdc__storage_parquet_writer_check_column_2b1e74ad__v1_a_null_in_a_required_column_is_refused(
    ) {
        assert!(matches!(
            check(false, vec![Some(1), None]),
            Err(WriteError::NullInRequired { .. })
        ));
    }

    #[test]
    fn mcdc__storage_parquet_writer_check_column_2b1e74ad__v2_a_null_in_a_nullable_column_passes() {
        assert!(check(true, vec![Some(1), None]).is_ok());
    }

    #[test]
    fn mcdc__storage_parquet_writer_check_column_2b1e74ad__v3_a_required_column_without_nulls_passes(
    ) {
        assert!(check(false, vec![Some(1), Some(2)]).is_ok());
    }
}
