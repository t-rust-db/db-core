//! Predicate push-down into a `FROM`-subquery -- see `super`'s module
//! doc.
//!
//! Splits the enclosing query's own `WHERE` into top-level `AND`
//! conjuncts and moves a conjunct into the `FROM`-subquery's own `WHERE`
//! when doing so is provably safe, so the subquery materializes fewer
//! rows. Ported from sqlite-rs's `subquery/pushdown.rs`; the reference's
//! `require_qualified` rule (a conjunct's unqualified column is
//! ambiguous once the enclosing `FROM` has a `JOIN`) is kept, and its
//! recursion over N `FROM` items reduces to db-core's single one.
//!
//! A conjunct containing its own subquery expression (`InSubquery`/
//! `Exists`) is never pushed -- reasoning about a second scope nested
//! inside the moved predicate is out of scope here, matching the
//! reference's conservative default of leaving anything unrecognized
//! exactly where it was.

use super::flatten::{collect_expr_column_names, split_qualified};
use crate::expr::{BinOp, Expr, FromClause, Query, SelectItem};

/// Pushes every safely-movable `WHERE` conjunct of `query` into its
/// `FROM`-subquery, returning whether it moved any.
pub fn push_down_where_predicates(query: &mut Query) -> bool {
    let FromClause::Subquery(inner, alias) = &query.from else {
        return false;
    };
    if !subquery_pushdown_safe(inner) {
        return false;
    }
    let Some(exposed) = exposed_columns(inner) else {
        return false;
    };
    let alias = alias.clone();
    // An unqualified column is only unambiguously the subquery's when
    // there is no `JOIN`ed table it could equally belong to.
    let require_qualified = !query.joins.is_empty();

    let Some(where_expr) = query.where_clause.take() else {
        return false;
    };
    let mut pushed = Vec::new();
    let mut remaining = Vec::new();
    for conjunct in top_level_and_conjuncts(where_expr) {
        if let Some(rewritten) =
            rewrite_for_pushdown(&conjunct, &alias, exposed.as_deref(), require_qualified)
        {
            pushed.push(rewritten);
        } else {
            remaining.push(conjunct);
        }
    }
    query.where_clause = rebuild_conjunction(remaining);
    if pushed.is_empty() {
        return false;
    }
    if let FromClause::Subquery(inner, _) = &mut query.from {
        for conjunct in pushed {
            inner.where_clause = and_exprs(inner.where_clause.take(), conjunct);
        }
    }
    true
}

/// Whether `inner`'s own shape rules out moving a filter earlier: a
/// `DISTINCT`, an aggregate/`GROUP BY`/`HAVING`, or a `LIMIT`/`OFFSET`
/// would all change which rows a pre-filter leaves behind versus
/// filtering the materialized result. Unlike [`super::flatten`]'s check,
/// a `JOIN`ed or subquery `FROM` inside `inner` is fine here -- the
/// predicate is still applied to the same row set, just earlier.
fn subquery_pushdown_safe(inner: &Query) -> bool {
    !inner.distinct
        && inner.group_by.is_empty()
        && inner.having.is_none()
        && inner.limit.is_none()
        && inner.offset.is_none()
}

/// The subquery's projected column names, or `None` for a `SELECT *`
/// (any name passes through unchanged); `Some(None)` for a projection
/// this pass can't map back (an aggregate/window item has no single
/// underlying column a predicate on it could be rewritten against).
fn exposed_columns(inner: &Query) -> Option<Option<Vec<String>>> {
    if inner.columns.iter().any(|c| matches!(c, SelectItem::Star)) {
        return Some(None);
    }
    let mut out = Vec::with_capacity(inner.columns.len());
    for col in &inner.columns {
        let SelectItem::Column(name) = col else {
            return None;
        };
        out.push(name.clone());
    }
    Some(Some(out))
}

/// `expr`, rewritten into the subquery's own scope (its `alias.`
/// qualifiers dropped), or `None` as soon as any column can't be proven
/// to belong solely to the subquery and be identity-mapped.
fn rewrite_for_pushdown(
    expr: &Expr,
    alias: &str,
    exposed: Option<&[String]>,
    require_qualified: bool,
) -> Option<Expr> {
    if contains_subquery(expr) {
        return None;
    }
    let mut names = Vec::new();
    collect_expr_column_names(expr, &mut names);
    if names.is_empty() {
        return None;
    }
    for name in &names {
        let (qualifier, col) = split_qualified(name);
        match qualifier {
            Some(q) if q.eq_ignore_ascii_case(alias) => {}
            Some(_) => return None,
            None if require_qualified => return None,
            None => {}
        }
        if let Some(exposed) = exposed {
            if !exposed.iter().any(|c| c.eq_ignore_ascii_case(col)) {
                return None;
            }
        }
    }
    let mut rewritten = expr.clone();
    strip_alias_in_expr(&mut rewritten, alias);
    Some(rewritten)
}

fn contains_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::InSubquery { .. } | Expr::Exists { .. } => true,
        Expr::Column(_) | Expr::Literal(_) => false,
        Expr::BinaryOp(lhs, _, rhs) => contains_subquery(lhs) || contains_subquery(rhs),
        Expr::Not(inner) | Expr::Neg(inner) | Expr::IsNull { expr: inner, .. } => {
            contains_subquery(inner)
        }
    }
}

fn strip_alias_in_expr(expr: &mut Expr, alias: &str) {
    match expr {
        Expr::Column(name) => {
            if let (Some(q), col) = split_qualified(name) {
                if q.eq_ignore_ascii_case(alias) {
                    *name = col.to_string();
                }
            }
        }
        Expr::Literal(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {}
        Expr::BinaryOp(lhs, _, rhs) => {
            strip_alias_in_expr(lhs, alias);
            strip_alias_in_expr(rhs, alias);
        }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::IsNull { expr: inner, .. } => {
            strip_alias_in_expr(inner, alias);
        }
    }
}

/// Splits an expression into its top-level `AND` conjuncts -- the same
/// split the reference's `top_level_and_conjuncts` makes.
fn top_level_and_conjuncts(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp(lhs, BinOp::And, rhs) => {
            let mut out = top_level_and_conjuncts(*lhs);
            out.extend(top_level_and_conjuncts(*rhs));
            out
        }
        other => vec![other],
    }
}

fn rebuild_conjunction(exprs: Vec<Expr>) -> Option<Expr> {
    exprs
        .into_iter()
        .reduce(|acc, e| Expr::BinaryOp(Box::new(acc), BinOp::And, Box::new(e)))
}

fn and_exprs(existing: Option<Expr>, addition: Expr) -> Option<Expr> {
    Some(match existing {
        Some(existing) => Expr::BinaryOp(Box::new(existing), BinOp::And, Box::new(addition)),
        None => addition,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Query {
        crate::parser::column::parse(sql).unwrap()
    }

    fn inner(query: &Query) -> &Query {
        match &query.from {
            FromClause::Subquery(inner, _) => inner,
            FromClause::Table(_) => panic!("expected a FROM-subquery"),
        }
    }

    #[test]
    fn pushes_a_qualified_conjunct_into_the_subquery() {
        let mut query = parse("SELECT b FROM (SELECT a, b FROM t) x WHERE x.a = 1");
        assert!(push_down_where_predicates(&mut query));
        assert!(query.where_clause.is_none(), "{:?}", query.where_clause);
        // The alias qualifier is dropped: inside the subquery, `a` is
        // its own table's column.
        assert_eq!(
            inner(&query).where_clause,
            Some(Expr::BinaryOp(
                Box::new(Expr::Column("a".to_string())),
                BinOp::Eq,
                Box::new(Expr::Literal(crate::types::Literal::Int(1))),
            ))
        );
    }

    #[test]
    fn pushes_only_the_movable_half_of_a_conjunction() {
        let mut query =
            parse("SELECT b FROM (SELECT a, b FROM t GROUP BY a, b) x WHERE x.a = 1 AND x.b = 2");
        // A `GROUP BY` subquery is not pushdown-safe at all.
        assert!(!push_down_where_predicates(&mut query));
        assert!(query.where_clause.is_some());
    }

    #[test]
    fn does_not_push_a_conjunct_naming_a_column_the_subquery_does_not_expose() {
        let mut query = parse("SELECT b FROM (SELECT b FROM t) x WHERE x.a = 1");
        assert!(!push_down_where_predicates(&mut query));
        assert!(inner(&query).where_clause.is_none());
    }

    #[test]
    fn does_not_push_a_conjunct_containing_its_own_subquery() {
        let mut query =
            parse("SELECT b FROM (SELECT a, b FROM t) x WHERE x.a IN (SELECT a FROM t)");
        assert!(!push_down_where_predicates(&mut query));
        assert!(inner(&query).where_clause.is_none());
    }

    #[test]
    fn conjoins_with_the_subquerys_existing_where() {
        let mut query = parse("SELECT b FROM (SELECT a, b FROM t WHERE b > 0) x WHERE x.a = 1");
        assert!(push_down_where_predicates(&mut query));
        assert!(matches!(
            inner(&query).where_clause,
            Some(Expr::BinaryOp(_, BinOp::And, _))
        ));
    }

    #[test]
    fn a_plain_table_from_is_left_alone() {
        let mut query = parse("SELECT a FROM t WHERE a = 1");
        assert!(!push_down_where_predicates(&mut query));
        assert!(query.where_clause.is_some());
    }
}
