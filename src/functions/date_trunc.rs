// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `date_trunc(part, ts)` for the column dialect (#647), as DuckDB 1.5 has
//! it over a TIMESTAMP: the start of the calendar unit `ts` falls in, UTC.
//! A TIMESTAMP is an integer count of `per_second` units here (micros or
//! nanos, from the column's type), so the engine rewrites the call to
//! [`DATE_TRUNC_FN`]`(part, ts, per_second)` once it knows the column, and
//! the result stays in the column's units, like `ts` itself.
//!
//! The parts and their aliases are DuckDB's date specifiers, case aside
//! (checked against DuckDB 1.5.5/1.5.6 one by one). Not the row engine's:
//! sqlite3 has no `date_trunc`.

use super::FunctionError;
use crate::value::Value;

/// The internal call the engine rewrites `date_trunc(part, ts)` to, with
/// the column's units per second as a third argument.
pub(crate) const DATE_TRUNC_FN: &str = "__date_trunc";

/// What `date_trunc` truncates to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TruncUnit {
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    /// Monday-based, as ISO weeks.
    Week,
    Month,
    Quarter,
    Year,
    Decade,
    Century,
    Millennium,
    /// The Monday that starts the ISO year the day belongs to.
    IsoYear,
}

/// DuckDB's specifier for `part`, any case; `None` for one it doesn't
/// recognize or doesn't truncate by (`era`, `timezone`, `nanosecond`).
pub(crate) fn trunc_unit(part: &str) -> Option<TruncUnit> {
    use TruncUnit::{
        Century, Day, Decade, Hour, IsoYear, Microsecond, Millennium, Millisecond, Minute, Month,
        Quarter, Second, Week, Year,
    };
    Some(match part.to_ascii_lowercase().as_str() {
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" | "usecond" | "useconds" => {
            Microsecond
        }
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" | "msecond" | "mseconds" => {
            Millisecond
        }
        "second" | "seconds" | "s" | "sec" | "secs" | "epoch" => Second,
        "minute" | "minutes" | "m" | "min" | "mins" => Minute,
        "hour" | "hours" | "h" | "hr" | "hrs" => Hour,
        "day" | "days" | "d" | "dayofmonth" | "dayofweek" | "weekday" | "dow" | "isodow"
        | "doy" | "dayofyear" | "julian" => Day,
        "week" | "weeks" | "w" | "weekofyear" | "yearweek" => Week,
        "month" | "months" | "mon" | "mons" => Month,
        "quarter" | "quarters" => Quarter,
        "year" | "years" | "y" | "yr" | "yrs" => Year,
        "decade" | "decades" | "dec" | "decs" => Decade,
        "century" | "centuries" | "c" | "cent" => Century,
        "millennium" | "millennia" | "mil" | "mils" => Millennium,
        "isoyear" => IsoYear,
        _ => return None,
    })
}

/// DuckDB's error for a part it doesn't know.
pub(crate) fn unknown_part(part: &str) -> String {
    format!("date_trunc: extract specifier \"{part}\" not recognized")
}

/// `value` (`per_second` units since 1970-01-01 UTC) truncated to `unit`,
/// in the same units; `None` past `i64`.
pub(crate) fn truncate(unit: TruncUnit, value: i64, per_second: i64) -> Option<i64> {
    let floor_to = |step: i64| -> Option<i64> {
        let step = step.max(1);
        value.checked_sub(value.checked_rem_euclid(step)?)
    };
    let per_day = per_second.checked_mul(86_400)?;
    match unit {
        TruncUnit::Microsecond => floor_to(per_second.checked_div(1_000_000)?),
        TruncUnit::Millisecond => floor_to(per_second.checked_div(1_000)?),
        TruncUnit::Second => floor_to(per_second),
        TruncUnit::Minute => floor_to(per_second.checked_mul(60)?),
        TruncUnit::Hour => floor_to(per_second.checked_mul(3_600)?),
        TruncUnit::Day => floor_to(per_day),
        TruncUnit::Week
        | TruncUnit::Month
        | TruncUnit::Quarter
        | TruncUnit::Year
        | TruncUnit::Decade
        | TruncUnit::Century
        | TruncUnit::Millennium
        | TruncUnit::IsoYear => {
            let days = value.checked_div_euclid(per_day)?;
            truncate_days(unit, days)?.checked_mul(per_day)
        }
    }
}

/// The first day (days since 1970-01-01) of the `unit` that day `days`
/// falls in, for the units of a day or more.
fn truncate_days(unit: TruncUnit, days: i64) -> Option<i64> {
    let (year, month, _) = civil_from_days(days)?;
    match unit {
        TruncUnit::Week => monday_of(days),
        TruncUnit::Month => days_from_civil(year, month, 1),
        TruncUnit::Quarter => {
            let first = month
                .checked_sub(1)?
                .checked_div(3)?
                .checked_mul(3)?
                .checked_add(1)?;
            days_from_civil(year, first, 1)
        }
        TruncUnit::Year => days_from_civil(year, 1, 1),
        TruncUnit::Decade => days_from_civil(year.checked_sub(year.checked_rem_euclid(10)?)?, 1, 1),
        TruncUnit::Century => {
            days_from_civil(year.checked_sub(year.checked_rem_euclid(100)?)?, 1, 1)
        }
        TruncUnit::Millennium => {
            days_from_civil(year.checked_sub(year.checked_rem_euclid(1_000)?)?, 1, 1)
        }
        TruncUnit::IsoYear => {
            // The ISO year starts on the Monday of the week holding Jan 4;
            // a day before that start belongs to the previous ISO year.
            let start = |y: i64| monday_of(days_from_civil(y, 1, 4)?);
            let next = start(year.checked_add(1)?)?;
            let this = start(year)?;
            Some(if days >= next {
                next
            } else if days >= this {
                this
            } else {
                start(year.checked_sub(1)?)?
            })
        }
        _ => Some(days),
    }
}

/// The Monday on or before day `days` (1970-01-01 was a Thursday).
fn monday_of(days: i64) -> Option<i64> {
    days.checked_sub(days.checked_add(3)?.checked_rem_euclid(7)?)
}

/// Howard Hinnant's `civil_from_days`: the Gregorian `(year, month, day)`
/// of day `days` since 1970-01-01. Shared with `datetime`'s column-dialect
/// functions, which break an epoch timestamp down the same exact way
/// (#1046).
pub(crate) fn civil_from_days(days: i64) -> Option<(i64, u32, u32)> {
    let z = days.checked_add(719_468)?;
    let era = z.checked_div_euclid(146_097)?;
    let day_of_era = z.checked_rem_euclid(146_097)?;
    let year_of_era = day_of_era
        .checked_sub(day_of_era.checked_div(1_460)?)?
        .checked_add(day_of_era.checked_div(36_524)?)?
        .checked_sub(day_of_era.checked_div(146_096)?)?
        .checked_div(365)?;
    let day_of_year = day_of_era.checked_sub(
        year_of_era
            .checked_mul(365)?
            .checked_add(year_of_era.checked_div(4)?)?
            .checked_sub(year_of_era.checked_div(100)?)?,
    )?;
    let march_month = day_of_year
        .checked_mul(5)?
        .checked_add(2)?
        .checked_div(153)?;
    let day = day_of_year
        .checked_sub(
            march_month
                .checked_mul(153)?
                .checked_add(2)?
                .checked_div(5)?,
        )?
        .checked_add(1)?;
    let month = if march_month < 10 {
        march_month.checked_add(3)?
    } else {
        march_month.checked_sub(9)?
    };
    let year = year_of_era
        .checked_add(era.checked_mul(400)?)?
        .checked_add(i64::from(month <= 2))?;
    Some((year, u32::try_from(month).ok()?, u32::try_from(day).ok()?))
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 of the
/// Gregorian date `year-month-day`.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let y = year.checked_sub(i64::from(month <= 2))?;
    let era = y.checked_div_euclid(400)?;
    let year_of_era = y.checked_rem_euclid(400)?;
    let march_month = i64::from(month.checked_add(9)?.checked_rem(12)?);
    let day_of_year = march_month
        .checked_mul(153)?
        .checked_add(2)?
        .checked_div(5)?
        .checked_add(i64::from(day))?
        .checked_sub(1)?;
    let day_of_era = year_of_era
        .checked_mul(365)?
        .checked_add(year_of_era.checked_div(4)?)?
        .checked_sub(year_of_era.checked_div(100)?)?
        .checked_add(day_of_year)?;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

/// [`DATE_TRUNC_FN`]`(part, value, per_second)`: NULL if `part` or `value`
/// is; an unknown part or a non-integer value is an error.
pub(crate) fn call(args: &[Value]) -> Result<Value, FunctionError> {
    let sql = |message: String| FunctionError::Sql(message);
    let [part, value, per_second] = args else {
        return Err(sql(format!(
            "{DATE_TRUNC_FN} takes 3 arguments, got {}",
            args.len()
        )));
    };
    let (Value::Text(part), Value::Integer(value)) = (part, value) else {
        return match (part, value) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            _ => Err(sql(
                "date_trunc: takes a part name and a TIMESTAMP column".to_string()
            )),
        };
    };
    let Value::Integer(per_second) = per_second else {
        return Err(sql("date_trunc: the column's units are missing".to_string()));
    };
    let unit = trunc_unit(part).ok_or_else(|| sql(unknown_part(part)))?;
    truncate(unit, *value, *per_second)
        .map(Value::Integer)
        .ok_or_else(|| sql("date_trunc: timestamp out of range".to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    /// Microseconds of `y-m-d h:mi:s` UTC.
    fn micros(y: i64, m: u32, d: u32, h: i64, mi: i64, s: i64) -> i64 {
        let day = days_from_civil(y, m, d).unwrap();
        ((day * 24 + h) * 60 + mi) * 60_000_000 + s * 1_000_000
    }

    fn trunc(part: &str, value: i64) -> i64 {
        truncate(trunc_unit(part).unwrap(), value, 1_000_000).unwrap()
    }

    /// The DuckDB 1.5 results for 2024-03-07 10:11:12.345678 and edges.
    #[test]
    fn units_match_duckdb() {
        let t = micros(2024, 3, 7, 10, 11, 12) + 345_678;
        assert_eq!(trunc("microseconds", t), t);
        assert_eq!(trunc("ms", t), t - 678);
        assert_eq!(trunc("second", t), micros(2024, 3, 7, 10, 11, 12));
        assert_eq!(trunc("min", t), micros(2024, 3, 7, 10, 11, 0));
        assert_eq!(trunc("HOURS", t), micros(2024, 3, 7, 10, 0, 0));
        assert_eq!(trunc("day", t), micros(2024, 3, 7, 0, 0, 0));
        assert_eq!(trunc("week", t), micros(2024, 3, 4, 0, 0, 0));
        assert_eq!(trunc("month", t), micros(2024, 3, 1, 0, 0, 0));
        assert_eq!(trunc("quarter", t), micros(2024, 1, 1, 0, 0, 0));
        assert_eq!(trunc("year", t), micros(2024, 1, 1, 0, 0, 0));
        assert_eq!(trunc("decade", t), micros(2020, 1, 1, 0, 0, 0));
        assert_eq!(trunc("century", t), micros(2000, 1, 1, 0, 0, 0));
        assert_eq!(trunc("millennium", t), micros(2000, 1, 1, 0, 0, 0));
        // Before 1970: the start of the unit, not toward zero.
        let late_1969 = micros(1969, 12, 31, 23, 59, 59) + 500_000;
        assert_eq!(trunc("day", late_1969), micros(1969, 12, 31, 0, 0, 0));
        assert_eq!(trunc("hour", late_1969), micros(1969, 12, 31, 23, 0, 0));
        assert_eq!(trunc("year", late_1969), micros(1969, 1, 1, 0, 0, 0));
        assert_eq!(trunc("decade", late_1969), micros(1960, 1, 1, 0, 0, 0));
        assert_eq!(
            trunc("week", micros(1970, 1, 1, 5, 0, 0)),
            micros(1969, 12, 29, 0, 0, 0)
        );
        assert_eq!(
            trunc("week", micros(1969, 12, 28, 12, 0, 0)),
            micros(1969, 12, 22, 0, 0, 0)
        );
        assert_eq!(
            trunc("day", micros(1960, 2, 29, 13, 0, 0)),
            micros(1960, 2, 29, 0, 0, 0)
        );
        // The ISO year 2020 began on 2019-12-30.
        assert_eq!(
            trunc("isoyear", micros(2021, 1, 2, 10, 0, 0)),
            micros(2019, 12, 30, 0, 0, 0)
        );
        assert_eq!(
            trunc("isoyear", micros(2021, 1, 4, 0, 0, 0)),
            micros(2021, 1, 4, 0, 0, 0)
        );
        assert_eq!(
            trunc("isoyear", micros(2024, 12, 30, 0, 0, 0)),
            micros(2024, 12, 30, 0, 0, 0)
        );
    }

    #[test]
    fn nanosecond_columns_keep_their_units() {
        let ns = (micros(2024, 3, 7, 10, 11, 12) + 123_456) * 1_000 + 789;
        let unit = |p| trunc_unit(p).unwrap();
        assert_eq!(truncate(unit("us"), ns, 1_000_000_000), Some(ns - 789));
        assert_eq!(
            truncate(unit("day"), ns, 1_000_000_000),
            Some(micros(2024, 3, 7, 0, 0, 0) * 1_000)
        );
    }

    #[test]
    fn aliases_and_refusals_match_duckdb() {
        for part in [
            "c", "cent", "dec", "y", "mons", "d", "dow", "julian", "w", "hr", "m", "epoch",
        ] {
            assert!(trunc_unit(part).is_some(), "{part}");
        }
        for part in [
            "cents",
            "yy",
            " day",
            "nanosecond",
            "ns",
            "era",
            "timezone",
            "",
        ] {
            assert!(trunc_unit(part).is_none(), "{part}");
        }
    }

    #[test]
    fn call_takes_nulls_and_refuses_bad_input() {
        let t = Value::Integer(micros(2024, 3, 7, 10, 0, 0));
        let us = Value::Integer(1_000_000);
        let day = Value::Text("day".into());
        assert_eq!(
            call(&[day.clone(), t.clone(), us.clone()]).unwrap(),
            Value::Integer(micros(2024, 3, 7, 0, 0, 0))
        );
        assert_eq!(
            call(&[Value::Null, t.clone(), us.clone()]).unwrap(),
            Value::Null
        );
        assert_eq!(
            call(&[day.clone(), Value::Null, us.clone()]).unwrap(),
            Value::Null
        );
        let err = call(&[Value::Text("fortnight".into()), t.clone(), us.clone()]).unwrap_err();
        assert!(
            err.to_string().contains("\"fortnight\" not recognized"),
            "{err}"
        );
        assert!(call(&[day.clone(), Value::Text("x".into()), us.clone()]).is_err());
        assert!(call(&[day.clone(), t.clone(), Value::Null]).is_err());
        assert!(call(&[day, t]).is_err());
        // The day of the latest nanosecond timestamp fits; the start of the
        // year of the earliest (1677-09-21) is before it: an error.
        assert!(truncate(TruncUnit::Day, i64::MAX, 1_000_000_000).is_some());
        assert!(truncate(TruncUnit::Year, i64::MIN, 1_000_000_000).is_none());
    }
}
