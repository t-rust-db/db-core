// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! TIMESTAMP handling shared by the column and stream engines (#679,
//! #647, #1046): the rewrite of a parsed `SELECT` that gives every
//! timestamp comparison, `date_trunc`, date/time function and cast the
//! column's units per second, and the ISO 8601 rendering of a
//! timestamp-valued output column. Free of storage, so the stream engine
//! uses it without the column engine's mmap (ADR 0000's stream profile).

use crate::parser::ast::{BinaryOp, Expr, ExprKind, FunctionArgs, Literal, ResultColumn, Select};

use super::{Cell, EngineError, ErrorKind};

/// The rewrite the column engine applies to every parsed `SELECT` before
/// planning, for a caller that parses and plans itself: a quoted
/// timestamp compared with a TIMESTAMP column becomes the column's integer
/// units (#679), and `date_trunc(part, ts)` becomes the column dialect's
/// `__date_trunc(part, ts, per_second)` (#647). `timestamps` comes from
/// [`parquet_timestamp_columns`](crate::engine::column::parquet_timestamp_columns)
/// or [`snapshot_timestamp_columns`](crate::engine::column::table::snapshot_timestamp_columns).
pub fn resolve_timestamps(
    select: &mut Select,
    timestamps: &[(String, i64)],
) -> Result<(), EngineError> {
    timestamp_literals(select, timestamps)
}

/// Rewrites every comparison of a TIMESTAMP column with a quoted value
/// in WHERE and HAVING into a comparison with the column's integer units
/// (#679): `ts >= '2026-01-02'` compared the raw microseconds with text
/// and matched nothing. A quoted value that is no timestamp is an error,
/// as in DuckDB.
pub(crate) fn timestamp_literals(
    select: &mut Select,
    timestamps: &[(String, i64)],
) -> Result<(), EngineError> {
    for_each_expr_mut(select, &mut |e| {
        timestamp_operator(e, timestamps)?;
        date_trunc_units(e, timestamps)?;
        // #1046: the rest of the date/time family over a TIMESTAMP, and
        // the TIMESTAMP casts, take the column's units too.
        time_fn_units(e, timestamps)?;
        cast_units(e, timestamps)
    })?;
    if timestamps.is_empty() {
        return Ok(());
    }
    if let Some(e) = select.where_clause.as_mut() {
        rewrite_timestamp_literals(e, timestamps)?;
    }
    if let Some(e) = select.having.as_mut() {
        rewrite_timestamp_literals(e, timestamps)?;
    }
    Ok(())
}

/// The name and units per second of `expr` when it is a TIMESTAMP column
/// (in parentheses or not), else `None`. The name is owned: planning-time
/// only, and it keeps the signature within the qualified subset.
fn timestamp_column(expr: &Expr, timestamps: &[(String, i64)]) -> Option<(String, i64)> {
    match &expr.kind {
        ExprKind::Column { name, .. } => timestamps
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(name))
            .map(|(_, per_second)| (name.clone(), *per_second)),
        ExprKind::Paren(inner) => timestamp_column(inner, timestamps),
        _ => None,
    }
}

/// The units per second of `expr` when it is timestamp-valued (#1046): a
/// TIMESTAMP column, a parenthesized one, a rewritten
/// `__date_trunc(part, ts, units)` result (whose units are its third
/// argument), or a `min`/`max` of one of those -- so `min(ts)` prints as
/// ISO 8601 too, as DuckDB prints a TIMESTAMP aggregate.
fn expr_timestamp_units(expr: &Expr, timestamps: &[(String, i64)]) -> Option<i64> {
    match &expr.kind {
        ExprKind::Column { .. } | ExprKind::Paren(_) => {
            timestamp_column(expr, timestamps).map(|(_, per_second)| per_second)
        }
        ExprKind::FunctionCall {
            name,
            args: FunctionArgs::List(args),
            ..
        } if name.eq_ignore_ascii_case(crate::functions::date_trunc::DATE_TRUNC_FN)
            && args.len() == 3 =>
        {
            match args.get(2).map(|a| &a.kind) {
                Some(ExprKind::Literal(Literal::Integer(per_second))) => Some(*per_second),
                _ => None,
            }
        }
        ExprKind::FunctionCall {
            name,
            args: FunctionArgs::List(args),
            ..
        } if (name.eq_ignore_ascii_case("min") || name.eq_ignore_ascii_case("max"))
            && args.len() == 1 =>
        {
            args.first()
                .and_then(|a| expr_timestamp_units(a, timestamps))
        }
        _ => None,
    }
}

/// `//` and `%` are not defined for a TIMESTAMP, as in DuckDB (#647):
/// its values are integers here, but `ts // 86400000000` would bucket by
/// a unit the reader can't see. `date_trunc` buckets a timestamp.
fn timestamp_operator(expr: &mut Expr, timestamps: &[(String, i64)]) -> Result<(), EngineError> {
    if let ExprKind::Binary {
        op: op @ (BinaryOp::IntDiv | BinaryOp::Mod),
        lhs,
        rhs,
    } = &expr.kind
    {
        if let Some((name, _)) = [lhs, rhs]
            .into_iter()
            .find_map(|side| timestamp_column(side, timestamps))
        {
            let symbol = if *op == BinaryOp::IntDiv { "//" } else { "%" };
            return Err(EngineError::new(
                ErrorKind::Compile,
                format!(
                    "operator {symbol} is not defined for TIMESTAMP column {name}; \
                     use date_trunc('day', {name}) to bucket it"
                ),
            ));
        }
    }
    Ok(())
}

/// `date_trunc(part, ts)` becomes the column dialect's
/// `__date_trunc(part, ts, per_second)` with the TIMESTAMP column's units
/// (#647). As in DuckDB, an unknown part and a second argument that is no
/// TIMESTAMP column are errors, before any row is read.
fn date_trunc_units(expr: &mut Expr, timestamps: &[(String, i64)]) -> Result<(), EngineError> {
    let span = expr.span;
    let ExprKind::FunctionCall {
        name,
        args: FunctionArgs::List(args),
        ..
    } = &mut expr.kind
    else {
        return Ok(());
    };
    if !name.eq_ignore_ascii_case("date_trunc") || args.len() != 2 {
        return Ok(());
    }
    let compile = |message: String| EngineError::new(ErrorKind::Compile, message);
    if let Some(ExprKind::Literal(Literal::Str(part))) = args.first().map(|a| &a.kind) {
        if crate::functions::date_trunc::trunc_unit(part).is_none() {
            return Err(compile(crate::functions::date_trunc::unknown_part(part)));
        }
    }
    let per_second = args
        .get(1)
        .and_then(|a| expr_timestamp_units(a, timestamps))
        .ok_or_else(|| {
            compile("date_trunc(part, ts) takes a TIMESTAMP column for ts".to_string())
        })?;
    *name = crate::functions::date_trunc::DATE_TRUNC_FN.to_string();
    args.push(Expr {
        kind: ExprKind::Literal(Literal::Integer(per_second)),
        span,
    });
    Ok(())
}

/// The date/time functions over a timestamp-valued expression, rewritten
/// to the column dialect's `__`-prefixed calls with the units appended,
/// exactly as `date_trunc` is (#647, #1046): `date_part(part, ts)` and
/// `extract(part FROM ts)` (which the grammar lowers to `date_part`),
/// `year`..`second(ts)`, `strftime(ts, fmt)` (DuckDB's argument order),
/// `epoch(ts)`, `epoch_ms(ts)`. A call whose timestamp argument is no
/// TIMESTAMP is an error before any row is read, as in DuckDB.
fn time_fn_units(expr: &mut Expr, timestamps: &[(String, i64)]) -> Result<(), EngineError> {
    use crate::functions::datetime::{DATE_PART_FN, EPOCH_FN, EPOCH_MS_FN, STRFTIME_FN};
    let span = expr.span;
    let ExprKind::FunctionCall {
        name,
        args: FunctionArgs::List(args),
        ..
    } = &mut expr.kind
    else {
        return Ok(());
    };
    let compile = |message: String| EngineError::new(ErrorKind::Compile, message);
    let lower = name.to_ascii_lowercase();

    // `year(ts)` .. `second(ts)` lower to `date_part('year', ts, units)`.
    let shorthand = match lower.as_str() {
        "year" => Some("year"),
        "month" => Some("month"),
        "day" => Some("day"),
        "hour" => Some("hour"),
        "minute" => Some("minute"),
        "second" => Some("second"),
        _ => None,
    };
    if let Some(part) = shorthand {
        if args.len() != 1 {
            return Ok(());
        }
        let per_second = args
            .first()
            .and_then(|a| expr_timestamp_units(a, timestamps))
            .ok_or_else(|| {
                compile(format!(
                    "{lower}() takes a TIMESTAMP column for its argument"
                ))
            })?;
        *name = DATE_PART_FN.to_string();
        args.insert(
            0,
            Expr {
                kind: ExprKind::Literal(Literal::Str(part.to_string())),
                span,
            },
        );
        args.push(Expr {
            kind: ExprKind::Literal(Literal::Integer(per_second)),
            span,
        });
        return Ok(());
    }

    let (fn_name, ts_index, arity) = match lower.as_str() {
        "date_part" => (DATE_PART_FN, 1, 2),
        // DuckDB's order: the timestamp is the first argument.
        "strftime" => (STRFTIME_FN, 0, 2),
        "epoch" => (EPOCH_FN, 0, 1),
        "epoch_ms" => (EPOCH_MS_FN, 0, 1),
        _ => return Ok(()),
    };
    if args.len() != arity {
        return Ok(());
    }
    if lower == "date_part" {
        if let Some(ExprKind::Literal(Literal::Str(part))) = args.first().map(|a| &a.kind) {
            if !crate::functions::datetime::known_extract_part(part) {
                return Err(compile(crate::functions::datetime::unknown_extract_part(
                    part,
                )));
            }
        }
    }
    let per_second = args
        .get(ts_index)
        .and_then(|a| expr_timestamp_units(a, timestamps))
        .ok_or_else(|| {
            compile(format!(
                "{lower}() takes a TIMESTAMP column as its {}-argument",
                if ts_index == 0 { "first" } else { "second" }
            ))
        })?;
    *name = fn_name.to_string();
    args.push(Expr {
        kind: ExprKind::Literal(Literal::Integer(per_second)),
        span,
    });
    Ok(())
}

/// `CAST(x AS VARCHAR|TEXT|DATE)` (#1046): over a timestamp-valued `x`
/// the cast becomes the column dialect's `__timestamp_text` /
/// `__timestamp_date` call with the units appended; over anything else it
/// is a compile error (the parser admits only these three types, so this
/// is the whole supported CAST surface).
fn cast_units(expr: &mut Expr, timestamps: &[(String, i64)]) -> Result<(), EngineError> {
    use crate::functions::datetime::{TIMESTAMP_DATE_FN, TIMESTAMP_TEXT_FN};
    let span = expr.span;
    let ExprKind::Cast {
        expr: inner,
        type_name,
    } = &mut expr.kind
    else {
        return Ok(());
    };
    let head = type_name
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let fn_name = match head.as_str() {
        "VARCHAR" | "TEXT" => TIMESTAMP_TEXT_FN,
        "DATE" => TIMESTAMP_DATE_FN,
        _ => return Ok(()),
    };
    let per_second = expr_timestamp_units(inner, timestamps).ok_or_else(|| {
        EngineError::new(
            ErrorKind::Compile,
            format!("CAST(x AS {head}) is only supported over a TIMESTAMP column; x is not one"),
        )
    })?;
    let inner = std::mem::replace(
        &mut **inner,
        Expr {
            kind: ExprKind::Literal(Literal::Null),
            span,
        },
    );
    *expr = Expr {
        kind: ExprKind::FunctionCall {
            name: fn_name.to_string(),
            distinct: false,
            args: FunctionArgs::List(vec![
                inner,
                Expr {
                    kind: ExprKind::Literal(Literal::Integer(per_second)),
                    span,
                },
            ]),
            tail: None,
        },
        span,
    };
    Ok(())
}

/// The units per second of an output column's expression when it is
/// timestamp-valued (#1046): a TIMESTAMP column or a `__date_trunc`
/// result. `None` for anything else.
pub(crate) fn output_timestamp_units(expr: &Expr, timestamps: &[(String, i64)]) -> Option<i64> {
    expr_timestamp_units(expr, timestamps)
}

/// Per-output-column units of `select`'s select list, for rendering
/// timestamp-valued columns as ISO 8601 (#1046).
pub(crate) fn output_column_units(
    select: &Select,
    timestamps: &[(String, i64)],
) -> Vec<Option<i64>> {
    select
        .columns
        .iter()
        .map(|col| match col {
            crate::parser::ast::ResultColumn::Expr { expr, .. } => {
                output_timestamp_units(expr, timestamps)
            }
            _ => None,
        })
        .collect()
}

/// One result row's cells, with a timestamp-valued column's integer
/// printed as ISO 8601, as DuckDB prints a TIMESTAMP -- ms, µs and ns
/// storage of one instant all print the same (#1046).
pub(crate) fn render_timestamp_cells(row: Vec<Cell>, units: &[Option<i64>]) -> Vec<Cell> {
    row.into_iter()
        .enumerate()
        .map(|(i, v)| {
            let cell = v;
            match (units.get(i), &cell) {
                (Some(Some(per_second)), Cell::Int(n)) => {
                    crate::functions::datetime::iso8601(*n, *per_second).map_or(cell, Cell::Text)
                }
                _ => cell,
            }
        })
        .collect()
}

/// Calls `f` on every expression of `select` -- select list, WHERE,
/// GROUP BY, HAVING and ORDER BY -- children before parents (#647).
/// Subqueries are left out: their columns are another table's.
fn for_each_expr_mut<F>(select: &mut Select, f: &mut F) -> Result<(), EngineError>
where
    F: FnMut(&mut Expr) -> Result<(), EngineError>,
{
    for column in &mut select.columns {
        if let ResultColumn::Expr { expr, .. } = column {
            walk_expr_mut(expr, f)?;
        }
    }
    if let Some(e) = select.where_clause.as_mut() {
        walk_expr_mut(e, f)?;
    }
    for e in &mut select.group_by {
        walk_expr_mut(e, f)?;
    }
    if let Some(e) = select.having.as_mut() {
        walk_expr_mut(e, f)?;
    }
    for term in &mut select.order_by {
        walk_expr_mut(&mut term.expr, f)?;
    }
    Ok(())
}

fn walk_expr_mut<F>(expr: &mut Expr, f: &mut F) -> Result<(), EngineError>
where
    F: FnMut(&mut Expr) -> Result<(), EngineError>,
{
    match &mut expr.kind {
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            walk_expr_mut(lhs, f)?;
            walk_expr_mut(rhs, f)?;
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Paren(inner)
        | ExprKind::IsNull { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Collate { expr: inner, .. }
        | ExprKind::InSubquery { expr: inner, .. } => walk_expr_mut(inner, f)?,
        ExprKind::FunctionCall {
            args: FunctionArgs::List(list),
            ..
        } => {
            for e in list {
                walk_expr_mut(e, f)?;
            }
        }
        ExprKind::In {
            expr: inner, list, ..
        } => {
            walk_expr_mut(inner, f)?;
            for e in list {
                walk_expr_mut(e, f)?;
            }
        }
        ExprKind::Between {
            expr: inner,
            lo,
            hi,
            ..
        } => {
            walk_expr_mut(inner, f)?;
            walk_expr_mut(lo, f)?;
            walk_expr_mut(hi, f)?;
        }
        ExprKind::Like {
            expr: inner,
            pattern,
            escape,
            ..
        } => {
            walk_expr_mut(inner, f)?;
            walk_expr_mut(pattern, f)?;
            if let Some(e) = escape {
                walk_expr_mut(e, f)?;
            }
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(e) = operand {
                walk_expr_mut(e, f)?;
            }
            for (w, t) in whens {
                walk_expr_mut(w, f)?;
                walk_expr_mut(t, f)?;
            }
            if let Some(e) = else_ {
                walk_expr_mut(e, f)?;
            }
        }
        _ => {}
    }
    f(expr)
}

fn rewrite_timestamp_literals(
    expr: &mut Expr,
    timestamps: &[(String, i64)],
) -> Result<(), EngineError> {
    // #647/#1046: a TIMESTAMP column, or a `__date_trunc` result, compared
    // with a quoted value (`date_trunc('day', ts) = '2026-01-05'`).
    let units = |e: &Expr| expr_timestamp_units(e, timestamps);
    match &mut expr.kind {
        ExprKind::Binary {
            op:
                BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge,
            lhs,
            rhs,
        } => {
            let (lhs_units, rhs_units) = (units(lhs), units(rhs));
            for (per_second, literal) in [(lhs_units, &mut **rhs), (rhs_units, &mut **lhs)] {
                if let (Some(per_second), ExprKind::Literal(Literal::Str(text))) =
                    (per_second, &literal.kind)
                {
                    let value = timestamp_units(text, per_second).ok_or_else(|| {
                        EngineError::new(
                            ErrorKind::Compile,
                            format!("conversion error: '{text}' is not a timestamp"),
                        )
                    })?;
                    literal.kind = ExprKind::Literal(Literal::Integer(value));
                    break;
                }
            }
            Ok(())
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            rewrite_timestamp_literals(lhs, timestamps)?;
            rewrite_timestamp_literals(rhs, timestamps)
        }
        ExprKind::Unary { expr: inner, .. } | ExprKind::Paren(inner) => {
            rewrite_timestamp_literals(inner, timestamps)
        }
        _ => Ok(()),
    }
}

/// `text` (`YYYY-MM-DD`, optionally with `[T ]HH:MM[:SS[.fraction]]`, UTC)
/// in `per_second` units since the epoch; `None` if it is no timestamp.
fn timestamp_units(text: &str, per_second: i64) -> Option<i64> {
    let text = text.trim();
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    if !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // A literal names its own instant, so the clock is read only for
    // `'now'`: a host without one (`wasm32-unknown-unknown`) panics on it.
    let arg = [crate::value::Value::Text(whole.to_string().into())];
    let parsed = if whole.eq_ignore_ascii_case("now") {
        crate::functions::call("unixepoch", &arg)
    } else {
        crate::functions::call_at("unixepoch", &arg, 0)
    };
    let seconds = match parsed {
        Ok(crate::value::Value::Integer(s)) => s,
        _ => return None,
    };
    // The fraction to the unit's precision (truncated past it).
    let digits = per_second.checked_ilog10().unwrap_or(0) as usize;
    let mut sub: String = fraction.chars().take(digits).collect();
    while sub.len() < digits {
        sub.push('0');
    }
    let sub: i64 = if sub.is_empty() { 0 } else { sub.parse().ok()? };
    seconds.checked_mul(per_second)?.checked_add(sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #679: a quoted date or timestamp in a column's units -- micro- and
    /// milliseconds, a fraction kept to the unit's precision -- and `None`
    /// for text that is no timestamp.
    #[test]
    fn timestamp_units_parse_dates_times_and_fractions() {
        let day = 1_767_225_600; // 2026-01-01T00:00:00Z
        assert_eq!(
            timestamp_units("2026-01-01", 1_000_000),
            Some(day * 1_000_000)
        );
        assert_eq!(
            timestamp_units("2026-01-01 10:00:00", 1_000_000),
            Some((day + 36_000) * 1_000_000)
        );
        assert_eq!(
            timestamp_units("2026-01-01T00:00:01.5", 1_000_000),
            Some((day + 1) * 1_000_000 + 500_000)
        );
        assert_eq!(
            timestamp_units("2026-01-01 00:00:00.123456789", 1_000),
            Some(day * 1_000 + 123)
        );
        assert_eq!(timestamp_units("not a time", 1_000_000), None);
        assert_eq!(timestamp_units("2026-01-01 00:00:00.12x", 1_000_000), None);
    }

    /// #679: comparisons of a TIMESTAMP column with a quoted value become
    /// integer comparisons, either side, in WHERE and HAVING; other
    /// columns and other shapes are left alone; a bad timestamp is an
    /// error.
    #[test]
    fn timestamp_literals_rewrite_comparisons_with_timestamp_columns() {
        let ts = [("ts".to_string(), 1_000_000)];
        let rewrite = |q: &str| {
            let mut select = crate::parser::column::parse(q).unwrap();
            timestamp_literals(&mut select, &ts).map(|()| select)
        };
        let select = rewrite(
            "SELECT count(*) FROM t WHERE ts >= '2026-01-01' AND ('2026-01-02' > ts OR s = '2026-01-01')",
        )
        .unwrap();
        let text = format!("{}", select.where_clause.unwrap());
        assert!(text.contains("1767225600000000"), "{text}");
        assert!(text.contains("1767312000000000"), "{text}");
        assert!(
            text.contains("'2026-01-01'"),
            "a non-timestamp column keeps its text: {text}"
        );
        // #1046: `min`/`max` of a TIMESTAMP column are timestamp-valued
        // too, so the quoted bound becomes the column's integer units, as
        // DuckDB casts it; an aggregate over a non-timestamp column keeps
        // its text.
        let grouped = rewrite("SELECT s FROM t GROUP BY s HAVING max(ts) > '2026-01-01'").unwrap();
        assert!(
            format!("{}", grouped.having.unwrap()).contains("1767225600000000"),
            "max(ts) > '2026-01-01' becomes an integer comparison"
        );
        let grouped = rewrite("SELECT s FROM t GROUP BY s HAVING max(s) > '2026-01-01'").unwrap();
        assert!(
            format!("{}", grouped.having.unwrap()).contains("'2026-01-01'"),
            "an aggregate's non-timestamp column keeps its text"
        );
        assert!(rewrite("SELECT count(*) FROM t WHERE ts = 'yesterday-ish'").is_err());
        assert!(timestamp_literals(
            &mut crate::parser::column::parse("SELECT a FROM t").unwrap(),
            &[]
        )
        .is_ok());
    }
    /// #647: `//` and `%` on a TIMESTAMP column are refused, as in DuckDB,
    /// anywhere in the query and through parentheses; on other columns
    /// they stay.
    #[test]
    fn integer_division_of_a_timestamp_column_is_refused() {
        let ts = [("ts".to_string(), 1_000_000)];
        let check = |q: &str| {
            let mut select = crate::parser::column::parse(q).unwrap();
            timestamp_literals(&mut select, &ts)
        };
        for q in [
            "SELECT ts // 86400000000 FROM t",
            "SELECT count(*) FROM t GROUP BY ts % 2",
            "SELECT count(*) FROM t WHERE (ts) // 2 > 0",
            "SELECT 86400 % ts AS r FROM t ORDER BY r",
        ] {
            let err = check(q).unwrap_err();
            assert!(err.message.contains("TIMESTAMP column ts"), "{q}: {err}");
        }
        assert!(check("SELECT a // 2, a % 2 FROM t WHERE ts > 0").is_ok());
    }
    // engine_timestamp_date_trunc_units_8a6e38e9 (#647):
    // `!name.eq_ignore_ascii_case("date_trunc") || args.len() != 2`.
    use crate::parser::ast::{Expr, ExprKind, FunctionArgs, ResultColumn};

    fn first_item(sql: &str) -> Expr {
        match crate::parser::column::parse(sql).unwrap().columns.remove(0) {
            ResultColumn::Expr { expr, .. } => expr,
            other => panic!("expected an expression: {other:?}"),
        }
    }

    fn call_name(expr: &Expr) -> &str {
        match &expr.kind {
            ExprKind::FunctionCall { name, .. } => name,
            _ => "",
        }
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_date_trunc_units_8a6e38e9__v1_two_arg_date_trunc_is_rewritten() {
        let ts = [("ts".to_string(), 1_000_000_000)];
        let mut expr = first_item("SELECT date_trunc('day', ts) FROM t");
        super::date_trunc_units(&mut expr, &ts).unwrap();
        assert_eq!(
            call_name(&expr),
            crate::functions::date_trunc::DATE_TRUNC_FN
        );
        assert_eq!(
            super::output_timestamp_units(&expr, &ts),
            Some(1_000_000_000)
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_date_trunc_units_8a6e38e9__v2_another_function_is_left_alone() {
        let ts = [("ts".to_string(), 1_000_000)];
        let mut expr = first_item("SELECT json_extract(ts, '$.a') FROM t");
        super::date_trunc_units(&mut expr, &ts).unwrap();
        assert_eq!(call_name(&expr), "json_extract");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_date_trunc_units_8a6e38e9__v3_another_arity_is_left_alone() {
        let ts = [("ts".to_string(), 1_000_000)];
        let mut expr = first_item("SELECT date_trunc('day', ts) FROM t");
        if let ExprKind::FunctionCall {
            args: FunctionArgs::List(args),
            ..
        } = &mut expr.kind
        {
            args.pop();
        }
        super::date_trunc_units(&mut expr, &ts).unwrap();
        assert_eq!(call_name(&expr), "date_trunc");
    }

    /// `sql`'s first select item with `date_trunc` rewritten to
    /// `__date_trunc(part, ts, units)` against a µs `ts`.
    fn rewritten_item(sql: &str) -> (Expr, [(String, i64); 1]) {
        let ts = [("ts".to_string(), 1_000_000)];
        let mut expr = first_item(sql);
        super::date_trunc_units(&mut expr, &ts).unwrap();
        (expr, ts)
    }

    fn pop_arg(expr: &mut Expr) {
        if let ExprKind::FunctionCall {
            args: FunctionArgs::List(args),
            ..
        } = &mut expr.kind
        {
            args.pop();
        }
    }

    // engine_timestamp_expr_timestamp_units_fe031764 (#1046):
    // `name == __date_trunc && args.len() == 3`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_expr_timestamp_units_fe031764__v1_rewritten_date_trunc_has_units() {
        let (expr, ts) = rewritten_item("SELECT date_trunc('day', ts) FROM t");
        assert_eq!(super::expr_timestamp_units(&expr, &ts), Some(1_000_000));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_expr_timestamp_units_fe031764__v2_another_function_has_none() {
        let (expr, ts) = rewritten_item("SELECT abs(ts) FROM t");
        assert_eq!(super::expr_timestamp_units(&expr, &ts), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_expr_timestamp_units_fe031764__v3_another_arity_has_none() {
        let (mut expr, ts) = rewritten_item("SELECT date_trunc('day', ts) FROM t");
        pop_arg(&mut expr);
        assert_eq!(
            call_name(&expr),
            crate::functions::date_trunc::DATE_TRUNC_FN
        );
        assert_eq!(super::expr_timestamp_units(&expr, &ts), None);
    }

    // engine_timestamp_expr_timestamp_units_4343dffd (#1046):
    // `(name == min || name == max) && args.len() == 1`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_expr_timestamp_units_4343dffd__v1_min_of_a_timestamp_has_units() {
        let (expr, ts) = rewritten_item("SELECT min(ts) FROM t");
        assert_eq!(super::expr_timestamp_units(&expr, &ts), Some(1_000_000));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_expr_timestamp_units_4343dffd__v2_max_of_a_timestamp_has_units() {
        let (expr, ts) = rewritten_item("SELECT max(ts) FROM t");
        assert_eq!(super::expr_timestamp_units(&expr, &ts), Some(1_000_000));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_expr_timestamp_units_4343dffd__v3_another_aggregate_has_none() {
        let (expr, ts) = rewritten_item("SELECT abs(ts) FROM t");
        assert_eq!(super::expr_timestamp_units(&expr, &ts), None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__engine_timestamp_expr_timestamp_units_4343dffd__v4_min_with_two_args_has_none() {
        // The parser refuses `min(ts, ts)`; a second argument is pushed.
        let (mut expr, ts) = rewritten_item("SELECT min(ts) FROM t");
        if let ExprKind::FunctionCall {
            args: FunctionArgs::List(args),
            ..
        } = &mut expr.kind
        {
            args.push(args[0].clone());
        }
        assert_eq!(super::expr_timestamp_units(&expr, &ts), None);
    }
}
