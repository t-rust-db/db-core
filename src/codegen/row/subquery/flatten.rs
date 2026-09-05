//! `FROM`-subquery flattening -- see `super`'s module doc.
//!
//! Rewrites `SELECT ... FROM (SELECT ... FROM t WHERE w) a WHERE w2`
//! into the equivalent single-level `SELECT ... FROM t WHERE w AND w2`
//! when doing so provably can't change the result, so the query never
//! pays for [`super::from_clause::materialize_from_subquery`]'s
//! ephemeral table at all. Ported from sqlite-rs's
//! `subquery/flatten.rs`, at db-core's own scope: `Query.from` holds
//! exactly one item, so the reference's "flatten one of N `FROM` items,
//! re-qualifying the rest" machinery (its `TableRefSlot`,
//! `rewrite_alias_in_select`) reduces to rewriting the single alias, and
//! its `qualify_with_outer_alias` has nothing to qualify against.

use crate::expr::{BinOp, Expr, FromClause, Query, SelectItem};

/// Flattens `query`'s `FROM`-subquery in place when it is safe to,
/// returning whether it did. Idempotent: a `query` whose `FROM` is
/// already a plain table is left untouched.
pub fn flatten_from_subquery(query: &mut Query) -> bool {
    let FromClause::Subquery(inner, alias) = &query.from else {
        return false;
    };
    if !subquery_flatten_safe(inner) {
        return false;
    }
    let Some(exposed) = exposed_columns(inner) else {
        return false;
    };

    // A `SELECT *` over a subquery that projects a *subset* of its
    // table's columns would widen to the whole table once flattened.
    if exposed.is_some() && query.columns.iter().any(|c| matches!(c, SelectItem::Star)) {
        return false;
    }

    let alias = alias.clone();
    let mut names = Vec::new();
    collect_column_names(query, &mut names);
    for name in &names {
        let (qualifier, col) = split_qualified(name);
        // A qualifier naming something other than the subquery's alias
        // can only come from a `JOIN`ed table, which flattening leaves
        // exactly where it was.
        if qualifier.is_some_and(|q| !q.eq_ignore_ascii_case(&alias)) {
            continue;
        }
        if let Some(exposed) = &exposed {
            if !exposed.iter().any(|c| c.eq_ignore_ascii_case(col)) {
                return false;
            }
        }
    }

    let inner = inner.clone();
    rewrite_column_names(query, &mut |name| strip_alias(name, &alias));
    query.from = inner.from;
    query.where_clause = and_exprs(inner.where_clause, query.where_clause.take());
    true
}

/// Whether `inner`'s own shape rules out merging it into the enclosing
/// query: a `JOIN` of its own, `DISTINCT`, an aggregate/`GROUP BY`/
/// `HAVING`, or a `LIMIT`/`OFFSET`/`ORDER BY` would all change which
/// rows (or how many) survive when the enclosing `WHERE` is applied in
/// the same pass instead of afterwards.
fn subquery_flatten_safe(inner: &Query) -> bool {
    !inner.distinct
        && inner.joins.is_empty()
        && inner.group_by.is_empty()
        && inner.having.is_none()
        && inner.order_by.is_none()
        && inner.limit.is_none()
        && inner.offset.is_none()
        && matches!(inner.from, FromClause::Table(_))
        && inner
            .columns
            .iter()
            .all(|c| matches!(c, SelectItem::Column(_) | SelectItem::Star))
}

/// The column names `inner` exposes, or `None` for a bare `SELECT *`
/// (any name passes through unchanged) -- the reference's `ColumnMap`,
/// narrowed to db-core's alias-free `SelectItem`.
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

pub(super) fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.find('.') {
        Some(idx) => (Some(&name[..idx]), &name[idx.saturating_add(1)..]),
        None => (None, name),
    }
}

/// Drops a leading `alias.` qualifier: once flattened there is no such
/// alias to qualify against, and an unqualified name resolves to the
/// `FROM` table, which is exactly the table the subquery scanned.
fn strip_alias(name: &str, alias: &str) -> String {
    match split_qualified(name) {
        (Some(q), col) if q.eq_ignore_ascii_case(alias) => col.to_string(),
        _ => name.to_string(),
    }
}

fn and_exprs(a: Option<Expr>, b: Option<Expr>) -> Option<Expr> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Expr::BinaryOp(Box::new(a), BinOp::And, Box::new(b))),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// Every column name the *enclosing* query mentions, excluding anything
/// inside a nested subquery expression (which has its own scope).
pub(super) fn collect_column_names(query: &Query, out: &mut Vec<String>) {
    for item in &query.columns {
        if let SelectItem::Column(name) = item {
            out.push(name.clone());
        }
    }
    for join in &query.joins {
        out.push(join.left_col.clone());
        out.push(join.right_col.clone());
    }
    for name in &query.group_by {
        out.push(name.clone());
    }
    if let Some(order_by) = &query.order_by {
        out.push(order_by.column.clone());
    }
    for expr in [query.where_clause.as_ref(), query.having.as_ref()]
        .into_iter()
        .flatten()
    {
        collect_expr_column_names(expr, out);
    }
}

pub(super) fn collect_expr_column_names(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Column(name) => out.push(name.clone()),
        Expr::Literal(_) | Expr::Exists { .. } => {}
        Expr::BinaryOp(lhs, _, rhs) => {
            collect_expr_column_names(lhs, out);
            collect_expr_column_names(rhs, out);
        }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::IsNull { expr: inner, .. } => {
            collect_expr_column_names(inner, out);
        }
        Expr::InSubquery { expr, .. } => collect_expr_column_names(expr, out),
    }
}

fn rewrite_column_names(query: &mut Query, f: &mut impl FnMut(&str) -> String) {
    for item in &mut query.columns {
        if let SelectItem::Column(name) = item {
            *name = f(name);
        }
    }
    for join in &mut query.joins {
        join.left_col = f(&join.left_col);
        join.right_col = f(&join.right_col);
    }
    for name in &mut query.group_by {
        *name = f(name);
    }
    if let Some(order_by) = &mut query.order_by {
        order_by.column = f(&order_by.column);
    }
    if let Some(expr) = &mut query.where_clause {
        rewrite_expr_column_names(expr, f);
    }
    if let Some(expr) = &mut query.having {
        rewrite_expr_column_names(expr, f);
    }
}

fn rewrite_expr_column_names(expr: &mut Expr, f: &mut impl FnMut(&str) -> String) {
    match expr {
        Expr::Column(name) => *name = f(name),
        Expr::Literal(_) | Expr::Exists { .. } => {}
        Expr::BinaryOp(lhs, _, rhs) => {
            rewrite_expr_column_names(lhs, f);
            rewrite_expr_column_names(rhs, f);
        }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::IsNull { expr: inner, .. } => {
            rewrite_expr_column_names(inner, f);
        }
        Expr::InSubquery { expr, .. } => rewrite_expr_column_names(expr, f),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Query {
        crate::parser::column::parse(sql).unwrap()
    }

    #[test]
    fn flattens_a_plain_projection_subquery_and_conjoins_both_wheres() {
        let mut query = parse("SELECT b FROM (SELECT a, b FROM t WHERE a > 1) x WHERE x.b < 9");
        assert!(flatten_from_subquery(&mut query));
        assert_eq!(query.from, FromClause::Table("t".to_string()));
        let Some(Expr::BinaryOp(lhs, BinOp::And, rhs)) = &query.where_clause else {
            panic!("expected a conjunction, got {:?}", query.where_clause);
        };
        // The subquery's own predicate comes first, the enclosing one
        // second, matching the reference's `and_exprs` order.
        assert!(matches!(**lhs, Expr::BinaryOp(_, BinOp::Gt, _)));
        assert!(matches!(**rhs, Expr::BinaryOp(_, BinOp::Lt, _)));
    }

    #[test]
    fn flattening_strips_the_subquery_alias_from_every_reference() {
        let mut query = parse("SELECT x.b FROM (SELECT a, b FROM t) x WHERE x.a = 1");
        assert!(flatten_from_subquery(&mut query));
        assert_eq!(query.columns, vec![SelectItem::Column("b".to_string())]);
        let mut names = Vec::new();
        collect_column_names(&query, &mut names);
        assert!(names.iter().all(|n| !n.contains('.')), "{names:?}");
    }

    #[test]
    fn a_plain_table_from_is_left_alone() {
        let mut query = parse("SELECT a FROM t");
        assert!(!flatten_from_subquery(&mut query));
        assert_eq!(query.from, FromClause::Table("t".to_string()));
    }

    #[test]
    fn does_not_flatten_a_subquery_with_limit_or_group_by() {
        for sql in [
            "SELECT b FROM (SELECT b FROM t LIMIT 1) x",
            "SELECT b FROM (SELECT b FROM t GROUP BY b) x",
        ] {
            let mut query = parse(sql);
            assert!(!flatten_from_subquery(&mut query), "{sql}");
        }
    }

    #[test]
    fn does_not_flatten_a_star_over_a_narrowing_subquery() {
        let mut query = parse("SELECT * FROM (SELECT b FROM t) x");
        assert!(!flatten_from_subquery(&mut query));
    }

    #[test]
    fn does_not_flatten_when_a_reference_is_not_exposed_by_the_subquery() {
        let mut query = parse("SELECT a FROM (SELECT b FROM t) x");
        assert!(!flatten_from_subquery(&mut query));
    }
}
