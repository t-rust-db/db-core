//! Non-recursive `WITH`-clause expansion (db-core#143) -- see `super`'s
//! module doc.
//!
//! Rather than teaching codegen a second table-materialization path,
//! this rewrites a `WITH` clause away *before* codegen ever sees it:
//! every top-level `FROM` reference (in the main query, and in a later
//! CTE's own body) that names an earlier-or-current CTE becomes a
//! [`FromClause::Subquery`] wrapping that CTE's query -- exactly the
//! shape db-core#95's `FROM`-subquery-in-derived-table machinery
//! ([`super::from_clause::materialize_from_subquery`]) already
//! materializes into an ephemeral table and scans like any other table.
//! Ported from sqlite-rs's `src/codegen/subquery.rs` +
//! `subquery/cte.rs`, at db-core's own scope:
//!
//! - **Only a bare `FROM cte_name`** is rewritten, not a `JOIN`: db-core's
//!   [`crate::expr::Join`] carries a real table name only (`table:
//!   String`), with no subquery form for it to hold a CTE reference in.
//! - **No `(col, ...)` column-rename list.** sqlite-rs's own
//!   `apply_column_aliases` renames the CTE body's own `SELECT`-list
//!   aliases; db-core's [`crate::expr::SelectItem::Column`] has no alias
//!   of its own to rename (unlike sqlite-rs's `ResultColumn::Expr`), so
//!   there is nothing for this port to hook the rename onto. A CTE
//!   declared with an explicit column list is rejected as unsupported
//!   rather than silently exposing the wrong names.
//! - **`WITH RECURSIVE` has no representation** in
//!   [`crate::expr::WithClause`] at all (mirrors sqlite-rs's own parser
//!   rejecting it ahead of this crate ever seeing it) -- nothing here
//!   needs to guard against it.

use crate::codegen::row::{CodegenError, Result};
use crate::expr::{CommonTableExpr, FromClause, Query};

/// Expands away `query.with_clause`, if any, in place -- see this
/// module's doc. A `query` with no `WITH` clause is left untouched.
pub fn expand_with_clause(query: &mut Query) -> Result<()> {
    let Some(with) = query.with_clause.take() else {
        return Ok(());
    };

    // Each CTE is resolved in declaration order, against every CTE
    // declared before it (SQLite's non-recursive `WITH` visibility
    // rule) -- `resolved` accumulates the already-rewritten definitions
    // so a later CTE (or the main query) referencing an earlier one
    // picks up its fully-substituted body.
    let mut resolved: Vec<CommonTableExpr> = Vec::with_capacity(with.ctes.len());
    for cte in with.ctes {
        if cte.columns.is_some() {
            return Err(CodegenError::Unsupported {
                reason: format!(
                    "WITH {}(...) -- an explicit CTE column-rename list is not yet supported",
                    cte.name
                ),
            });
        }
        let mut inner = *cte.query;
        substitute_cte_refs(&mut inner, &resolved)?;
        resolved.push(CommonTableExpr {
            name: cte.name,
            columns: None,
            query: Box::new(inner),
        });
    }

    substitute_cte_refs(query, &resolved)
}

/// Substitutes `query`'s own top-level `FROM` reference, if it names one
/// of `ctes`, with a [`FromClause::Subquery`] wrapping that CTE's query.
/// Recurses into an already-`FromClause::Subquery` `FROM` so an inline
/// derived table (`FROM (SELECT ... FROM cte_name) sub`) can itself
/// reference an earlier-declared CTE, the same way view expansion would.
/// Does not recurse into subquery *expressions* (scalar/`IN`/`EXISTS`)
/// or `JOIN` -- a CTE is only visible in `FROM` position in this pass
/// (see the module doc for `JOIN`'s AST limitation).
fn substitute_cte_refs(query: &mut Query, ctes: &[CommonTableExpr]) -> Result<()> {
    match &mut query.from {
        FromClause::Table(name) => {
            if let Some(cte) = ctes.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
                query.from = FromClause::Subquery(cte.query.clone(), name.clone());
            }
            Ok(())
        }
        FromClause::Subquery(inner, _) => substitute_cte_refs(inner, ctes),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::expr::{SelectItem, WithClause};

    fn base_query(from: &str) -> Query {
        Query {
            columns: vec![SelectItem::Column("a".into())],
            from: from.into(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            with_clause: None,
        }
    }

    #[test]
    fn no_with_clause_leaves_the_query_untouched() {
        let mut query = base_query("t");
        expand_with_clause(&mut query).unwrap();
        assert_eq!(query.from, FromClause::Table("t".into()));
    }

    #[test]
    fn a_from_reference_naming_a_cte_becomes_a_subquery() {
        let mut query = base_query("cte1");
        query.with_clause = Some(WithClause {
            ctes: vec![CommonTableExpr {
                name: "cte1".into(),
                columns: None,
                query: Box::new(base_query("t")),
            }],
        });
        expand_with_clause(&mut query).unwrap();
        let FromClause::Subquery(inner, alias) = &query.from else {
            panic!("expected a FromClause::Subquery, got {:?}", query.from);
        };
        assert_eq!(alias, "cte1");
        assert_eq!(inner.from, FromClause::Table("t".into()));
        assert!(query.with_clause.is_none());
    }

    #[test]
    fn a_from_reference_naming_a_real_table_is_left_alone() {
        let mut query = base_query("t");
        query.with_clause = Some(WithClause {
            ctes: vec![CommonTableExpr {
                name: "cte1".into(),
                columns: None,
                query: Box::new(base_query("u")),
            }],
        });
        expand_with_clause(&mut query).unwrap();
        assert_eq!(query.from, FromClause::Table("t".into()));
    }

    #[test]
    fn a_later_cte_may_reference_an_earlier_one() {
        let mut query = base_query("cte2");
        query.with_clause = Some(WithClause {
            ctes: vec![
                CommonTableExpr {
                    name: "cte1".into(),
                    columns: None,
                    query: Box::new(base_query("t")),
                },
                CommonTableExpr {
                    name: "cte2".into(),
                    columns: None,
                    query: Box::new(base_query("cte1")),
                },
            ],
        });
        expand_with_clause(&mut query).unwrap();
        let FromClause::Subquery(cte2_body, alias) = &query.from else {
            panic!("expected a FromClause::Subquery, got {:?}", query.from);
        };
        assert_eq!(alias, "cte2");
        let FromClause::Subquery(cte1_body, inner_alias) = &cte2_body.from else {
            panic!(
                "expected cte2's own FROM to have resolved cte1, got {:?}",
                cte2_body.from
            );
        };
        assert_eq!(inner_alias, "cte1");
        assert_eq!(cte1_body.from, FromClause::Table("t".into()));
    }

    #[test]
    fn a_cte_compiles_end_to_end_through_compile_select_with_catalog() {
        use crate::codegen::row::select::compile_select_with_catalog;
        use crate::codegen::row::TableSchema;
        use crate::vm::row::Opcode;

        let catalog = vec![TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string()],
            column_types: vec![String::new()],
            rowid_alias: None,
            root_page: 2,
            indexes: Vec::new(),
        }];

        let mut inner = base_query("t");
        inner.where_clause = Some(crate::expr::Expr::BinaryOp(
            Box::new(crate::expr::Expr::Column("a".into())),
            crate::expr::BinOp::Gt,
            Box::new(crate::expr::Expr::Literal(crate::types::Literal::Int(0))),
        ));

        let mut query = base_query("cte1");
        query.with_clause = Some(WithClause {
            ctes: vec![CommonTableExpr {
                name: "cte1".into(),
                columns: None,
                query: Box::new(inner),
            }],
        });

        let program = compile_select_with_catalog(&catalog, &query).unwrap();
        let ops: Vec<Opcode> = program.instructions.iter().map(|i| i.opcode).collect();
        // The CTE's body (`SELECT a FROM t WHERE a > 0`) is flattenable,
        // so it never pays for an ephemeral materialization -- it scans
        // `t` directly with the WHERE folded in, exactly like a
        // hand-written `SELECT a FROM t WHERE a > 0` would.
        assert!(!ops.contains(&Opcode::OpenEphemeral), "{ops:?}");
        assert!(ops.contains(&Opcode::Rewind), "{ops:?}");
    }

    #[test]
    fn an_explicit_column_rename_list_is_unsupported() {
        let mut query = base_query("cte1");
        query.with_clause = Some(WithClause {
            ctes: vec![CommonTableExpr {
                name: "cte1".into(),
                columns: Some(vec!["x".into()]),
                query: Box::new(base_query("t")),
            }],
        });
        let err = expand_with_clause(&mut query).unwrap_err();
        assert!(matches!(err, CodegenError::Unsupported { .. }));
    }
}
