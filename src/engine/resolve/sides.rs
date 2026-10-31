// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Which Parquet table probes when a cross-mode join names several (#782).
//!
//! A join of one Parquet table to SQLite lookup tables always probes with
//! the Parquet table (#598). With two Parquet tables one of them builds in
//! memory and the other streams through it row group by row group, so the
//! larger should probe: this module picks it from the footer row counts, not
//! from the order the query is written in, and rewrites the `FROM` clause
//! so the chosen table is first, the shape the star-join planner takes.
//!
//! The query must be a star around the probe: every other table joined to
//! it directly, by an `ON a.x = b.y` or a `USING`. A `LEFT JOIN` keeps the
//! rows of its left side, so with one in the query the `FROM` table alone
//! may probe and nothing is rewritten.

use super::ColumnSource;
use crate::engine::{EngineError, ErrorKind};
use crate::parser::ast::{
    BinaryOp, Expr, ExprKind, FromClause, Join, JoinConstraint, JoinOp, Select, TableRef,
};

/// The index into `tables` of the Parquet table that probes `select`, and
/// `select` with that table first in its `FROM`. A query that names one
/// Parquet table is returned as it is unless it is an inner star join
/// written with a SQLite table first, which is rewritten probe-first (#919).
pub(super) fn choose_probe(
    select: Select,
    tables: &[ColumnSource<'_>],
) -> Result<(Select, usize), EngineError> {
    let Some(from) = &select.from else {
        return Ok((select, 0));
    };
    let named = named_tables(from);
    let present: Vec<usize> = tables
        .iter()
        .enumerate()
        .filter(|(_, t)| named.iter().any(|n| n.eq_ignore_ascii_case(t.table_name())))
        .map(|(i, _)| i)
        .collect();
    let first = from.first.name().unwrap_or_default();
    let only_inner = from.joins.iter().all(|j| j.op == JoinOp::Inner);
    let mut best: Option<(usize, i64)> = None;
    if present.len() > 1 {
        for &i in &present {
            let Some(table) = tables.get(i) else {
                continue;
            };
            let name = table.table_name();
            // A left join keeps its left side's rows, so only the `FROM`
            // table can probe.
            let allowed = only_inner || name.eq_ignore_ascii_case(first);
            if !allowed || !is_star_around(from, &named, name) {
                continue;
            }
            let rows = table.table_stats().rows;
            // The larger probes; a tie keeps the one written first.
            if best.is_none_or(|(_, most)| rows > most) {
                best = Some((i, rows));
            }
        }
    } else {
        // One Parquet table probes wherever the query writes it; an inner
        // join written SQLite-first is put probe-first too, the shape row
        // group pruning reads its `JOIN` targets as build tables in (#919).
        let Some(&probe) = present.first() else {
            return Ok((select, 0));
        };
        let name = tables
            .get(probe)
            .map(|t| t.table_name().to_string())
            .unwrap_or_default();
        if !only_inner || !is_star_around(from, &named, &name) {
            return Ok((select, probe));
        }
        return Ok((with_probe_first(select, &name)?, probe));
    }
    let Some((probe, _)) = best else {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            if only_inner {
                "a join of several Parquet tables must be a star: every other table \
                 joined directly to the probe table by `ON a.x = b.y` or `USING`"
            } else {
                "a join of several Parquet tables with a LEFT JOIN must probe with its \
                 FROM table, joined directly to every other table"
            },
        ));
    };
    let probe_name = tables
        .get(probe)
        .map(|t| t.table_name().to_string())
        .unwrap_or_default();
    if probe_name.eq_ignore_ascii_case(first) {
        return Ok((select, probe));
    }
    Ok((with_probe_first(select, &probe_name)?, probe))
}

/// The names of the tables `from` lists, in the order written.
fn named_tables(from: &FromClause) -> Vec<String> {
    std::iter::once(&from.first)
        .chain(from.joins.iter().map(|j| &j.table))
        .filter_map(|t| t.name().map(str::to_string))
        .collect()
}

/// Whether every table of `named` other than `centre` is joined to it
/// directly.
fn is_star_around(from: &FromClause, named: &[String], centre: &str) -> bool {
    named
        .iter()
        .filter(|n| !n.eq_ignore_ascii_case(centre))
        .all(|other| from.joins.iter().any(|j| links(from, j, centre, other)))
}

/// Whether `join` joins `a` and `b` to each other: an `ON` equating a column
/// of one with a column of the other, or a `USING`, which joins the clause's
/// table to the `FROM` table.
fn links(from: &FromClause, join: &Join, a: &str, b: &str) -> bool {
    let pair = |x: &str, y: &str| {
        (x.eq_ignore_ascii_case(a) && y.eq_ignore_ascii_case(b))
            || (x.eq_ignore_ascii_case(b) && y.eq_ignore_ascii_case(a))
    };
    match &join.constraint {
        Some(JoinConstraint::On(on)) => on_tables(on).is_some_and(|(x, y)| pair(x, y)),
        Some(JoinConstraint::Using(_)) => match (join.table.name(), from.first.name()) {
            (Some(x), Some(y)) => pair(x, y),
            _ => false,
        },
        None => false,
    }
}

/// The tables of the two columns an `ON` equates: `ON a.x = b.y`.
fn on_tables(on: &Expr) -> Option<(&str, &str)> {
    let ExprKind::Binary {
        op: BinaryOp::Eq,
        lhs,
        rhs,
    } = &on.kind
    else {
        return None;
    };
    Some((table_of(lhs)?, table_of(rhs)?))
}

/// The table a column is qualified with.
fn table_of(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Column { table, .. } => table.as_deref(),
        _ => None,
    }
}

/// `select` with `probe` as its `FROM` table and every other table joined
/// to it by the clause that linked the two. Inner joins only, which the
/// caller guarantees: swapping the sides of an inner join changes nothing.
fn with_probe_first(mut select: Select, probe: &str) -> Result<Select, EngineError> {
    let Some(from) = select.from.take() else {
        return Ok(select);
    };
    let mut refs: Vec<TableRef> = std::iter::once(from.first.clone())
        .chain(from.joins.iter().map(|j| j.table.clone()))
        .collect();
    let at = refs
        .iter()
        .position(|t| t.name().is_some_and(|n| n.eq_ignore_ascii_case(probe)))
        .ok_or_else(|| {
            EngineError::new(ErrorKind::Compile, format!("`{probe}` is not in the FROM"))
        })?;
    let probe_ref = refs.remove(at);
    let mut joins = Vec::with_capacity(refs.len());
    for other in refs {
        let other_name = other.name().unwrap_or_default().to_string();
        let clause = from
            .joins
            .iter()
            .find(|j| links(&from, j, probe, &other_name))
            .ok_or_else(|| {
                EngineError::new(
                    ErrorKind::Compile,
                    format!("`{other_name}` is not joined to `{probe}`"),
                )
            })?;
        joins.push(Join {
            table: other,
            ..clause.clone()
        });
    }
    select.from = Some(FromClause {
        first: probe_ref,
        joins,
    });
    Ok(select)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(sql: &str) -> Select {
        crate::parser::parse(sql).unwrap()
    }

    fn from_of(select: &Select) -> &FromClause {
        select.from.as_ref().unwrap()
    }

    #[test]
    fn a_star_is_found_through_on_in_either_order() {
        let s = select(
            "SELECT 1 FROM events JOIN transactions ON transactions.sid = events.sid \
             JOIN channels ON events.cid = channels.cid",
        );
        let from = from_of(&s);
        let named = named_tables(from);
        assert!(is_star_around(from, &named, "events"));
        assert!(!is_star_around(from, &named, "transactions"));
        assert!(!is_star_around(from, &named, "channels"));
    }

    #[test]
    fn a_chain_is_not_a_star() {
        let s = select("SELECT 1 FROM a JOIN b ON a.k = b.k JOIN c ON b.j = c.j");
        let from = from_of(&s);
        let named = named_tables(from);
        assert!(is_star_around(from, &named, "b"));
        assert!(!is_star_around(from, &named, "a"));
        assert!(!is_star_around(from, &named, "c"));
    }

    #[test]
    fn using_joins_the_clauses_table_to_the_from_table() {
        let s = select("SELECT 1 FROM a JOIN b USING (k) JOIN c USING (j)");
        let from = from_of(&s);
        let named = named_tables(from);
        assert!(is_star_around(from, &named, "a"));
        assert!(!is_star_around(from, &named, "b"));
    }

    #[test]
    fn the_probe_moves_first_and_keeps_the_clause_that_linked_each_table() {
        let s = select(
            "SELECT 1 FROM transactions JOIN events ON transactions.sid = events.sid \
             JOIN channels ON events.cid = channels.cid",
        );
        let swapped = with_probe_first(s, "events").unwrap();
        let from = from_of(&swapped);
        assert_eq!(from.first.name(), Some("events"));
        let names: Vec<_> = from.joins.iter().filter_map(|j| j.table.name()).collect();
        assert_eq!(names, ["transactions", "channels"]);
        // Each joined table carries the `ON` that linked it to the probe.
        for join in &from.joins {
            let Some(JoinConstraint::On(on)) = &join.constraint else {
                panic!("expected an ON");
            };
            let (x, y) = on_tables(on).unwrap();
            let table = join.table.name().unwrap();
            assert!(
                (x == "events" && y == table) || (y == "events" && x == table),
                "{x} = {y} for {table}"
            );
        }
    }

    #[test]
    fn a_table_the_probe_is_not_joined_to_cannot_be_moved_under_it() {
        let s = select("SELECT 1 FROM a JOIN b ON a.k = b.k JOIN c ON b.j = c.j");
        assert!(with_probe_first(s, "a").is_err());
    }
}
