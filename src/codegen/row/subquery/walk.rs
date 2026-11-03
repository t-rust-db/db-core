// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The subqueries directly inside a `SELECT`'s expressions (#940): its
//! select list, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`/
//! `OFFSET`, `JOIN … ON` and each compound arm's expressions. A caller
//! recurses into each one itself (its `FROM`-subqueries are the caller's
//! own business), so view expansion and flattening reach a subquery
//! expression's own `FROM` as they reach a top-level one.

use super::super::select::CodegenError;
use crate::parser::ast::{Expr, ExprKind, FunctionArgs, JoinConstraint, ResultColumn, Select};

/// Calls `f` on each subquery directly inside `select`'s expressions.
pub(super) fn each_expression_subquery_mut<F>(
    select: &mut Select,
    f: &mut F,
) -> Result<(), CodegenError>
where
    F: FnMut(&mut Select) -> Result<(), CodegenError>,
{
    for column in &mut select.columns {
        if let ResultColumn::Expr { expr, .. } = column {
            expr_mut(expr, f)?;
        }
    }
    if let Some(from) = &mut select.from {
        for join in &mut from.joins {
            if let Some(JoinConstraint::On(on)) = &mut join.constraint {
                expr_mut(on, f)?;
            }
        }
    }
    for arm in &mut select.compound {
        for column in &mut arm.columns {
            if let ResultColumn::Expr { expr, .. } = column {
                expr_mut(expr, f)?;
            }
        }
        if let Some(from) = &mut arm.from {
            for join in &mut from.joins {
                if let Some(JoinConstraint::On(on)) = &mut join.constraint {
                    expr_mut(on, f)?;
                }
            }
        }
        for e in arm
            .where_clause
            .iter_mut()
            .chain(arm.group_by.iter_mut())
            .chain(arm.having.iter_mut())
        {
            expr_mut(e, f)?;
        }
    }
    let limit = select
        .limit
        .iter_mut()
        .flat_map(|l| std::iter::once(&mut l.limit).chain(l.offset.iter_mut()));
    for e in select
        .where_clause
        .iter_mut()
        .chain(select.group_by.iter_mut())
        .chain(select.having.iter_mut())
        .chain(select.order_by.iter_mut().map(|t| &mut t.expr))
        .chain(limit)
    {
        expr_mut(e, f)?;
    }
    Ok(())
}

/// Calls `f` on each subquery directly inside `expr` (#940): an UPDATE's
/// SET values and WHERE, a DELETE's WHERE.
pub(super) fn each_subquery_in_expr_mut<F>(expr: &mut Expr, f: &mut F) -> Result<(), CodegenError>
where
    F: FnMut(&mut Select) -> Result<(), CodegenError>,
{
    expr_mut(expr, f)
}

fn expr_mut<F>(expr: &mut Expr, f: &mut F) -> Result<(), CodegenError>
where
    F: FnMut(&mut Select) -> Result<(), CodegenError>,
{
    match &mut expr.kind {
        ExprKind::Literal(_) | ExprKind::Param(_) | ExprKind::Column { .. } => Ok(()),
        ExprKind::Subquery(subquery) | ExprKind::Exists { subquery, .. } => f(subquery),
        ExprKind::InSubquery { expr, subquery, .. } => {
            expr_mut(expr, f)?;
            f(subquery)
        }
        ExprKind::InSubqueryMulti {
            exprs, subquery, ..
        } => {
            for e in exprs {
                expr_mut(e, f)?;
            }
            f(subquery)
        }
        ExprKind::Unary { expr, .. }
        | ExprKind::IsNull { expr, .. }
        | ExprKind::Cast { expr, .. }
        | ExprKind::Collate { expr, .. }
        | ExprKind::Paren(expr) => expr_mut(expr, f),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            expr_mut(lhs, f)?;
            expr_mut(rhs, f)
        }
        ExprKind::Between { expr, lo, hi, .. } => {
            expr_mut(expr, f)?;
            expr_mut(lo, f)?;
            expr_mut(hi, f)
        }
        ExprKind::In { expr, list, .. } => {
            expr_mut(expr, f)?;
            list.iter_mut().try_for_each(|e| expr_mut(e, f))
        }
        ExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_mut(expr, f)?;
            expr_mut(pattern, f)?;
            escape.iter_mut().try_for_each(|e| expr_mut(e, f))
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            operand.iter_mut().try_for_each(|e| expr_mut(e, f))?;
            for (w, t) in whens {
                expr_mut(w, f)?;
                expr_mut(t, f)?;
            }
            else_.iter_mut().try_for_each(|e| expr_mut(e, f))
        }
        ExprKind::FunctionCall { args, tail, .. } => {
            if let FunctionArgs::List(list) = args {
                list.iter_mut().try_for_each(|e| expr_mut(e, f))?;
            }
            if let Some(tail) = tail {
                tail.filter.iter_mut().try_for_each(|e| expr_mut(e, f))?;
                if let Some(over) = &mut tail.over {
                    over.partition_by
                        .iter_mut()
                        .try_for_each(|e| expr_mut(e, f))?;
                    over.order_by
                        .iter_mut()
                        .try_for_each(|t| expr_mut(&mut t.expr, f))?;
                }
            }
            Ok(())
        }
    }
}

/// Whether `pred` holds for a subquery directly inside `select`'s
/// expressions (the read-only twin of [`each_expression_subquery_mut`]).
pub(super) fn any_expression_subquery<P>(select: &Select, pred: &mut P) -> bool
where
    P: FnMut(&Select) -> bool,
{
    let mut found = false;
    let mut check = |e: &Expr| {
        found = found || expr_any(e, pred);
    };
    for column in &select.columns {
        if let ResultColumn::Expr { expr, .. } = column {
            check(expr);
        }
    }
    for on in join_ons(&select.from) {
        check(on);
    }
    for arm in &select.compound {
        for column in &arm.columns {
            if let ResultColumn::Expr { expr, .. } = column {
                check(expr);
            }
        }
        for on in join_ons(&arm.from) {
            check(on);
        }
        for e in arm
            .where_clause
            .iter()
            .chain(arm.group_by.iter())
            .chain(arm.having.iter())
        {
            check(e);
        }
    }
    let limit = select
        .limit
        .iter()
        .flat_map(|l| std::iter::once(&l.limit).chain(l.offset.iter()));
    for e in select
        .where_clause
        .iter()
        .chain(select.group_by.iter())
        .chain(select.having.iter())
        .chain(select.order_by.iter().map(|t| &t.expr))
        .chain(limit)
    {
        check(e);
    }
    found
}

/// The `JOIN … ON` conditions of a FROM clause.
fn join_ons(from: &Option<crate::parser::ast::FromClause>) -> Vec<&Expr> {
    from.iter()
        .flat_map(|f| &f.joins)
        .filter_map(|j| match &j.constraint {
            Some(JoinConstraint::On(on)) => Some(on),
            _ => None,
        })
        .collect()
}

fn expr_any<P>(expr: &Expr, pred: &mut P) -> bool
where
    P: FnMut(&Select) -> bool,
{
    match &expr.kind {
        ExprKind::Literal(_) | ExprKind::Param(_) | ExprKind::Column { .. } => false,
        ExprKind::Subquery(subquery) | ExprKind::Exists { subquery, .. } => pred(subquery),
        ExprKind::InSubquery { expr, subquery, .. } => expr_any(expr, pred) || pred(subquery),
        ExprKind::InSubqueryMulti {
            exprs, subquery, ..
        } => exprs.iter().any(|e| expr_any(e, pred)) || pred(subquery),
        ExprKind::Unary { expr, .. }
        | ExprKind::IsNull { expr, .. }
        | ExprKind::Cast { expr, .. }
        | ExprKind::Collate { expr, .. }
        | ExprKind::Paren(expr) => expr_any(expr, pred),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            expr_any(lhs, pred) || expr_any(rhs, pred)
        }
        ExprKind::Between { expr, lo, hi, .. } => {
            expr_any(expr, pred) || expr_any(lo, pred) || expr_any(hi, pred)
        }
        ExprKind::In { expr, list, .. } => {
            expr_any(expr, pred) || list.iter().any(|e| expr_any(e, pred))
        }
        ExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_any(expr, pred)
                || expr_any(pattern, pred)
                || escape.iter().any(|e| expr_any(e, pred))
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            operand.iter().any(|e| expr_any(e, pred))
                || whens
                    .iter()
                    .any(|(w, t)| expr_any(w, pred) || expr_any(t, pred))
                || else_.iter().any(|e| expr_any(e, pred))
        }
        ExprKind::FunctionCall { args, tail, .. } => {
            let in_args = match args {
                FunctionArgs::List(list) => list.iter().any(|e| expr_any(e, pred)),
                FunctionArgs::Star => false,
            };
            in_args
                || tail.as_ref().is_some_and(|tail| {
                    tail.filter.iter().any(|e| expr_any(e, pred))
                        || tail.over.as_ref().is_some_and(|over| {
                            over.partition_by.iter().any(|e| expr_any(e, pred))
                                || over.order_by.iter().any(|t| expr_any(&t.expr, pred))
                        })
                })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::parser::row::{parse_select, ParseOutcome};

    fn parsed(sql: &str) -> Select {
        match parse_select(sql) {
            ParseOutcome::Accepted(select) => *select,
            other => panic!("{sql}: {other:?}"),
        }
    }

    /// How many subqueries each walker finds directly in `sql`'s
    /// expressions, both counts checked equal.
    fn found(sql: &str) -> usize {
        let mut select = parsed(sql);
        let mut mutable = 0;
        each_expression_subquery_mut(&mut select, &mut |_| {
            mutable += 1;
            Ok(())
        })
        .unwrap();
        let mut read_only = 0;
        let any = any_expression_subquery(&select, &mut |_| {
            read_only += 1;
            false
        });
        assert!(!any, "{sql}");
        assert_eq!(mutable, read_only, "{sql}");
        mutable
    }

    const S: &str = "(SELECT 1 FROM u)";

    /// #940: a subquery is found in every expression position and inside
    /// every expression form.
    #[test]
    fn finds_a_subquery_in_every_position_and_form() {
        for (sql, n) in [
            (format!("SELECT {S} FROM t"), 1),
            (format!("SELECT a FROM t WHERE a = {S}"), 1),
            (format!("SELECT a FROM t GROUP BY {S}"), 1),
            (format!("SELECT a FROM t GROUP BY a HAVING count(*) > {S}"), 1),
            (format!("SELECT a FROM t ORDER BY {S}"), 1),
            (format!("SELECT a FROM t LIMIT {S} OFFSET {S}"), 2),
            (format!("SELECT a FROM t JOIN v ON v.a = {S}"), 1),
            (
                format!("SELECT a FROM t UNION SELECT {S} FROM w JOIN v ON v.a = {S} WHERE {S} GROUP BY {S} HAVING {S}"),
                5,
            ),
            (format!("SELECT a FROM t WHERE EXISTS {S}"), 1),
            (format!("SELECT a FROM t WHERE a IN {S}"), 1),
            (format!("SELECT a FROM t WHERE {S} IN (SELECT b FROM u)"), 2),
            (format!("SELECT a FROM t WHERE ({S}, a) IN (SELECT b, c FROM u)"), 2),
            (format!("SELECT a FROM t WHERE NOT -{S} IS NULL"), 1),
            (format!("SELECT CAST({S} AS TEXT) COLLATE NOCASE FROM t"), 1),
            (format!("SELECT a FROM t WHERE a IS {S}"), 1),
            (format!("SELECT a FROM t WHERE a BETWEEN {S} AND {S}"), 2),
            (format!("SELECT a FROM t WHERE {S} BETWEEN 1 AND 2"), 1),
            (format!("SELECT a FROM t WHERE a IN (1, {S})"), 1),
            (format!("SELECT a FROM t WHERE {S} LIKE {S} ESCAPE {S}"), 3),
            (format!("SELECT CASE {S} WHEN {S} THEN {S} ELSE {S} END FROM t"), 4),
            (format!("SELECT abs({S}), count(*) FROM t"), 1),
            (format!("SELECT count(*) FILTER (WHERE a > {S}) FROM t"), 1),
            (
                format!("SELECT row_number() OVER (PARTITION BY {S} ORDER BY {S}) FROM t"),
                2,
            ),
            ("SELECT a, 1, ? FROM t WHERE a = 1".to_string(), 0),
        ] {
            assert_eq!(found(&sql), n, "{sql}");
        }
    }

    #[test]
    fn stops_at_the_first_match_and_passes_errors_through() {
        let select = parsed(&format!("SELECT {S}, {S} FROM t"));
        let mut seen = 0;
        assert!(any_expression_subquery(&select, &mut |_| {
            seen += 1;
            true
        }));
        assert_eq!(seen, 1);
        let mut select = parsed(&format!("SELECT {S} FROM t WHERE a = {S}"));
        let err = each_expression_subquery_mut(&mut select, &mut |_| {
            Err(CodegenError::Unsupported {
                reason: "stop".to_string(),
            })
        });
        assert!(err.is_err());
        let mut expr = parsed(&format!("SELECT {S} FROM t")).columns.remove(0);
        let ResultColumn::Expr { expr, .. } = &mut expr else {
            panic!("expected an expression");
        };
        let mut n = 0;
        each_subquery_in_expr_mut(expr, &mut |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 1);
    }
}
