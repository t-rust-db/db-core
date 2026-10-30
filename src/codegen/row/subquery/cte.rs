// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Non-recursive `WITH`-clause materialization (#376). Rather than
//! teaching codegen a second table-materialization path, this rewrites
//! a `WITH` clause away *before* codegen ever sees it: every `FROM`/
//! `JOIN` table reference (in the main query, and in a later CTE's own
//! body) that names an earlier-or-current CTE becomes a
//! `TableRefKind::Subquery` wrapping that CTE's `query` — exactly the
//! shape #257's `FROM`-subquery-in-derived-table machinery
//! (`materialize_from_subquery`) already materializes into an
//! ephemeral table and scans like any other table. A CTE name shadows
//! a same-named real table for the scope of the one `SELECT` that
//! declared it, without touching the catalog: the substitution is
//! purely a local AST rewrite, so it can never leak into a sibling
//! statement.
//!
//! `WITH RECURSIVE` is rejected by the parser already (#375) — nothing
//! here needs to guard against it.

use std::borrow::Cow;

use crate::codegen::row::select::CodegenError;
use crate::parser::ast::{
    CommonTableExpr, Expr, ExprKind, FromClause, FunctionArgs, JoinConstraint, ResultColumn,
    Select, TableRef, TableRefKind,
};

/// Most bytes of CTE/view SQL one statement may inline (#876). Every
/// reference copies the referenced body, so this bounds the expanded AST
/// whatever the reference pattern: a 3,000-CTE chain inlines ~60 KiB, a
/// 15-level chain where each CTE references the previous one twice
/// already needs 32 Ki copies.
const MAX_EXPANSION_BYTES: usize = 256 * 1024;

/// Deepest CTE/view nesting one statement may expand to (#876): codegen
/// recurses once per nested subquery, and a 1,000-CTE chain overflowed a
/// 2 MiB thread's stack.
const MAX_EXPANSION_DEPTH: usize = 250;

/// What CTE and view expansion of one statement has used of
/// [`MAX_EXPANSION_BYTES`] and [`MAX_EXPANSION_DEPTH`].
pub(super) struct Expansion {
    bytes_left: usize,
    depth: usize,
    /// The CTEs whose bodies are being expanded, outermost first: a
    /// reference to one of them from inside its own body is circular.
    active: Vec<String>,
}

impl Expansion {
    pub(super) fn new() -> Self {
        Expansion {
            bytes_left: MAX_EXPANSION_BYTES,
            depth: 0,
            active: Vec::new(),
        }
    }

    /// Accounts for inlining `body` one level below the current one; pair
    /// with [`Self::leave`] once its own references are expanded.
    pub(super) fn enter(&mut self, body: &Select) -> Result<(), CodegenError> {
        let too_large = || CodegenError::ExpansionTooLarge {
            max_bytes: MAX_EXPANSION_BYTES,
            max_depth: MAX_EXPANSION_DEPTH,
        };
        let len = (body.span.len as usize).max(1);
        self.bytes_left = self.bytes_left.checked_sub(len).ok_or_else(too_large)?;
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_EXPANSION_DEPTH {
            return Err(too_large());
        }
        Ok(())
    }

    pub(super) fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
}

/// Rewrites away `select.with_clause`, if any — see the module doc.
/// A `Select` with no `WITH` clause is returned as `Cow::Borrowed`
/// rather than a full AST clone — the common case for a query with no
/// `WITH` clause at all (#590 item 6).
///
/// # Errors
///
/// [`CodegenError::ExpansionTooLarge`] when inlining the CTEs would copy
/// too much SQL or nest too deep (#876).
pub fn expand_with_clause(select: &Select) -> Result<Cow<'_, Select>, CodegenError> {
    if select.with_clause.is_none() {
        return Ok(Cow::Borrowed(select));
    }
    let mut out = select.clone();
    expand_ctes_in(&mut out, &mut Expansion::new())?;
    Ok(Cow::Owned(out))
}

/// Expands `select`'s own `WITH` clause, and any nested one, in place,
/// charging `expansion` -- shared with view expansion, whose bodies may
/// carry their own `WITH` clause.
pub(super) fn expand_ctes_in(
    select: &mut Select,
    expansion: &mut Expansion,
) -> Result<(), CodegenError> {
    substitute_cte_refs(select, &[], expansion)
}

/// Substitutes every table reference to one of `ctes` (the CTEs visible
/// at this point) with a `TableRefKind::Subquery` wrapping that CTE's
/// query: in `select`'s `FROM`/`JOIN`s and compound arms, and in every
/// subquery nested anywhere in its expressions (`IN`, `EXISTS`, scalar),
/// which used to see the real same-named table instead (#885). A nested
/// `WITH` is expanded here too, its CTEs shadowing same-named outer ones.
fn substitute_cte_refs(
    select: &mut Select,
    ctes: &[CommonTableExpr],
    expansion: &mut Expansion,
) -> Result<(), CodegenError> {
    let combined: Vec<CommonTableExpr>;
    let ctes = match select.with_clause.take() {
        Some(with) => {
            combined = ctes.iter().cloned().chain(with.ctes).collect();
            combined.as_slice()
        }
        None => ctes,
    };
    if let Some(from) = &mut select.from {
        substitute_in_from(from, ctes, expansion)?;
    }
    for arm in &mut select.compound {
        if let Some(from) = &mut arm.from {
            substitute_in_from(from, ctes, expansion)?;
        }
        for col in &mut arm.columns {
            if let ResultColumn::Expr { expr, .. } = col {
                substitute_in_expr(expr, ctes, expansion)?;
            }
        }
        for expr in arm
            .where_clause
            .iter_mut()
            .chain(arm.group_by.iter_mut())
            .chain(arm.having.iter_mut())
        {
            substitute_in_expr(expr, ctes, expansion)?;
        }
    }
    for col in &mut select.columns {
        if let ResultColumn::Expr { expr, .. } = col {
            substitute_in_expr(expr, ctes, expansion)?;
        }
    }
    let limit = select
        .limit
        .iter_mut()
        .flat_map(|l| std::iter::once(&mut l.limit).chain(l.offset.iter_mut()));
    for expr in select
        .where_clause
        .iter_mut()
        .chain(select.group_by.iter_mut())
        .chain(select.having.iter_mut())
        .chain(select.order_by.iter_mut().map(|t| &mut t.expr))
        .chain(limit)
    {
        substitute_in_expr(expr, ctes, expansion)?;
    }
    Ok(())
}

fn substitute_in_from(
    from: &mut FromClause,
    ctes: &[CommonTableExpr],
    expansion: &mut Expansion,
) -> Result<(), CodegenError> {
    substitute_table_ref(&mut from.first, ctes, expansion)?;
    for join in &mut from.joins {
        substitute_table_ref(&mut join.table, ctes, expansion)?;
        if let Some(JoinConstraint::On(on)) = &mut join.constraint {
            substitute_in_expr(on, ctes, expansion)?;
        }
    }
    Ok(())
}

/// Substitutes CTE references inside every subquery nested in `expr`.
fn substitute_in_expr(
    expr: &mut Expr,
    ctes: &[CommonTableExpr],
    expansion: &mut Expansion,
) -> Result<(), CodegenError> {
    match &mut expr.kind {
        ExprKind::Literal(_) | ExprKind::Param(_) | ExprKind::Column { .. } => {}
        ExprKind::Subquery(subquery) | ExprKind::Exists { subquery, .. } => {
            substitute_cte_refs(subquery, ctes, expansion)?;
        }
        ExprKind::InSubquery { expr, subquery, .. } => {
            substitute_in_expr(expr, ctes, expansion)?;
            substitute_cte_refs(subquery, ctes, expansion)?;
        }
        ExprKind::InSubqueryMulti {
            exprs, subquery, ..
        } => {
            for e in exprs {
                substitute_in_expr(e, ctes, expansion)?;
            }
            substitute_cte_refs(subquery, ctes, expansion)?;
        }
        ExprKind::Unary { expr, .. }
        | ExprKind::IsNull { expr, .. }
        | ExprKind::Cast { expr, .. }
        | ExprKind::Collate { expr, .. }
        | ExprKind::Paren(expr) => substitute_in_expr(expr, ctes, expansion)?,
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            substitute_in_expr(lhs, ctes, expansion)?;
            substitute_in_expr(rhs, ctes, expansion)?;
        }
        ExprKind::Between { expr, lo, hi, .. } => {
            substitute_in_expr(expr, ctes, expansion)?;
            substitute_in_expr(lo, ctes, expansion)?;
            substitute_in_expr(hi, ctes, expansion)?;
        }
        ExprKind::In { expr, list, .. } => {
            substitute_in_expr(expr, ctes, expansion)?;
            for e in list {
                substitute_in_expr(e, ctes, expansion)?;
            }
        }
        ExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            substitute_in_expr(expr, ctes, expansion)?;
            substitute_in_expr(pattern, ctes, expansion)?;
            if let Some(escape) = escape {
                substitute_in_expr(escape, ctes, expansion)?;
            }
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(operand) = operand {
                substitute_in_expr(operand, ctes, expansion)?;
            }
            for (when, then) in whens {
                substitute_in_expr(when, ctes, expansion)?;
                substitute_in_expr(then, ctes, expansion)?;
            }
            if let Some(else_) = else_ {
                substitute_in_expr(else_, ctes, expansion)?;
            }
        }
        ExprKind::FunctionCall { args, tail, .. } => {
            if let FunctionArgs::List(args) = args {
                for e in args {
                    substitute_in_expr(e, ctes, expansion)?;
                }
            }
            if let Some(tail) = tail {
                if let Some(filter) = &mut tail.filter {
                    substitute_in_expr(filter, ctes, expansion)?;
                }
                if let Some(over) = &mut tail.over {
                    for e in &mut over.partition_by {
                        substitute_in_expr(e, ctes, expansion)?;
                    }
                    for term in &mut over.order_by {
                        substitute_in_expr(&mut term.expr, ctes, expansion)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn substitute_table_ref(
    table_ref: &mut TableRef,
    ctes: &[CommonTableExpr],
    expansion: &mut Expansion,
) -> Result<(), CodegenError> {
    match &mut table_ref.kind {
        TableRefKind::Name(name) => {
            // The latest declaration wins: a nested `WITH` is appended
            // after the CTEs it shadows.
            let Some((index, cte)) = ctes
                .iter()
                .enumerate()
                .rev()
                .find(|(_, c)| c.name.eq_ignore_ascii_case(name))
            else {
                // Inside its own body a CTE isn't visible (no `WITH
                // RECURSIVE`), so the name would silently fall through to
                // a same-named real table: sqlite3 refuses it (#885).
                if expansion
                    .active
                    .iter()
                    .any(|active| active.eq_ignore_ascii_case(name))
                {
                    return Err(CodegenError::Rejected(format!(
                        "circular reference: {name}"
                    )));
                }
                return Ok(());
            };
            let earlier = ctes.get(..index).ok_or_else(|| CodegenError::Internal {
                reason: format!("CTE {index} is past the {} declared", ctes.len()),
            })?;
            // Each CTE is resolved against the CTEs declared before it
            // (SQLite's non-recursive `WITH` visibility rule), and only
            // when referenced: expanding every definition up front cloned
            // each earlier, already-expanded body into the next --
            // quadratic in a chain's length (#876).
            expansion.enter(&cte.query)?;
            expansion.active.push(cte.name.clone());
            let mut query = (*cte.query).clone();
            substitute_cte_refs(&mut query, earlier, expansion)?;
            expansion.active.pop();
            expansion.leave();
            if let Some(columns) = &cte.columns {
                apply_column_aliases(&mut query, columns, &cte.name)?;
            }
            // A subquery-in-FROM's alias is mandatory to the rest of
            // codegen (`resolve_from_table_schema`'s doc comment) —
            // default it to the CTE's own name when the reference
            // didn't supply one itself (the common `FROM cte_name`
            // case, as opposed to `FROM cte_name AS c`).
            let alias = table_ref.alias.clone().or_else(|| Some(cte.name.clone()));
            table_ref.kind = TableRefKind::Subquery(Box::new(query));
            table_ref.alias = alias;
            Ok(())
        }
        // An inline derived table (`FROM (SELECT ... FROM cte_name) sub`)
        // can itself reference an earlier-declared CTE in its own FROM —
        // recurse into it the same way view expansion does, so a CTE
        // isn't only resolvable directly under a query's top-level FROM.
        TableRefKind::Subquery(inner) => substitute_cte_refs(inner, ctes, expansion),
        TableRefKind::Unnest { expr, .. } => substitute_in_expr(expr, ctes, expansion),
    }
}

/// Renames a CTE's (or view's) own result columns to its explicit
/// `(col, ...)` list by giving each result column an explicit alias — the
/// synthetic schema #257's `subquery_output_columns` builds for a
/// materialized `FROM`-subquery already prefers an explicit alias over any
/// name it would otherwise derive, so this is the only hook needed to
/// honor `WITH cte(a, b) AS (...)`.
///
/// # Errors
///
/// sqlite3's `table {name} has N values for M columns` when the counts
/// differ, and `Unsupported` for a `*`/`table.*` select list, whose width
/// isn't known here: both used to be skipped silently, leaving the
/// declared names unapplied (#885).
pub(super) fn apply_column_aliases(
    query: &mut Select,
    columns: &[String],
    name: &str,
) -> Result<(), CodegenError> {
    if query
        .columns
        .iter()
        .any(|c| !matches!(c, ResultColumn::Expr { .. }))
    {
        return Err(CodegenError::Unsupported {
            reason: format!("a column list for {name} over a `*` select list"),
        });
    }
    if query.columns.len() != columns.len() {
        return Err(CodegenError::Rejected(format!(
            "table {name} has {} values for {} columns",
            query.columns.len(),
            columns.len()
        )));
    }
    for (col, column_name) in query.columns.iter_mut().zip(columns) {
        if let ResultColumn::Expr { alias, .. } = col {
            *alias = Some(column_name.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::row::{resolve_views, ExpandViews, ViewSchema};
    use crate::parser::row::{parse_select, ParseOutcome};

    fn sel(sql: &str) -> Select {
        match parse_select(sql) {
            ParseOutcome::Accepted(select) => *select,
            other => panic!("{sql:?} must parse, got {other:?}"),
        }
    }

    /// `WITH c0 AS (SELECT a FROM t), c1 AS (<body(c0)>), ... SELECT a FROM c{n-1}`.
    fn chain(n: usize, body: impl Fn(usize) -> String) -> Select {
        let mut sql = "WITH c0 AS (SELECT a FROM t)".to_string();
        for i in 1..n {
            sql += &format!(", c{i} AS ({})", body(i - 1));
        }
        sel(&format!("{sql} SELECT a FROM c{}", n - 1))
    }

    fn too_large<T: std::fmt::Debug>(result: Result<T, CodegenError>) -> bool {
        matches!(result, Err(CodegenError::ExpansionTooLarge { .. }))
    }

    /// Whether `select`'s `FROM` reads a substituted CTE body rather than
    /// a named (real) table.
    fn reads_a_cte(select: &Select) -> bool {
        matches!(
            select.from.as_ref().map(|f| &f.first.kind),
            Some(TableRefKind::Subquery(_))
        )
    }

    /// #885: a CTE shadowing a table was substituted only in `FROM`/`JOIN`,
    /// so an `IN`/`EXISTS`/scalar subquery read the real table -- wrong
    /// rows, silently. Every nested subquery now sees the CTE.
    #[test]
    fn subqueries_in_expressions_see_the_cte() {
        let select = sel(
            "WITH t AS (SELECT 42 AS a) SELECT (SELECT a FROM t) FROM t \
             WHERE a IN (SELECT a FROM t) AND EXISTS (SELECT 1 FROM t) \
             AND CASE WHEN a < (SELECT a FROM t) THEN 1 END",
        );
        let expanded = expand_with_clause(&select).unwrap();
        assert!(reads_a_cte(&expanded));
        let ResultColumn::Expr { expr, .. } = &expanded.columns[0] else {
            panic!("{:?}", expanded.columns)
        };
        let ExprKind::Subquery(scalar) = &expr.kind else {
            panic!("{expr:?}")
        };
        assert!(reads_a_cte(scalar), "scalar subquery");
        let mut subqueries = Vec::new();
        let mut stack = vec![expanded.where_clause.as_ref().unwrap()];
        while let Some(e) = stack.pop() {
            match &e.kind {
                ExprKind::Binary { lhs, rhs, .. } => stack.extend([&**lhs, &**rhs]),
                ExprKind::Case { whens, .. } => stack.extend(whens.iter().map(|(w, _)| w)),
                ExprKind::InSubquery { subquery, .. }
                | ExprKind::Exists { subquery, .. }
                | ExprKind::Subquery(subquery) => subqueries.push(subquery),
                _ => {}
            }
        }
        assert_eq!(subqueries.len(), 3, "IN, EXISTS and the CASE's scalar");
        assert!(subqueries.iter().all(|s| reads_a_cte(s)));
    }

    /// #885: sqlite3 refuses a non-recursive CTE that reads its own name
    /// and a column list of the wrong width; both used to pass silently.
    #[test]
    fn circular_and_misdeclared_ctes_are_refused() {
        let err = |sql: &str| expand_with_clause(&sel(sql)).unwrap_err().to_string();
        assert_eq!(
            err("WITH t AS (SELECT * FROM t) SELECT * FROM t"),
            "circular reference: t"
        );
        assert_eq!(
            err("WITH c(x, y) AS (SELECT 1) SELECT * FROM c"),
            "table c has 1 values for 2 columns"
        );
        assert!(err("WITH c(x) AS (SELECT * FROM t) SELECT x FROM c").contains("`*`"));
    }

    /// #876: a linear chain nests one subquery per CTE. 250 levels is the
    /// cap; past it the statement is refused instead of overflowing
    /// codegen's stack (a 1,000-CTE chain did on a 2 MiB thread).
    #[test]
    fn a_chain_expands_up_to_the_depth_cap() {
        let link = |p: usize| format!("SELECT a FROM c{p}");
        assert!(expand_with_clause(&chain(250, link)).is_ok());
        assert!(too_large(expand_with_clause(&chain(251, link))));
    }

    /// #876: each CTE referencing the previous one twice doubled the
    /// expanded AST per level -- 20 levels took 5.3 GB.
    #[test]
    fn a_chain_that_references_each_cte_twice_is_refused() {
        let double = |p: usize| format!("SELECT x.a FROM c{p} x, c{p} y");
        assert!(expand_with_clause(&chain(8, double)).is_ok());
        assert!(too_large(expand_with_clause(&chain(20, double))));
    }

    /// #876: the same doubling through catalog views.
    #[test]
    fn views_that_reference_each_view_twice_are_refused() {
        let views: Vec<ViewSchema> = (0..20)
            .map(|i| ViewSchema {
                name: format!("v{i}"),
                sql: if i == 0 {
                    "CREATE VIEW v0 AS SELECT a FROM t".to_string()
                } else {
                    format!(
                        "CREATE VIEW v{i} AS SELECT x.a FROM v{p} x, v{p} y",
                        p = i - 1
                    )
                },
            })
            .collect();
        let resolved = resolve_views(&views);
        assert!(sel("SELECT a FROM v6").expand_views(&resolved).is_ok());
        assert!(too_large(sel("SELECT a FROM v19").expand_views(&resolved)));
    }

    /// #876: a 3,000-CTE chain is refused at the depth cap on a 2 MiB
    /// thread, without first building the quadratic expansion (6 s and
    /// 3.9 GB before).
    #[test]
    fn a_very_long_chain_is_refused_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(|| {
                let select = chain(3000, |p| format!("SELECT a FROM c{p}"));
                assert!(too_large(expand_with_clause(&select)));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
