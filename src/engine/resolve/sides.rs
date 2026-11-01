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
//! Every other table must be joined to the probe, directly or through a
//! chain of other tables (tools#108: `events JOIN sessions ON ... JOIN
//! machine ON machine.id = sessions.machine_id`), by an `ON a.x = b.y`, or
//! by a `USING` to the probe. A `LEFT JOIN` keeps the rows of its left
//! side, so with one in the query the `FROM` table alone may probe and
//! nothing is rewritten.

use super::ColumnSource;
use crate::engine::{EngineError, ErrorKind};
use crate::parser::ast::{
    BinaryOp, Expr, ExprKind, FromClause, Join, JoinConstraint, JoinOp, Select, TableRef,
};

/// The index into `tables` of the Parquet table that probes `select`, and
/// `select` with that table first in its `FROM`. A query that names one
/// Parquet table is returned as it is unless it is an inner join written
/// with a SQLite table first, which is rewritten probe-first (#919).
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
            if !allowed || !reaches_all_from(from, &named, name) {
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
        if !only_inner || !reaches_all_from(from, &named, &name) {
            return Ok((select, probe));
        }
        return Ok((with_probe_first(select, &name)?, probe));
    }
    let Some((probe, _)) = best else {
        return Err(EngineError::new(
            ErrorKind::Unsupported,
            if only_inner {
                "a join of several Parquet tables must join every other table to the \
                 probe table, directly or through another table, by `ON a.x = b.y` or \
                 `USING`"
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

/// Whether every table of `named` is joined to `centre`, directly or
/// through tables joined to it ([`join_order`]).
fn reaches_all_from(from: &FromClause, named: &[String], centre: &str) -> bool {
    join_order(from, centre).is_some_and(|order| order.len() == named.len().saturating_sub(1))
}

/// The tables of `from` other than `centre`, each with the clause that
/// joins it to `centre` or to a table before it, in the order that makes
/// that so (written order where it can). `None` when a table is not
/// reached. A `USING` joins its table to the `FROM` table, so it only
/// links a table to `centre`, the `FROM` table once rewritten.
fn join_order(from: &FromClause, centre: &str) -> Option<Vec<(TableRef, Join)>> {
    let mut rest: Vec<TableRef> = std::iter::once(&from.first)
        .chain(from.joins.iter().map(|j| &j.table))
        .filter(|t| !t.name().is_some_and(|n| n.eq_ignore_ascii_case(centre)))
        .cloned()
        .collect();
    let mut placed: Vec<String> = vec![centre.to_string()];
    let mut order = Vec::with_capacity(rest.len());
    while !rest.is_empty() {
        let found = rest.iter().enumerate().find_map(|(i, table)| {
            let name = table.name()?;
            from.joins
                .iter()
                .find(|j| {
                    placed.iter().any(|p| {
                        let using = matches!(j.constraint, Some(JoinConstraint::Using(_)));
                        (!using || p.eq_ignore_ascii_case(centre)) && links(from, j, p, name)
                    })
                })
                .map(|j| (i, j.clone()))
        })?;
        let (i, clause) = found;
        let table = rest.remove(i);
        placed.push(table.name().unwrap_or_default().to_string());
        order.push((table, clause));
    }
    Some(order)
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
/// by the clause that linked it to `probe` or to a table before it
/// ([`join_order`]). Inner joins only, which the caller guarantees:
/// reordering inner joins changes nothing.
fn with_probe_first(mut select: Select, probe: &str) -> Result<Select, EngineError> {
    let Some(from) = select.from.take() else {
        return Ok(select);
    };
    let probe_ref = std::iter::once(&from.first)
        .chain(from.joins.iter().map(|j| &j.table))
        .find(|t| t.name().is_some_and(|n| n.eq_ignore_ascii_case(probe)))
        .cloned()
        .ok_or_else(|| {
            EngineError::new(ErrorKind::Compile, format!("`{probe}` is not in the FROM"))
        })?;
    let order = join_order(&from, probe).ok_or_else(|| {
        EngineError::new(
            ErrorKind::Compile,
            format!("a table is not joined to `{probe}`, directly or through another table"),
        )
    })?;
    let joins = order
        .into_iter()
        .map(|(table, clause)| Join { table, ..clause })
        .collect();
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
    fn a_star_is_reached_through_on_in_either_order() {
        let s = select(
            "SELECT 1 FROM events JOIN transactions ON transactions.sid = events.sid \
             JOIN channels ON events.cid = channels.cid",
        );
        let from = from_of(&s);
        let named = named_tables(from);
        // Each table reaches the others, `events` directly.
        for centre in ["events", "transactions", "channels"] {
            assert!(reaches_all_from(from, &named, centre), "{centre}");
        }
    }

    #[test]
    fn a_chain_reaches_every_table_from_either_end() {
        let s = select("SELECT 1 FROM a JOIN b ON a.k = b.k JOIN c ON b.j = c.j");
        let from = from_of(&s);
        let named = named_tables(from);
        for centre in ["a", "b", "c"] {
            assert!(reaches_all_from(from, &named, centre), "{centre}");
        }
        let apart = select("SELECT 1 FROM a JOIN b ON a.k = b.k JOIN c ON c.j = d.j");
        let from = from_of(&apart);
        assert!(!reaches_all_from(from, &named_tables(from), "a"));
    }

    #[test]
    fn using_joins_the_clauses_table_to_the_from_table() {
        let s = select("SELECT 1 FROM a JOIN b USING (k) JOIN c USING (j)");
        let from = from_of(&s);
        let named = named_tables(from);
        assert!(reaches_all_from(from, &named, "a"));
        assert!(!reaches_all_from(from, &named, "b"));
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
    fn a_chain_is_rewritten_outward_from_the_probe() {
        let s = select("SELECT 1 FROM b JOIN c ON b.j = c.j JOIN a ON a.k = b.k");
        let swapped = with_probe_first(s, "a").unwrap();
        let from = from_of(&swapped);
        assert_eq!(from.first.name(), Some("a"));
        let names: Vec<_> = from.joins.iter().filter_map(|j| j.table.name()).collect();
        assert_eq!(names, ["b", "c"]);
    }

    #[test]
    fn a_table_joined_to_nothing_cannot_be_moved_under_the_probe() {
        let s = select("SELECT 1 FROM a JOIN b ON a.k = b.k JOIN c ON c.j = d.j");
        assert!(with_probe_first(s, "a").is_err());
    }

    #[test]
    fn a_using_links_only_to_the_probe() {
        // `b JOIN c USING (j)` joins c to b, the FROM table; with `a`
        // first it would join c to `a` instead.
        let s = select("SELECT 1 FROM b JOIN c USING (j) JOIN a ON a.k = b.k");
        let from = from_of(&s);
        assert!(!reaches_all_from(from, &named_tables(from), "a"));
    }
}
