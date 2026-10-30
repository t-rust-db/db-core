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
use crate::parser::ast::{CommonTableExpr, ResultColumn, Select, TableRef, TableRefKind};

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
}

impl Expansion {
    pub(super) fn new() -> Self {
        Expansion {
            bytes_left: MAX_EXPANSION_BYTES,
            depth: 0,
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
    expand_with_clause_within(select, &mut Expansion::new())
}

/// [`expand_with_clause`] charging `expansion` -- shared with view
/// expansion, whose bodies may carry their own `WITH` clause.
pub(super) fn expand_with_clause_within<'a>(
    select: &'a Select,
    expansion: &mut Expansion,
) -> Result<Cow<'a, Select>, CodegenError> {
    let Some(with) = &select.with_clause else {
        return Ok(Cow::Borrowed(select));
    };
    let mut out = select.clone();
    out.with_clause = None;
    substitute_cte_refs(&mut out, &with.ctes, expansion)?;
    Ok(Cow::Owned(out))
}

/// Substitutes every `FROM`/`JOIN` table reference in `select`'s own
/// main `FROM` clause and each `UNION ALL` compound arm's `FROM` clause
/// that names one of `ctes` (the CTEs visible at this point) with a
/// `TableRefKind::Subquery` wrapping that CTE's query. Does not recurse
/// into subquery *expressions* (scalar/`IN`/`EXISTS`) — a CTE is only
/// visible in `FROM`/`JOIN` position in this pass, matching how #257's
/// subquery-in-FROM support is itself scoped.
fn substitute_cte_refs(
    select: &mut Select,
    ctes: &[CommonTableExpr],
    expansion: &mut Expansion,
) -> Result<(), CodegenError> {
    if let Some(from) = &mut select.from {
        substitute_table_ref(&mut from.first, ctes, expansion)?;
        for join in &mut from.joins {
            substitute_table_ref(&mut join.table, ctes, expansion)?;
        }
    }
    for arm in &mut select.compound {
        if let Some(from) = &mut arm.from {
            substitute_table_ref(&mut from.first, ctes, expansion)?;
            for join in &mut from.joins {
                substitute_table_ref(&mut join.table, ctes, expansion)?;
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
            let Some((index, cte)) = ctes
                .iter()
                .enumerate()
                .find(|(_, c)| c.name.eq_ignore_ascii_case(name))
            else {
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
            let mut query = (*cte.query).clone();
            substitute_cte_refs(&mut query, earlier, expansion)?;
            expansion.leave();
            if let Some(columns) = &cte.columns {
                apply_column_aliases(&mut query, columns);
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
        TableRefKind::Unnest { .. } => Ok(()),
    }
}

/// Renames a CTE's own result columns to its explicit `(col, ...)` list
/// by giving each result column an explicit alias — the synthetic
/// schema #257's `subquery_output_columns` builds for a materialized
/// `FROM`-subquery already prefers an explicit alias over any name it
/// would otherwise derive, so this is the only hook needed to honor
/// `WITH cte(a, b) AS (...)`. Only a same-length, all-`Expr` (no `*`/
/// `table.*`) result-column list can be renamed positionally; anything
/// else is left alone (the CTE still compiles, just exposed under its
/// query's own natural column names instead of the declared list).
pub(super) fn apply_column_aliases(query: &mut Select, columns: &[String]) {
    if query.columns.len() != columns.len() {
        return;
    }
    if query
        .columns
        .iter()
        .any(|c| !matches!(c, ResultColumn::Expr { .. }))
    {
        return;
    }
    for (col, name) in query.columns.iter_mut().zip(columns) {
        if let ResultColumn::Expr { alias, .. } = col {
            *alias = Some(name.clone());
        }
    }
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
