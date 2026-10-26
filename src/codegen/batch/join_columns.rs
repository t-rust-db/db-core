//! Name resolution for joins (#778): the rule SQLite and DuckDB apply to
//! the columns of a `FROM a JOIN b ...` query, applied before planning so
//! [`super::compile_join`] never has to guess which side a bare name is on.
//!
//! The caller says which columns each table of the query has
//! ([`resolve_join_columns`]); the planner underneath only knows the
//! `FROM` table's. Resolution then:
//!
//! - expands `*` (every table's columns in the order written, a `USING` key
//!   once) and `t.*` (that table's columns) with bare names as headers,
//!   duplicates included (`id`, `id`);
//! - qualifies an unqualified column with the one table that has it, an
//!   [`PlanError::AmbiguousColumn`] when several do (a `USING (k)` key is
//!   merged and means the table written first), and leaves it alone when
//!   none does (a select-list alias, or an unknown name the planner reports).

use super::{output_column_names, resolve_group_by, PlanError, Result};
use crate::parser::ast::{
    Expr, ExprKind, FunctionArgs, Join, JoinConstraint, ResultColumn, Select,
};
use crate::parser::Span;

/// A join's `SELECT` with its names resolved, and the result headers it had
/// before they were.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedJoin {
    /// The select with `*` expanded and every column qualified.
    pub select: Select,
    /// What [`output_column_names`] gave before the columns were qualified:
    /// `SUM(amount)` stays that, not `SUM(production.amount)`.
    pub headers: Vec<String>,
}

struct StarColumn {
    table: String,
    alias: Option<String>,
    column: String,
    /// A `USING` key on the joined side: `*` lists it once, from the table
    /// written first.
    merged: bool,
}

/// Resolves the names of a joined `select` against `tables`, each table of
/// the `FROM`/`JOIN` clauses with its column names (matched by name,
/// ignoring case; a table not listed has no columns). Expands `*`/`t.*`,
/// resolves `GROUP BY` terms naming a select item, then qualifies every
/// column with its table.
///
/// # Errors
///
/// [`PlanError::AmbiguousColumn`] for a bare name several tables have,
/// [`PlanError::UnknownStarTable`] for a `t.*` naming no table of the query.
pub fn resolve_join_columns(
    select: &Select,
    tables: &[(String, Vec<String>)],
) -> Result<ResolvedJoin> {
    let select = expand_star(select.clone(), tables)?;
    let select = if select.group_by.is_empty() {
        select
    } else {
        let schema: Vec<String> = named_tables(&select)
            .iter()
            .flat_map(|name| columns_of(tables, name))
            .collect();
        resolve_group_by(&select, &schema)?
    };
    let headers = output_column_names(&select);
    let select = qualify(select, tables)?;
    Ok(ResolvedJoin { select, headers })
}

/// The named tables of the `FROM`/`JOIN` clauses in the order written.
fn named_tables(select: &Select) -> Vec<&str> {
    select
        .from
        .iter()
        .flat_map(|f| std::iter::once(&f.first).chain(f.joins.iter().map(|j| &j.table)))
        .filter_map(|t| t.name())
        .collect()
}

fn columns_of(tables: &[(String, Vec<String>)], name: &str) -> Vec<String> {
    tables
        .iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(name))
        .map(|(_, columns)| columns.clone())
        .unwrap_or_default()
}

fn using_columns(join: &Join) -> Vec<String> {
    match &join.constraint {
        Some(JoinConstraint::Using(columns)) => columns.clone(),
        _ => Vec::new(),
    }
}

/// The columns `*` and `t.*` stand for: for each table of the query in the
/// order written, its columns (`merged` marks a `USING` key on the joined
/// side, which `*` lists once and `t.*` still lists).
fn star_columns(select: &Select, tables: &[(String, Vec<String>)]) -> Vec<StarColumn> {
    let Some(from) = select.from.as_ref() else {
        return Vec::new();
    };
    let refs = std::iter::once((&from.first, Vec::new()))
        .chain(from.joins.iter().map(|j| (&j.table, using_columns(j))));
    let mut all = Vec::new();
    for (table, keys) in refs {
        let Some(name) = table.name() else {
            continue;
        };
        for column in columns_of(tables, name) {
            let merged = keys.iter().any(|k| k.eq_ignore_ascii_case(&column));
            all.push(StarColumn {
                table: name.to_string(),
                alias: table.alias.clone(),
                column,
                merged,
            });
        }
    }
    all
}

fn expand_star(mut select: Select, tables: &[(String, Vec<String>)]) -> Result<Select> {
    let has_star = select
        .columns
        .iter()
        .any(|c| matches!(c, ResultColumn::Star | ResultColumn::TableStar { .. }));
    if !has_star {
        return Ok(select);
    }
    let all = star_columns(&select, tables);
    let item = |e: &StarColumn| ResultColumn::Expr {
        expr: column_expr(Some(e.table.clone()), e.column.clone()),
        alias: Some(e.column.clone()),
    };
    let mut columns = Vec::with_capacity(select.columns.len());
    for c in std::mem::take(&mut select.columns) {
        match c {
            ResultColumn::Star => columns.extend(all.iter().filter(|e| !e.merged).map(item)),
            ResultColumn::TableStar { table } => {
                let of_table: Vec<ResultColumn> = all
                    .iter()
                    .filter(|e| {
                        [Some(e.table.as_str()), e.alias.as_deref()]
                            .into_iter()
                            .any(|n| n.is_some_and(|n| n.eq_ignore_ascii_case(&table)))
                    })
                    .map(item)
                    .collect();
                if of_table.is_empty() {
                    return Err(PlanError::UnknownStarTable(table));
                }
                columns.extend(of_table);
            }
            other => columns.push(other),
        }
    }
    select.columns = columns;
    Ok(select)
}

fn column_expr(table: Option<String>, name: String) -> Expr {
    Expr {
        kind: ExprKind::Column {
            table,
            catalog: None,
            name,
        },
        span: Span::UNKNOWN,
    }
}

/// The table an unqualified column belongs to: the one table that has it.
/// Several is [`PlanError::AmbiguousColumn`], except for a `USING (k)` key,
/// which is merged and means the table written first. `None` when no table
/// has it.
fn column_owner(
    name: &str,
    tables: &[(String, Vec<String>)],
    using_keys: &[String],
) -> Result<Option<String>> {
    let owners: Vec<&String> = tables
        .iter()
        .filter(|(_, columns)| columns.iter().any(|c| c.eq_ignore_ascii_case(name)))
        .map(|(table, _)| table)
        .collect();
    match owners.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some((*only).clone())),
        [first, ..] if using_keys.iter().any(|k| k.eq_ignore_ascii_case(name)) => {
            Ok(Some((*first).clone()))
        }
        _ => Err(PlanError::AmbiguousColumn {
            name: name.to_string(),
            tables: owners.into_iter().cloned().collect(),
        }),
    }
}

/// Qualifies every unqualified column of `expr` with its table.
fn qualify_columns(expr: &mut Expr, owner: &impl Fn(&str) -> Result<Option<String>>) -> Result<()> {
    match &mut expr.kind {
        ExprKind::Column {
            table: table @ None,
            name,
            ..
        } => {
            *table = owner(name)?;
        }
        ExprKind::FunctionCall { args, tail, .. } => {
            if let FunctionArgs::List(list) = args {
                for e in list {
                    qualify_columns(e, owner)?;
                }
            }
            if let Some(tail) = tail {
                if let Some(filter) = &mut tail.filter {
                    qualify_columns(filter, owner)?;
                }
            }
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::IsNull { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Collate { expr: inner, .. }
        | ExprKind::Paren(inner) => qualify_columns(inner, owner)?,
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Is { lhs, rhs, .. } => {
            qualify_columns(lhs, owner)?;
            qualify_columns(rhs, owner)?;
        }
        ExprKind::Between {
            expr: inner,
            lo,
            hi,
            ..
        } => {
            qualify_columns(inner, owner)?;
            qualify_columns(lo, owner)?;
            qualify_columns(hi, owner)?;
        }
        ExprKind::In {
            expr: inner, list, ..
        } => {
            qualify_columns(inner, owner)?;
            for e in list {
                qualify_columns(e, owner)?;
            }
        }
        ExprKind::Like {
            expr: inner,
            pattern,
            escape,
            ..
        } => {
            qualify_columns(inner, owner)?;
            qualify_columns(pattern, owner)?;
            if let Some(e) = escape {
                qualify_columns(e, owner)?;
            }
        }
        ExprKind::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                qualify_columns(o, owner)?;
            }
            for (when, then) in whens {
                qualify_columns(when, owner)?;
                qualify_columns(then, owner)?;
            }
            if let Some(e) = else_ {
                qualify_columns(e, owner)?;
            }
        }
        // Literals and parameters hold no column; subqueries are refused by
        // the column validator before a select gets here.
        _ => {}
    }
    Ok(())
}

/// Qualifies every unqualified column of the select list, `WHERE`,
/// `GROUP BY`, `HAVING` and `ORDER BY` with the table that has it
/// ([`column_owner`]). Before this, a bare name silently meant the `FROM`
/// table: `WHERE id = 10` on `a JOIN b ON a.id = b.id` filtered `a` where
/// SQLite and DuckDB report an ambiguous column.
fn qualify(mut select: Select, tables: &[(String, Vec<String>)]) -> Result<Select> {
    let Some(from) = select.from.as_ref() else {
        return Ok(select);
    };
    let listed: Vec<(String, Vec<String>)> = named_tables(&select)
        .iter()
        .map(|name| (name.to_string(), columns_of(tables, name)))
        .collect();
    let using_keys: Vec<String> = from.joins.iter().flat_map(using_columns).collect();
    let owner = |name: &str| column_owner(name, &listed, &using_keys);
    for column in &mut select.columns {
        if let ResultColumn::Expr { expr, alias } = column {
            // A bare `SELECT id` keeps `id` as its header once the
            // expression is rewritten to `production.id`.
            if let ExprKind::Column {
                table: None, name, ..
            } = &expr.kind
            {
                alias.get_or_insert_with(|| name.clone());
            }
            qualify_columns(expr, &owner)?;
        }
    }
    let exprs = select
        .where_clause
        .iter_mut()
        .chain(select.group_by.iter_mut())
        .chain(select.having.iter_mut());
    for expr in exprs {
        qualify_columns(expr, &owner)?;
    }
    // An `ORDER BY` term naming a select item (its alias, or the column
    // an unaliased `t.col` item shows) stays that reference.
    let items: Vec<String> = select
        .columns
        .iter()
        .filter_map(|c| match c {
            ResultColumn::Expr {
                alias: Some(alias), ..
            } => Some(alias.clone()),
            ResultColumn::Expr {
                expr:
                    Expr {
                        kind: ExprKind::Column { name, .. },
                        ..
                    },
                ..
            } => Some(name.clone()),
            _ => None,
        })
        .collect();
    for term in &mut select.order_by {
        let names_item = match &term.expr.kind {
            ExprKind::Column {
                table: None, name, ..
            } => items.iter().any(|i| i.eq_ignore_ascii_case(name)),
            _ => false,
        };
        if !names_item {
            qualify_columns(&mut term.expr, &owner)?;
        }
    }
    Ok(select)
}
