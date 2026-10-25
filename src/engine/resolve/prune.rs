// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Row-group pruning for a Parquet probe side from filters on its SQLite
//! lookup tables (#598).
//!
//! `events JOIN event_def ON events.event_id = event_def.event_id WHERE
//! event_def.name = 'x'` filters the *lookup* table, so footer statistics
//! of `events` cannot see the filter. For an `INNER JOIN` a probe row
//! survives only if its key is among the keys of the lookup rows that pass
//! the filter, so this module scans the lookup table with the filter and
//! hands the probe file `event_id = k1 OR event_id = k2 ...` to prune row
//! groups with. The query's own `WHERE` still runs after the join: the
//! predicate here only drops row groups that cannot contribute.
//!
//! Only conjuncts of the `WHERE` that name one lookup table's columns
//! (qualified) and nothing else are used, and a `LEFT JOIN`'s table is
//! never used: its unmatched probe rows must survive.

use std::borrow::Cow;

use super::{plan_err, requalify, split_qualified};
use crate::codegen::batch as planner;
use crate::engine::cross_mode::{self, RowTableSegment};
use crate::engine::row::RowEngine;
use crate::engine::{EngineError, ErrorKind};
use crate::parser::ast::{
    BinaryOp, Expr, ExprKind, Join, JoinConstraint, JoinOp, Literal, ResultColumn, Select,
    TableRefKind,
};
use crate::parser::Span;
use crate::vm::batch::Value;
use crate::vm::engine;

/// Most keys listed as `key = v OR ...`; past it integer keys prune as
/// the range `[min, max]` and other keys do not prune.
const MAX_LISTED_KEYS: usize = 256;

/// What the lookup filters say about the probe file.
#[derive(Debug, Default)]
pub(super) struct Pruning {
    /// A predicate over the probe file's columns that every joined row
    /// satisfies; `None` when nothing is known.
    pub predicate: Option<Expr>,
    /// A lookup filter matched no row: an inner join has no rows at all.
    pub empty: bool,
}

/// The pruning `select`'s `WHERE` implies on the probe file's row groups.
pub(super) fn probe_pruning(select: &Select, lookup: &RowEngine) -> Result<Pruning, EngineError> {
    let mut pruning = Pruning::default();
    let (Some(from), Some(where_clause)) = (&select.from, &select.where_clause) else {
        return Ok(pruning);
    };
    let mut conjuncts = Vec::new();
    split_and(where_clause, &mut conjuncts);
    for join in from.joins.iter().filter(|j| j.op == JoinOp::Inner) {
        let TableRefKind::Name(table) = &join.table.kind else {
            continue;
        };
        let Some((probe_col, lookup_col)) = join_keys(join, table) else {
            continue;
        };
        let filters: Vec<&Expr> = conjuncts
            .iter()
            .filter(|c| references_only(c, table))
            .collect();
        if filters.is_empty() {
            continue;
        }
        let keys = lookup_keys(select, lookup, table, &lookup_col, &filters)?;
        match key_predicate(&probe_col, &keys) {
            KeyPredicate::Empty => pruning.empty = true,
            KeyPredicate::Unknown => {}
            KeyPredicate::Expr(e) => {
                pruning.predicate = Some(match pruning.predicate.take() {
                    Some(prev) => binary(BinaryOp::And, prev, e),
                    None => e,
                });
            }
        }
    }
    Ok(pruning)
}

fn split_and(expr: &Expr, out: &mut Vec<Expr>) {
    match &expr.kind {
        ExprKind::Paren(inner) => split_and(inner, out),
        ExprKind::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
        } => {
            split_and(lhs, out);
            split_and(rhs, out);
        }
        _ => out.push(expr.clone()),
    }
}

/// Whether `expr` is built only from literals and columns qualified with
/// `table`, in the shapes the batch planner evaluates as a filter. Any
/// other shape (function call, subquery, bind parameter, unqualified
/// column) is not used, so a column of another table is never mistaken
/// for the lookup's.
fn references_only(expr: &Expr, table: &str) -> bool {
    match &expr.kind {
        ExprKind::Literal(_) => true,
        ExprKind::Column {
            table: Some(t),
            catalog: None,
            ..
        } => t.eq_ignore_ascii_case(table),
        ExprKind::Paren(e) | ExprKind::Unary { expr: e, .. } | ExprKind::IsNull { expr: e, .. } => {
            references_only(e, table)
        }
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            [lhs, rhs].iter().all(|e| references_only(e, table))
        }
        ExprKind::Between { expr, lo, hi, .. } => {
            [expr, lo, hi].iter().all(|e| references_only(e, table))
        }
        ExprKind::In { expr, list, .. } => {
            references_only(expr, table) && list.iter().all(|e| references_only(e, table))
        }
        ExprKind::Like {
            expr,
            pattern,
            escape: None,
            ..
        } => [expr, pattern].iter().all(|e| references_only(e, table)),
        _ => false,
    }
}

/// `(probe column, lookup column)` of `join`'s `ON a.x = b.y` (the lookup
/// column is the one qualified with `table`) or `USING (k)`.
fn join_keys(join: &Join, table: &str) -> Option<(String, String)> {
    let on = match &join.constraint {
        Some(JoinConstraint::On(on)) => on,
        // `USING (k)` names the same column on both sides.
        Some(JoinConstraint::Using(columns)) => {
            return match columns.as_slice() {
                [key] => Some((key.clone(), key.clone())),
                _ => None,
            };
        }
        None => return None,
    };
    let ExprKind::Binary {
        op: BinaryOp::Eq,
        lhs,
        rhs,
    } = &on.kind
    else {
        return None;
    };
    let (a, b) = (column_of(lhs)?, column_of(rhs)?);
    let is_lookup = |c: &(Option<&str>, &str)| c.0.is_some_and(|t| t.eq_ignore_ascii_case(table));
    match (is_lookup(&a), is_lookup(&b)) {
        (false, true) => Some((a.1.to_string(), b.1.to_string())),
        (true, false) => Some((b.1.to_string(), a.1.to_string())),
        _ => None,
    }
}

fn column_of(expr: &Expr) -> Option<(Option<&str>, &str)> {
    match &expr.kind {
        ExprKind::Column { table, name, .. } => Some((table.as_deref(), name.as_str())),
        _ => None,
    }
}

/// The values of `key` over the rows of `table` that pass every filter,
/// found by running `SELECT table.key FROM table WHERE filters` over the
/// materialized lookup table with the batch VM.
fn lookup_keys(
    select: &Select,
    lookup: &RowEngine,
    table: &str,
    key: &str,
    filters: &[&Expr],
) -> Result<Vec<Value>, EngineError> {
    let mut scan = select.clone();
    scan.with_clause = None;
    scan.distinct = None;
    scan.columns = vec![ResultColumn::Expr {
        expr: column(Some(table), key),
        alias: None,
    }];
    if let Some(from) = scan.from.as_mut() {
        from.joins.clear();
        from.first.kind = TableRefKind::Name(table.to_string());
        from.first.alias = None;
    }
    scan.where_clause = filters
        .iter()
        .map(|f| (*f).clone())
        .reduce(|a, b| binary(BinaryOp::And, a, b));
    scan.group_by.clear();
    scan.having = None;
    scan.compound.clear();
    scan.order_by.clear();
    scan.limit = None;
    scan.scope = None;

    let program = planner::compile(&scan).map_err(plan_err)?;
    let qualified = program.columns_to_load();
    let raw: Vec<String> = qualified
        .iter()
        .map(|n| split_qualified(n).1.to_string())
        .collect();
    let batch = cross_mode::scan_table_as_batch(lookup, table, &raw)?;
    let segment = RowTableSegment::new(requalify(batch, &raw, &qualified));
    let output =
        engine::run(&[segment], &program).map_err(|e| EngineError::new(ErrorKind::Execute, e))?;
    Ok(output
        .into_rows()
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect())
}

enum KeyPredicate {
    /// No lookup row passes the filters.
    Empty,
    /// The keys are of a kind (or too many) to prune with.
    Unknown,
    Expr(Expr),
}

/// `probe = k1 OR probe = k2 ...` for `keys`, or their integer range once
/// there are more than [`MAX_LISTED_KEYS`]. A NULL key never joins.
fn key_predicate(probe: &str, keys: &[Value]) -> KeyPredicate {
    let mut literals: Vec<Literal> = Vec::new();
    for key in keys {
        let literal = match key {
            Value::Null => continue,
            Value::Int(i) => Literal::Integer(*i),
            Value::Str(s) => Literal::Str(Cow::as_ref(s).to_string()),
            _ => return KeyPredicate::Unknown,
        };
        if !literals.contains(&literal) {
            literals.push(literal);
        }
    }
    if literals.is_empty() {
        return KeyPredicate::Empty;
    }
    if literals.len() <= MAX_LISTED_KEYS {
        let equal = |l: Literal| binary(BinaryOp::Eq, column(None, probe), literal(l));
        let mut terms = literals.into_iter().map(equal);
        let first = terms.next();
        return first.map_or(KeyPredicate::Unknown, |first| {
            KeyPredicate::Expr(terms.fold(first, |a, b| binary(BinaryOp::Or, a, b)))
        });
    }
    let ints: Vec<i64> = literals
        .iter()
        .filter_map(|l| match l {
            Literal::Integer(i) => Some(*i),
            _ => None,
        })
        .collect();
    let (Some(lo), Some(hi)) = (ints.iter().min(), ints.iter().max()) else {
        return KeyPredicate::Unknown;
    };
    if ints.len() != literals.len() {
        return KeyPredicate::Unknown;
    }
    KeyPredicate::Expr(binary(
        BinaryOp::And,
        binary(
            BinaryOp::Ge,
            column(None, probe),
            literal(Literal::Integer(*lo)),
        ),
        binary(
            BinaryOp::Le,
            column(None, probe),
            literal(Literal::Integer(*hi)),
        ),
    ))
}

fn expr(kind: ExprKind) -> Expr {
    Expr {
        kind,
        span: Span::UNKNOWN,
    }
}

fn column(table: Option<&str>, name: &str) -> Expr {
    expr(ExprKind::Column {
        table: table.map(str::to_string),
        catalog: None,
        name: name.to_string(),
    })
}

fn literal(l: Literal) -> Expr {
    expr(ExprKind::Literal(l))
}

fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    expr(ExprKind::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    })
}

#[cfg(test)]
#[cfg(feature = "storage-column-write")]
mod tests {
    use super::{key_predicate, probe_pruning, KeyPredicate, MAX_LISTED_KEYS};
    use crate::engine::column::BatchEngine;
    use crate::engine::row::RowEngine;
    use crate::engine::Engine as _;
    use crate::parser::ast::{BinaryOp, ExprKind, Select};
    use crate::storage::parquet::writer::{
        ColumnSpec, ColumnType, ColumnValues, Compression, ParquetWriter, WriterOptions,
    };
    use crate::vm::batch::Value;
    use std::path::PathBuf;

    /// `events(event_id)`: ten row groups of 100 ascending ids (group `g`
    /// holds `100g..100g+99`; the writer cuts one group per batch), so a
    /// key set maps onto known groups.
    fn events() -> BatchEngine {
        let options = WriterOptions {
            compression: Compression::Uncompressed,
            row_group_rows: 100,
            dictionary: false,
            ..WriterOptions::default()
        };
        let schema = vec![ColumnSpec::new("event_id", ColumnType::Int64, false)];
        let mut w = ParquetWriter::new(Vec::new(), schema, options).unwrap();
        for group in 0..10 {
            let ids: Vec<Option<i64>> = (group * 100..(group + 1) * 100).map(Some).collect();
            w.write_columns(vec![ColumnValues::Int64(ids)]).unwrap();
        }
        let bytes = w.finish().unwrap().0;
        BatchEngine::from_bytes("events", bytes).unwrap()
    }

    struct Lookup(PathBuf, RowEngine);

    impl Drop for Lookup {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).ok();
        }
    }

    fn lookup(label: &str) -> Lookup {
        let path =
            std::env::temp_dir().join(format!("db-core-prune-{label}-{}.db", std::process::id()));
        std::fs::copy("tests/fixtures/btrees/table_single_page.db", &path).unwrap();
        let mut engine = RowEngine::open(&path).unwrap();
        engine
            .run_query(
                "CREATE TABLE event_def(event_id INTEGER PRIMARY KEY, name TEXT);\
                 INSERT INTO event_def VALUES (5, 'a');\
                 INSERT INTO event_def VALUES (250, 'b');\
                 INSERT INTO event_def VALUES (255, 'b');\
                 INSERT INTO event_def VALUES (900, 'c');",
            )
            .unwrap();
        Lookup(path, engine)
    }

    fn parse(sql: &str) -> Select {
        crate::parser::parse(sql).unwrap()
    }

    /// How many row groups of `events` survive `sql`'s pruning.
    fn surviving_groups(sql: &str, label: &str) -> Option<usize> {
        let lookup = lookup(label);
        let pruning = probe_pruning(&parse(sql), &lookup.1).unwrap();
        if pruning.empty {
            return Some(0);
        }
        let events = events();
        let columns = ["events.event_id".to_string()];
        let kept = events
            .with_probe_segments(&columns, pruning.predicate.as_ref(), |s| Ok(s.len()))
            .unwrap();
        pruning.predicate.map(|_| kept)
    }

    #[test]
    fn a_lookup_filter_prunes_the_row_groups_without_its_keys() {
        let sql = "SELECT 1 FROM events JOIN event_def ON events.event_id = event_def.event_id \
                   WHERE event_def.name = 'b'";
        // keys 250 and 255 are both in group 2
        assert_eq!(surviving_groups(sql, "eq"), Some(1));
        let sql = "SELECT 1 FROM events JOIN event_def ON event_def.event_id = events.event_id \
                   WHERE event_def.name IN ('a', 'c') AND event_def.name <> 'zz'";
        // keys 5 and 900: groups 0 and 9, ON written lookup-first
        assert_eq!(surviving_groups(sql, "in"), Some(2));
    }

    #[test]
    fn using_and_either_on_operand_order_prune_the_same_row_groups() {
        for (join, label) in [
            ("JOIN event_def USING (event_id)", "using"),
            (
                "JOIN event_def ON event_def.event_id = events.event_id",
                "on-swapped",
            ),
        ] {
            let sql = format!("SELECT 1 FROM events {join} WHERE event_def.name = 'b'");
            assert_eq!(surviving_groups(&sql, label), Some(1), "{sql}");
        }
    }

    #[test]
    fn a_lookup_filter_matching_nothing_empties_the_join() {
        let sql = "SELECT 1 FROM events JOIN event_def ON events.event_id = event_def.event_id \
                   WHERE event_def.name = 'nope'";
        assert_eq!(surviving_groups(sql, "none"), Some(0));
    }

    #[test]
    fn filters_that_say_nothing_about_the_probe_keys_prune_nothing() {
        let on = "FROM events JOIN event_def ON events.event_id = event_def.event_id";
        let left = "FROM events LEFT JOIN event_def ON events.event_id = event_def.event_id";
        for (sql, label) in [
            // a LEFT JOIN keeps unmatched probe rows
            (
                format!("SELECT 1 {left} WHERE event_def.name = 'b'"),
                "left",
            ),
            // a probe-side filter is the file's own business
            (format!("SELECT 1 {on} WHERE events.event_id > 10"), "probe"),
            // an OR across both tables is not a conjunct of the lookup alone
            (
                format!("SELECT 1 {on} WHERE event_def.name = 'b' OR events.event_id = 1"),
                "or",
            ),
            // an unqualified column could belong to either table
            (format!("SELECT 1 {on} WHERE name = 'b'"), "bare"),
            (format!("SELECT 1 {on}"), "nowhere"),
        ] {
            assert_eq!(surviving_groups(&sql, label), None, "{sql}");
        }
    }

    #[test]
    fn many_integer_keys_prune_as_a_range_and_other_keys_not_at_all() {
        let many: Vec<Value> = (0..=MAX_LISTED_KEYS as i64 + 1).map(Value::Int).collect();
        let KeyPredicate::Expr(e) = key_predicate("id", &many) else {
            panic!("an integer range");
        };
        assert!(matches!(
            e.kind,
            ExprKind::Binary {
                op: BinaryOp::And,
                ..
            }
        ));
        let many_text: Vec<Value> = (0..=MAX_LISTED_KEYS)
            .map(|i| Value::Str(i.to_string().into()))
            .collect();
        assert!(matches!(
            key_predicate("id", &many_text),
            KeyPredicate::Unknown
        ));
        assert!(matches!(
            key_predicate("id", &[Value::Null]),
            KeyPredicate::Empty
        ));
        assert!(matches!(
            key_predicate("id", &[Value::Float(1.5)]),
            KeyPredicate::Unknown
        ));
    }
}
