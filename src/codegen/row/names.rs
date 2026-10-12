//! Result column names as sqlite3 reports them (#596).
//!
//! sqlite3 names a result column:
//!
//! - by its `AS` alias, if it has one;
//! - for a column reference (qualified, quoted or parenthesized), by the
//!   name the column was **declared** with in the table it resolves to
//!   (`SELECT abc FROM t` is `Abc` when `t` declares `Abc`), or by the
//!   name of the subquery, CTE or view column it resolves to; `rowid`,
//!   `oid` and `_rowid_` by the table's `INTEGER PRIMARY KEY` column, or
//!   `rowid` without one;
//! - for any other expression, by its **source text** as written: case,
//!   spacing and comments kept (`COUNT( * )`, `abc /*c*/ + 1`, `'lit'`).
//!
//! `*` and `t.*` expand to the sources' column names, and `*` lists a
//! column joined by `USING`/`NATURAL` once. A compound `SELECT` takes its
//! first arm's names.
//!
//! The columns of a subquery, CTE or view are named by a slightly
//! different rule: a column reference keeps the name as written (`SELECT
//! ABC FROM t` gives `ABC`), and a repeated name gets a `:1`, `:2`, ...
//! suffix. An explicit column list (`WITH c(z)`, `CREATE VIEW v(p, q)`)
//! wins.
//!
//! Expression source text comes from each expression's span into the
//! statement's SQL, so callers pass the text the `Select` was parsed
//! from.

use crate::parser::ast::{
    Expr, ExprKind, FromClause, Join, JoinConstraint, ResultColumn, Select, TableRefKind,
};
use crate::parser::row::{parse_create_view, ParseOutcome};
use crate::schema::{TableSchema, ViewSchema};

/// The names a rowid can be referenced by.
const ROWID_NAMES: [&str; 3] = ["rowid", "oid", "_rowid_"];

/// How deep views may nest before naming gives up on them (a view that
/// references itself is an error elsewhere; here it only must not loop).
const MAX_DEPTH: usize = 32;

/// One `FROM` source as names see it.
#[derive(Debug, Clone, Default)]
struct Source {
    /// The alias, or the table, CTE or view name; empty for an unaliased
    /// subquery.
    name: String,
    /// Column names, in order.
    columns: Vec<String>,
    /// What a `rowid` reference is named: the `INTEGER PRIMARY KEY`
    /// column, `rowid`, or nothing for a source without a rowid.
    rowid: Option<String>,
    /// Columns `*` leaves out: joined by `USING`/`NATURAL` to an earlier
    /// source.
    hidden_from_star: Vec<String>,
}

/// A CTE in scope, with its column names.
#[derive(Debug, Clone)]
struct Cte {
    name: String,
    columns: Vec<String>,
}

/// What a name is computed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The statement's own result columns.
    Result,
    /// The columns of a subquery, CTE or view.
    Derived,
}

/// The result column names of `select`, parsed from `sql`, as sqlite3
/// reports them; see the [module docs](self).
pub fn result_column_names(
    select: &Select,
    sql: &str,
    schemas: &[TableSchema],
    views: &[ViewSchema],
) -> Vec<String> {
    names(select, sql, schemas, views, &[], Mode::Result, 0)
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The names of `select`'s result columns, with `outer` CTEs in scope.
fn names(
    select: &Select,
    sql: &str,
    schemas: &[TableSchema],
    views: &[ViewSchema],
    outer: &[Cte],
    mode: Mode,
    depth: usize,
) -> Vec<String> {
    let ctes = ctes_in_scope(select, sql, schemas, views, outer, depth);
    let sources = select
        .from
        .as_ref()
        .map(|from| from_sources(from, sql, schemas, views, &ctes, depth))
        .unwrap_or_default();
    let mut out = Vec::new();
    for column in &select.columns {
        match column {
            ResultColumn::Star => {
                for source in &sources {
                    out.extend(
                        source
                            .columns
                            .iter()
                            .filter(|c| !source.hidden_from_star.iter().any(|h| eq(h, c)))
                            .cloned(),
                    );
                }
            }
            ResultColumn::TableStar { table } => {
                if let Some(source) = sources.iter().find(|s| eq(&s.name, table)) {
                    out.extend(source.columns.iter().cloned());
                }
            }
            ResultColumn::Expr { expr, alias } => out.push(match alias {
                Some(alias) => alias.clone(),
                None => expr_name(expr, sql, &sources, mode),
            }),
        }
    }
    if mode == Mode::Derived {
        dedupe(&mut out);
    }
    out
}

/// `select`'s CTEs, each named with the ones before it (and `outer`) in
/// scope, after `outer`.
fn ctes_in_scope(
    select: &Select,
    sql: &str,
    schemas: &[TableSchema],
    views: &[ViewSchema],
    outer: &[Cte],
    depth: usize,
) -> Vec<Cte> {
    let mut ctes = outer.to_vec();
    for cte in select.with_clause.iter().flat_map(|w| w.ctes.iter()) {
        let columns = cte.columns.clone().unwrap_or_else(|| {
            names(
                &cte.query,
                sql,
                schemas,
                views,
                &ctes,
                Mode::Derived,
                depth.saturating_add(1),
            )
        });
        ctes.push(Cte {
            name: cte.name.clone(),
            columns,
        });
    }
    ctes
}

/// The sources of a `FROM` clause, in order, with `*`'s join coalescing.
fn from_sources(
    from: &FromClause,
    sql: &str,
    schemas: &[TableSchema],
    views: &[ViewSchema],
    ctes: &[Cte],
    depth: usize,
) -> Vec<Source> {
    let mut sources = vec![source(&from.first, sql, schemas, views, ctes, depth)];
    for join in &from.joins {
        let mut next = source(&join.table, sql, schemas, views, ctes, depth);
        next.hidden_from_star = joined_columns(join, &next, &sources);
        sources.push(next);
    }
    sources
}

/// The columns of `right` that `join` matches to earlier sources: its
/// `USING` list, or for `NATURAL` every column an earlier source has too.
fn joined_columns(join: &Join, right: &Source, left: &[Source]) -> Vec<String> {
    if let Some(JoinConstraint::Using(columns)) = &join.constraint {
        return columns.clone();
    }
    if !join.natural {
        return Vec::new();
    }
    right
        .columns
        .iter()
        .filter(|c| left.iter().any(|s| s.columns.iter().any(|l| eq(l, c))))
        .cloned()
        .collect()
}

/// One `FROM` entry as a [`Source`].
fn source(
    table_ref: &crate::parser::ast::TableRef,
    sql: &str,
    schemas: &[TableSchema],
    views: &[ViewSchema],
    ctes: &[Cte],
    depth: usize,
) -> Source {
    let (name, columns, rowid) = match &table_ref.kind {
        TableRefKind::Subquery(query) => (
            String::new(),
            names(
                query,
                sql,
                schemas,
                views,
                ctes,
                Mode::Derived,
                depth.saturating_add(1),
            ),
            None,
        ),
        TableRefKind::Name(name) => {
            let (columns, rowid) = named_source(name, schemas, views, ctes, depth);
            (name.clone(), columns, rowid)
        }
    };
    Source {
        name: table_ref.alias.clone().unwrap_or(name),
        columns,
        rowid,
        hidden_from_star: Vec::new(),
    }
}

/// The columns and rowid name of a source referenced by name: a CTE
/// (innermost first), a view, or a table. An unknown name has none.
fn named_source(
    name: &str,
    schemas: &[TableSchema],
    views: &[ViewSchema],
    ctes: &[Cte],
    depth: usize,
) -> (Vec<String>, Option<String>) {
    if let Some(cte) = ctes.iter().rev().find(|c| eq(&c.name, name)) {
        return (cte.columns.clone(), None);
    }
    if let Some(view) = views.iter().find(|v| eq(&v.name, name)) {
        return (view_columns(view, schemas, views, depth), None);
    }
    match schemas.iter().find(|s| eq(&s.name, name)) {
        Some(schema) => {
            let rowid = if schema.without_rowid {
                None
            } else {
                Some(
                    schema
                        .rowid_alias
                        .and_then(|i| schema.columns.get(i))
                        .cloned()
                        .unwrap_or_else(|| "rowid".to_string()),
                )
            };
            (schema.columns.clone(), rowid)
        }
        None => (Vec::new(), None),
    }
}

/// A view's column names: its explicit list, or its query's, named in
/// the view's own SQL text.
fn view_columns(
    view: &ViewSchema,
    schemas: &[TableSchema],
    views: &[ViewSchema],
    depth: usize,
) -> Vec<String> {
    if depth >= MAX_DEPTH {
        return Vec::new();
    }
    match parse_create_view(&view.sql) {
        ParseOutcome::Accepted(create) => create.columns.unwrap_or_else(|| {
            names(
                &create.query,
                &view.sql,
                schemas,
                views,
                &[],
                Mode::Derived,
                depth.saturating_add(1),
            )
        }),
        ParseOutcome::Unsupported { .. } | ParseOutcome::Invalid { .. } => Vec::new(),
    }
}

/// A result column's name when it has no alias.
fn expr_name(expr: &Expr, sql: &str, sources: &[Source], mode: Mode) -> String {
    let mut inner = expr;
    while let ExprKind::Paren(e) = &inner.kind {
        inner = e;
    }
    if let ExprKind::Column { table, name, .. } = &inner.kind {
        return match mode {
            Mode::Derived => name.clone(),
            Mode::Result => {
                resolve(sources, table.as_deref(), name).unwrap_or_else(|| name.clone())
            }
        };
    }
    source_text(expr, sql)
}

/// The name of the column `table.name` resolves to: a real column of the
/// first source that has one, else a rowid alias.
fn resolve(sources: &[Source], table: Option<&str>, name: &str) -> Option<String> {
    let candidates: Vec<&Source> = sources
        .iter()
        .filter(|s| table.is_none_or(|t| eq(&s.name, t)))
        .collect();
    let column = candidates
        .iter()
        .find_map(|s| s.columns.iter().find(|c| eq(c, name)).cloned());
    column.or_else(|| {
        ROWID_NAMES
            .iter()
            .any(|r| eq(r, name))
            .then(|| candidates.iter().find_map(|s| s.rowid.clone()))
            .flatten()
    })
}

/// `expr`'s text in `sql`, as written.
fn source_text(expr: &Expr, sql: &str) -> String {
    let start = usize::try_from(expr.span.offset).unwrap_or(usize::MAX);
    let end = start.saturating_add(usize::try_from(expr.span.len).unwrap_or(0));
    sql.get(start..end).unwrap_or_default().to_string()
}

/// Renames repeated names `name:1`, `name:2`, ... (case-insensitive),
/// as sqlite3 names a derived table's columns.
fn dedupe(names: &mut [String]) {
    let mut seen: Vec<String> = Vec::new();
    for name in names.iter_mut() {
        let base = name.clone();
        let mut n = 0u32;
        while seen.iter().any(|s| eq(s, name)) {
            n = n.saturating_add(1);
            *name = format!("{base}:{n}");
        }
        seen.push(name.clone());
    }
}

#[cfg(test)]
mod tests;
