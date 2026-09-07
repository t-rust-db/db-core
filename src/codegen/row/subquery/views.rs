//! `CREATE VIEW` expansion (db-core#206) -- see `super`'s module doc.
//!
//! Mirrors [`super::cte`]'s non-recursive `WITH`-clause expansion
//! almost exactly, but for the schema catalog's [`ViewSchema`] rows
//! instead of a query-local `WITH` clause: every `FROM`/`JOIN` table
//! reference naming a view becomes a [`TableRefKind::Subquery`]
//! wrapping that view's stored `SELECT`, so the rest of codegen
//! materializes and scans it exactly like any other `FROM`-subquery.
//!
//! Ported near-verbatim from sqlite-rs's `src/codegen/subquery/
//! views.rs`, adapted to this crate's `ViewSchema` (db-core's own copy
//! per ADR 0012 -- `db_storage::row::schema::ViewSchema` isn't
//! available here, ADR 0008 forbids the dependency).

use crate::codegen::row::{CodegenError, Result, ViewSchema};
use crate::parser::ast::{Select, TableRef, TableRefKind};
use crate::parser::row::error::{parse_create_view, ParseOutcome};

/// A view resolved from its stored `CREATE VIEW ... AS <select>` text
/// into a ready-to-substitute query body.
#[derive(Debug, Clone)]
pub struct ResolvedView {
    /// The view's name.
    pub name: String,
    /// The view's body.
    pub query: Box<Select>,
}

/// Parses every view's stored `sql` back into a [`ResolvedView`]. A row
/// whose `sql` doesn't parse as a `CREATE VIEW` (shouldn't happen for
/// anything this crate's own DDL codegen wrote, but the catalog is an
/// untrusted input as far as this pass is concerned) is silently
/// dropped -- a query referencing it then fails to resolve the name at
/// the ordinary "no such table" path, rather than this pass surfacing a
/// parse error for a view nothing in the query actually needs.
pub fn resolve_views(views: &[ViewSchema]) -> Vec<ResolvedView> {
    views
        .iter()
        .filter_map(|view| match parse_create_view(&view.sql) {
            ParseOutcome::Accepted(create) => Some(ResolvedView {
                name: view.name.clone(),
                query: create.query,
            }),
            _ => None,
        })
        .collect()
}

/// Expands away every view reference in `select`'s own main `FROM`
/// clause and each compound arm's `FROM` clause, in place. Does not
/// recurse into subquery *expressions* (scalar/`IN`/`EXISTS`) -- a view
/// is only visible in `FROM`/`JOIN` position, matching how #95's
/// subquery-in-FROM support and [`super::cte::expand_with_clause`] are
/// themselves scoped.
pub fn expand_views(select: &mut Select, views: &[ResolvedView]) -> Result<()> {
    let mut seen = Vec::new();
    substitute_view_refs(select, views, &mut seen)
}

fn substitute_view_refs(
    select: &mut Select,
    views: &[ResolvedView],
    seen: &mut Vec<String>,
) -> Result<()> {
    if let Some(from) = &mut select.from {
        substitute_table_ref(&mut from.first, views, seen)?;
        for join in &mut from.joins {
            substitute_table_ref(&mut join.table, views, seen)?;
        }
    }
    for arm in &mut select.compound {
        if let Some(from) = &mut arm.from {
            substitute_table_ref(&mut from.first, views, seen)?;
            for join in &mut from.joins {
                substitute_table_ref(&mut join.table, views, seen)?;
            }
        }
    }
    Ok(())
}

fn substitute_table_ref(
    table_ref: &mut TableRef,
    views: &[ResolvedView],
    seen: &mut Vec<String>,
) -> Result<()> {
    match &mut table_ref.kind {
        TableRefKind::Name(name) => {
            let Some(view) = views.iter().find(|v| v.name.eq_ignore_ascii_case(name)) else {
                return Ok(());
            };
            if seen.iter().any(|s| s.eq_ignore_ascii_case(&view.name)) {
                return Err(CodegenError::CircularView(view.name.clone()));
            }
            // A subquery-in-FROM's alias is mandatory to the rest of
            // codegen (see `resolve_from_table_schema`'s doc comment)
            // -- default it to the view's own name when the reference
            // didn't supply one (the common `FROM view_name` case, as
            // opposed to `FROM view_name AS v`).
            let alias = table_ref.alias.clone().or_else(|| Some(view.name.clone()));
            let mut body = (*view.query).clone();
            seen.push(view.name.clone());
            substitute_view_refs(&mut body, views, seen)?;
            seen.pop();
            table_ref.kind = TableRefKind::Subquery(Box::new(body));
            table_ref.alias = alias;
            Ok(())
        }
        // An inline derived table (`FROM (SELECT ... FROM v) sub`) can
        // itself reference a view in its own FROM -- recurse into it
        // the same way CTE expansion would.
        TableRefKind::Subquery(inner) => substitute_view_refs(inner, views, seen),
    }
}
