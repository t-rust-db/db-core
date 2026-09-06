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

use crate::parser::ast::{Expr, ExprKind, ResultColumn, Select, TableRefKind};

/// Flattens `query`'s `FROM`-subquery in place when it is safe to,
/// returning whether it did. Idempotent: a `query` whose `FROM` is
/// already a plain table is left untouched.
pub fn flatten_from_subquery(query: &mut Select) -> bool {
    let Some(from) = &query.from else {
        return false;
    };
    // Flattening merges the subquery's `FROM` into the enclosing one,
    // which only works while the enclosing `FROM` is that subquery and
    // nothing else.
    if !from.joins.is_empty() {
        return false;
    }
    let TableRefKind::Subquery(inner) = &from.first.kind else {
        return false;
    };
    let Some(alias) = from.first.alias.as_ref() else {
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
    if exposed.is_some()
        && query
            .columns
            .iter()
            .any(|c| matches!(c, ResultColumn::Star))
    {
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
fn subquery_flatten_safe(inner: &Select) -> bool {
    !super::super::is_distinct(inner)
        && super::super::joins_of(inner).is_empty()
        && inner.group_by.is_empty()
        && inner.having.is_none()
        && inner.order_by.is_empty()
        && inner.limit.is_none()
        // `WITH`/compound bodies have their own scope and row shape;
        // neither is representable in the enclosing query after a merge.
        && inner.with_clause.is_none()
        && inner.compound.is_empty()
        && inner
            .from
            .as_ref()
            .is_some_and(|from| matches!(from.first.kind, TableRefKind::Name(_)))
        && inner.columns.iter().all(|c| match c {
            ResultColumn::Star => true,
            ResultColumn::Expr { expr, .. } => {
                matches!(expr.kind, ExprKind::Column { .. })
            }
            ResultColumn::TableStar { .. } => false,
        })
}

/// The column names `inner` exposes, or `None` for a bare `SELECT *`
/// (any name passes through unchanged) -- the reference's `ColumnMap`,
/// narrowed to db-core's alias-free `SelectItem`.
fn exposed_columns(inner: &Select) -> Option<Option<Vec<String>>> {
    if inner
        .columns
        .iter()
        .any(|c| matches!(c, ResultColumn::Star))
    {
        return Some(None);
    }
    let mut out = Vec::with_capacity(inner.columns.len());
    for col in &inner.columns {
        let ResultColumn::Expr { expr, .. } = col else {
            return None;
        };
        let ExprKind::Column { name, .. } = &expr.kind else {
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
        (Some(a), Some(b)) => Some(super::super::and_expr(a, b)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// Every column name the *enclosing* query mentions, excluding anything
/// inside a nested subquery expression (which has its own scope).
pub(super) fn collect_column_names(query: &Select, out: &mut Vec<String>) {
    let mut push = |name: &str| out.push(name.to_string());
    for item in &query.columns {
        if let ResultColumn::Expr { expr, .. } = item {
            super::super::walk_columns(expr, &mut push);
        }
    }
    for join in super::super::joins_of(query) {
        // The AST carries the join's real `ON <expr>` rather than a
        // pair of column names (#147), so its columns are collected by
        // the same expression walk as everything else.
        if let Some(crate::parser::ast::JoinConstraint::On(expr)) = &join.constraint {
            super::super::walk_columns(expr, &mut push);
        }
    }
    for expr in &query.group_by {
        super::super::walk_columns(expr, &mut push);
    }
    for term in &query.order_by {
        super::super::walk_columns(&term.expr, &mut push);
    }
    for expr in [query.where_clause.as_ref(), query.having.as_ref()]
        .into_iter()
        .flatten()
    {
        super::super::walk_columns(expr, &mut push);
    }
}

pub(super) fn collect_expr_column_names(expr: &Expr, out: &mut Vec<String>) {
    super::super::walk_columns(expr, &mut |name| out.push(name.to_string()));
}

fn rewrite_column_names(query: &mut Select, f: &mut impl FnMut(&str) -> String) {
    let mut rename = |name: &mut String| *name = f(name);
    for item in &mut query.columns {
        if let ResultColumn::Expr { expr, .. } = item {
            super::super::walk_columns_mut(expr, &mut rename);
        }
    }
    for join in query
        .from
        .as_mut()
        .map(|from| from.joins.as_mut_slice())
        .unwrap_or_default()
    {
        if let Some(crate::parser::ast::JoinConstraint::On(expr)) = &mut join.constraint {
            super::super::walk_columns_mut(expr, &mut rename);
        }
    }
    for expr in &mut query.group_by {
        super::super::walk_columns_mut(expr, &mut rename);
    }
    for term in &mut query.order_by {
        super::super::walk_columns_mut(&mut term.expr, &mut rename);
    }
    for expr in [query.where_clause.as_mut(), query.having.as_mut()]
        .into_iter()
        .flatten()
    {
        super::super::walk_columns_mut(expr, &mut rename);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Select {
        crate::codegen::row::testutil::select(sql)
    }

    fn is_table(from: Option<&crate::parser::ast::FromClause>, name: &str) -> bool {
        matches!(
            from.map(|f| &f.first.kind),
            Some(TableRefKind::Name(n)) if n == name
        )
    }

    #[test]
    fn flattens_a_plain_projection_subquery_and_conjoins_both_wheres() {
        let mut query = parse("SELECT b FROM (SELECT a, b FROM t WHERE a > 1) x WHERE x.b < 9");
        assert!(flatten_from_subquery(&mut query));
        assert!(is_table(query.from.as_ref(), "t"));
        let Some(Expr {
            kind:
                ExprKind::Binary {
                    op: crate::parser::ast::BinaryOp::And,
                    lhs,
                    rhs,
                },
            ..
        }) = &query.where_clause
        else {
            panic!("expected a conjunction, got {:?}", query.where_clause);
        };
        // The subquery's own predicate comes first, the enclosing one
        // second, matching the reference's `and_exprs` order.
        assert!(matches!(
            lhs.kind,
            ExprKind::Binary {
                op: crate::parser::ast::BinaryOp::Gt,
                ..
            }
        ));
        assert!(matches!(
            rhs.kind,
            ExprKind::Binary {
                op: crate::parser::ast::BinaryOp::Lt,
                ..
            }
        ));
    }

    #[test]
    fn flattening_strips_the_subquery_alias_from_every_reference() {
        let mut query = parse("SELECT x.b FROM (SELECT a, b FROM t) x WHERE x.a = 1");
        assert!(flatten_from_subquery(&mut query));
        assert!(matches!(
            query.columns.as_slice(),
            [ResultColumn::Expr {
                expr: Expr {
                    kind: ExprKind::Column { name, .. },
                    ..
                },
                ..
            }] if name == "b"
        ));
        let mut names = Vec::new();
        collect_column_names(&query, &mut names);
        assert!(names.iter().all(|n| !n.contains('.')), "{names:?}");
    }

    #[test]
    fn a_plain_table_from_is_left_alone() {
        let mut query = parse("SELECT a FROM t");
        assert!(!flatten_from_subquery(&mut query));
        assert!(is_table(query.from.as_ref(), "t"));
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

    /// MC/DC vector (obligation `flatten_44`, the "star over a narrowing
    /// subquery" guard): both leaves true -- the subquery exposes a
    /// column subset *and* the enclosing query is `SELECT *`, so
    /// flattening is declined.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__flatten_44__v1_subset_projection_and_outer_star_declines() {
        let mut query = parse("SELECT * FROM (SELECT b FROM t) x");
        assert!(!flatten_from_subquery(&mut query));
        assert!(!is_table(query.from.as_ref(), "t"));
    }

    /// MC/DC vector (obligation `flatten_44`): leaf B (outer `SELECT *`)
    /// false while leaf A (exposed subset) stays true -- the enclosing
    /// query names its columns, so flattening proceeds. Pairs against
    /// `mcdc__flatten_44__v1_subset_projection_and_outer_star_declines`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__flatten_44__v2_subset_projection_with_named_outer_columns_flattens() {
        let mut query = parse("SELECT b FROM (SELECT b FROM t) x");
        assert!(flatten_from_subquery(&mut query));
        assert!(is_table(query.from.as_ref(), "t"));
    }

    /// MC/DC vector (obligation `flatten_44`): leaf A (`exposed.is_some()`)
    /// false while leaf B (outer `SELECT *`) stays true -- the subquery
    /// is itself a bare `SELECT *` (`exposed_columns` yields `Some(None)`,
    /// i.e. an exposed set of `None`), so the star widens nothing and
    /// flattening proceeds. Pairs against
    /// `mcdc__flatten_44__v1_subset_projection_and_outer_star_declines`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__flatten_44__v3_inner_star_with_outer_star_flattens() {
        let mut query = parse("SELECT * FROM (SELECT * FROM t) x");
        assert!(flatten_from_subquery(&mut query));
        assert!(is_table(query.from.as_ref(), "t"));
    }
}
