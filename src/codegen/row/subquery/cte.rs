//! Non-recursive `WITH`-clause expansion (db-core#143) -- see `super`'s
//! module doc.
//!
//! Rather than teaching codegen a second table-materialization path,
//! this rewrites a `WITH` clause away *before* codegen ever sees it:
//! every `FROM`/`JOIN` table reference (in the main query, and in a
//! later CTE's own body) that names an earlier-or-current CTE becomes a
//! [`TableRefKind::Subquery`] wrapping that CTE's query -- exactly the
//! shape db-core#95's `FROM`-subquery-in-derived-table machinery
//! ([`super::from_clause::materialize_from_subquery`]) already
//! materializes into an ephemeral table and scans like any other table.
//! Ported near-verbatim from sqlite-rs's `src/codegen/subquery/cte.rs`
//! -- `parser::ast` is sqlite-rs's own AST post-db-core#147, so this
//! pass needs no scoping-down at the AST level, only at the codegen
//! level (see the caveats below and [`super`]'s own module doc for what
//! `materialize_from_subquery` still can't compile).
//!
//! `WITH RECURSIVE` is rejected by the parser already -- nothing here
//! needs to guard against it.

use crate::codegen::row::Result;
use crate::parser::ast::{CommonTableExpr, ResultColumn, Select, TableRef, TableRefKind};

/// Expands away `select.with_clause`, if any, in place -- see this
/// module's doc. A `select` with no `WITH` clause is left untouched.
pub fn expand_with_clause(select: &mut Select) -> Result<()> {
    let Some(with) = select.with_clause.take() else {
        return Ok(());
    };

    // Each CTE is resolved in declaration order, against every CTE
    // declared before it (SQLite's non-recursive `WITH` visibility
    // rule) -- `resolved` accumulates the already-rewritten definitions
    // so a later CTE (or the main query) referencing an earlier one
    // picks up its fully-substituted body.
    let mut resolved: Vec<CommonTableExpr> = Vec::with_capacity(with.ctes.len());
    for cte in with.ctes {
        let mut query = *cte.query;
        substitute_cte_refs(&mut query, &resolved);
        if let Some(columns) = &cte.columns {
            apply_column_aliases(&mut query, columns);
        }
        resolved.push(CommonTableExpr {
            name: cte.name,
            columns: cte.columns,
            query: Box::new(query),
            span: cte.span,
        });
    }

    substitute_cte_refs(select, &resolved);
    Ok(())
}

/// Substitutes every `FROM`/`JOIN` table reference in `select`'s own
/// main `FROM` clause and each `UNION ALL` compound arm's `FROM` clause
/// that names one of `ctes` with a `TableRefKind::Subquery` wrapping
/// that CTE's query. Does not recurse into subquery *expressions*
/// (scalar/`IN`/`EXISTS`) -- a CTE is only visible in `FROM`/`JOIN`
/// position in this pass, matching how db-core#95's subquery-in-FROM
/// support is itself scoped.
fn substitute_cte_refs(select: &mut Select, ctes: &[CommonTableExpr]) {
    if let Some(from) = &mut select.from {
        substitute_table_ref(&mut from.first, ctes);
        for join in &mut from.joins {
            substitute_table_ref(&mut join.table, ctes);
        }
    }
    for arm in &mut select.compound {
        if let Some(from) = &mut arm.from {
            substitute_table_ref(&mut from.first, ctes);
            for join in &mut from.joins {
                substitute_table_ref(&mut join.table, ctes);
            }
        }
    }
}

fn substitute_table_ref(table_ref: &mut TableRef, ctes: &[CommonTableExpr]) {
    match &mut table_ref.kind {
        TableRefKind::Name(name) => {
            let Some(cte) = ctes.iter().find(|c| c.name.eq_ignore_ascii_case(name)) else {
                return;
            };
            // A subquery-in-FROM's alias is mandatory to the rest of
            // codegen (`resolve_from_table_schema`'s doc comment) --
            // default it to the CTE's own name when the reference
            // didn't supply one itself (the common `FROM cte_name`
            // case, as opposed to `FROM cte_name AS c`).
            let alias = table_ref.alias.clone().or_else(|| Some(cte.name.clone()));
            table_ref.kind = TableRefKind::Subquery(cte.query.clone());
            table_ref.alias = alias;
        }
        // An inline derived table (`FROM (SELECT ... FROM cte_name) sub`)
        // can itself reference an earlier-declared CTE in its own FROM --
        // recurse into it the same way view expansion would, so a CTE
        // isn't only resolvable directly under a query's top-level FROM.
        TableRefKind::Subquery(inner) => substitute_cte_refs(inner, ctes),
    }
}

/// Renames a CTE's own result columns to its explicit `(col, ...)` list
/// by giving each result column an explicit alias -- the synthetic
/// schema [`super::from_clause::subquery_output_columns`] builds for a
/// materialized `FROM`-subquery already prefers an explicit alias over
/// any name it would otherwise derive (mirroring the reference), so
/// this is the only hook needed to honor `WITH cte(a, b) AS (...)`.
/// Only a same-length, all-`Expr` (no `*`/`table.*`) result-column list
/// can be renamed positionally; anything else is left alone (the CTE
/// still compiles, just exposed under its query's own natural column
/// names instead of the declared list).
fn apply_column_aliases(query: &mut Select, columns: &[String]) {
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
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::codegen::row::select::compile_select_with_catalog;
    use crate::codegen::row::TableSchema;
    use crate::vm::row::Opcode;

    fn parse(sql: &str) -> Select {
        crate::codegen::row::testutil::select(sql)
    }

    #[test]
    fn no_with_clause_leaves_the_query_untouched() {
        let mut select = parse("SELECT a FROM t");
        expand_with_clause(&mut select).unwrap();
        assert!(select.with_clause.is_none());
        assert_eq!(
            select.from.as_ref().unwrap().first.kind,
            TableRefKind::Name("t".to_string())
        );
    }

    #[test]
    fn a_from_reference_naming_a_cte_becomes_a_subquery() {
        let mut select = parse("WITH cte1 AS (SELECT a FROM t) SELECT a FROM cte1");
        expand_with_clause(&mut select).unwrap();
        assert!(select.with_clause.is_none());
        let TableRefKind::Subquery(inner) = &select.from.as_ref().unwrap().first.kind else {
            panic!(
                "expected a TableRefKind::Subquery, got {:?}",
                select.from.as_ref().unwrap().first.kind
            );
        };
        assert_eq!(
            select.from.as_ref().unwrap().first.alias.as_deref(),
            Some("cte1")
        );
        assert_eq!(
            inner.from.as_ref().unwrap().first.kind,
            TableRefKind::Name("t".to_string())
        );
    }

    #[test]
    fn an_explicit_alias_on_the_cte_reference_is_kept() {
        let mut select = parse("WITH cte1 AS (SELECT a FROM t) SELECT a FROM cte1 AS c");
        expand_with_clause(&mut select).unwrap();
        assert_eq!(
            select.from.as_ref().unwrap().first.alias.as_deref(),
            Some("c")
        );
    }

    #[test]
    fn a_from_reference_naming_a_real_table_is_left_alone() {
        let mut select = parse("WITH cte1 AS (SELECT a FROM u) SELECT a FROM t");
        expand_with_clause(&mut select).unwrap();
        assert_eq!(
            select.from.as_ref().unwrap().first.kind,
            TableRefKind::Name("t".to_string())
        );
    }

    #[test]
    fn a_later_cte_may_reference_an_earlier_one() {
        let mut select = parse(
            "WITH cte1 AS (SELECT a FROM t), cte2 AS (SELECT a FROM cte1) SELECT a FROM cte2",
        );
        expand_with_clause(&mut select).unwrap();
        let TableRefKind::Subquery(cte2_body) = &select.from.as_ref().unwrap().first.kind else {
            panic!("expected a TableRefKind::Subquery");
        };
        let TableRefKind::Subquery(cte1_body) = &cte2_body.from.as_ref().unwrap().first.kind else {
            panic!(
                "expected cte2's own FROM to have resolved cte1, got {:?}",
                cte2_body.from
            );
        };
        assert_eq!(
            cte2_body.from.as_ref().unwrap().first.alias.as_deref(),
            Some("cte1")
        );
        assert_eq!(
            cte1_body.from.as_ref().unwrap().first.kind,
            TableRefKind::Name("t".to_string())
        );
    }

    #[test]
    fn an_explicit_column_rename_list_renames_the_ctes_result_columns() {
        let mut select = parse("WITH cte1(x) AS (SELECT a FROM t) SELECT x FROM cte1");
        expand_with_clause(&mut select).unwrap();
        let TableRefKind::Subquery(inner) = &select.from.as_ref().unwrap().first.kind else {
            panic!("expected a TableRefKind::Subquery");
        };
        let ResultColumn::Expr { alias, .. } = &inner.columns[0] else {
            panic!("expected a ResultColumn::Expr");
        };
        assert_eq!(alias.as_deref(), Some("x"));
    }

    #[test]
    fn a_mismatched_length_column_rename_list_is_left_alone() {
        let mut select = parse("WITH cte1(x, y) AS (SELECT a FROM t) SELECT a FROM cte1");
        expand_with_clause(&mut select).unwrap();
        let TableRefKind::Subquery(inner) = &select.from.as_ref().unwrap().first.kind else {
            panic!("expected a TableRefKind::Subquery");
        };
        let ResultColumn::Expr { alias, .. } = &inner.columns[0] else {
            panic!("expected a ResultColumn::Expr");
        };
        assert_eq!(*alias, None);
    }

    #[test]
    fn a_cte_compiles_end_to_end_through_compile_select_with_catalog() {
        let catalog = vec![TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string()],
            column_types: vec![String::new()],
            rowid_alias: None,
            root_page: 2,
            indexes: Vec::new(),
        }];

        let query = parse("WITH cte1 AS (SELECT a FROM t WHERE a > 0) SELECT a FROM cte1");
        let program = compile_select_with_catalog(&catalog, &query).unwrap();
        let ops: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        // The CTE's body (`SELECT a FROM t WHERE a > 0`) is flattenable,
        // so it never pays for an ephemeral materialization -- it scans
        // `t` directly with the WHERE folded in, exactly like a
        // hand-written `SELECT a FROM t WHERE a > 0` would.
        assert!(!ops.contains(&Opcode::OpenEphemeral), "{ops:?}");
        assert!(ops.contains(&Opcode::Rewind), "{ops:?}");
    }
}
