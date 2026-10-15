// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
use super::*;
/// Where an ORDER BY term's sort key comes from: a raw table column
/// (known schema index, always present in the sorter's row tuple), or
/// a genuine expression that must be computed into its own register
/// and appended to that tuple — its position within the record isn't
/// known until `compile_sorted_scan` actually allocates registers.
#[derive(Debug, Clone)]
pub(super) enum OrderByTarget {
    Column(usize),
    Expr(Expr),
}

pub(super) struct OrderByPlan {
    pub(super) target: OrderByTarget,
    pub(super) descending: bool,
    pub(super) collation: Collation,
    pub(super) nulls_first: bool,
}

pub(super) fn resolve_order_by(
    select: &Select,
    schema: &TableSchema,
) -> Result<Vec<OrderByPlan>, CodegenError> {
    let mut plans = Vec::with_capacity(select.order_by.len());
    for term in &select.order_by {
        let base_expr = strip_collate(&term.expr);
        let target = resolve_order_by_target(base_expr, select, schema)?;
        let descending = term.desc.unwrap_or(false);
        // No NULLS clause defaults to NULLS FIRST for ASC, NULLS LAST for
        // DESC (SQLite's default, matching this compiler's prior
        // behavior); an explicit clause overrides that per direction.
        let nulls_first = term
            .nulls_last
            .map_or(!descending, |nulls_last| !nulls_last);
        let collation = collation_of(&term.expr).unwrap_or_else(|| match &target {
            OrderByTarget::Column(idx) => schema
                .column_collations
                .get(*idx)
                .copied()
                .unwrap_or(Collation::Binary),
            OrderByTarget::Expr(_) => Collation::Binary,
        });
        plans.push(OrderByPlan {
            target,
            descending,
            collation,
            nulls_first,
        });
    }
    Ok(plans)
}

/// Unwraps `expr COLLATE name` (and surrounding parens) down to the
/// expression the ordering is actually keyed on; the collation itself
/// is read separately via `collation_of`.
pub(super) fn strip_collate(expr: &Expr) -> &Expr {
    match &expr.kind {
        ExprKind::Collate { expr: inner, .. } | ExprKind::Paren(inner) => strip_collate(inner),
        _ => expr,
    }
}

/// One result column as seen by ORDER BY ordinal/alias resolution: its
/// full expression (so an ordinal/alias resolving to a computed
/// expression can still become an `OrderByTarget::Expr`) and its `AS`
/// alias, if any. `*`/`table.*` expand against `schema` the same way
/// `result_columns` does, since this compiler is single-table (V2
/// scope).
pub(super) struct OrderByEntry {
    expr: Expr,
    alias: Option<String>,
}

/// A dummy span for expressions synthesized during `*`/`table.*`
/// expansion — not sourced from any actual token, so never used for
/// error reporting.
pub(super) const SYNTHETIC_SPAN: Span = Span {
    line: 0,
    column: 0,
    offset: 0,
    len: 0,
};

/// The compound `SELECT`'s own output column names, in projection
/// order — an alias when the result column has one, else (for a bare
/// column reference) that column's name, else SQLite's positional
/// `columnN` fallback. Used to build the synthetic [`TableSchema`]
/// [`resolve_order_by`] resolves a compound's trailing `ORDER BY`
/// against, since a compound's `ORDER BY` binds to its own result
/// columns (only the first arm's names are visible), never to any
/// arm's underlying table columns.
///
/// Also reused (crate-external, `pub`) by `bin/sqlite-rs/repl.rs` to
/// derive `.headers on` column labels for a single-table `SELECT` —
/// the same "alias, else bare column name, else `columnN`" rule
/// applies there.
pub fn output_column_names(select: &Select, schema: &TableSchema) -> Vec<String> {
    order_by_entries(select, schema)
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            entry.alias.unwrap_or_else(|| match &entry.expr.kind {
                ExprKind::Column { name, .. } => name.clone(),
                _ => format!("column{}", i.saturating_add(1)),
            })
        })
        .collect()
}

pub(super) fn order_by_entries(select: &Select, schema: &TableSchema) -> Vec<OrderByEntry> {
    let mut out = Vec::new();
    for col in &select.columns {
        match col {
            ResultColumn::Star | ResultColumn::TableStar { .. } => {
                for name in &schema.columns {
                    out.push(OrderByEntry {
                        expr: Expr {
                            kind: ExprKind::Column {
                                table: None,
                                catalog: None,
                                name: name.clone(),
                            },
                            span: SYNTHETIC_SPAN,
                        },
                        alias: None,
                    });
                }
            }
            ResultColumn::Expr { expr, alias } => out.push(OrderByEntry {
                expr: expr.clone(),
                alias: alias.clone(),
            }),
        }
    }
    out
}

/// `select` with its GROUP BY and HAVING names resolved as sqlite3 does
/// (#610), or `None` when nothing changes. A GROUP BY integer is a
/// 1-based position into `entries` (the result columns, `*` expanded);
/// a bare GROUP BY or HAVING name that `is_column` doesn't know as a
/// FROM column is a result-column alias. Columns win over aliases here,
/// unlike ORDER BY. A GROUP BY term may not name an aggregate.
pub(super) fn resolve_group_terms<F>(
    select: &Select,
    entries: &[OrderByEntry],
    is_column: F,
) -> Result<Option<Select>, CodegenError>
where
    F: Fn(&str) -> bool,
{
    if select.group_by.is_empty() && select.having.is_none() {
        return Ok(None);
    }
    let alias = |name: &str| -> Option<Expr> {
        if is_column(name) {
            return None;
        }
        entries
            .iter()
            .find(|e| {
                e.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(name))
            })
            .map(|e| e.expr.clone())
    };
    let mut changed = false;
    let mut group_by = Vec::with_capacity(select.group_by.len());
    for (i, term) in select.group_by.iter().enumerate() {
        let named = match &strip_collate(term).kind {
            ExprKind::Literal(Literal::Integer(n)) => Some(
                usize::try_from(*n)
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|idx| entries.get(idx))
                    .map(|e| e.expr.clone())
                    .ok_or_else(|| CodegenError::Unsupported {
                        reason: format!(
                            "{} GROUP BY term out of range - should be between 1 and {}",
                            ordinal(i.saturating_add(1)),
                            entries.len()
                        ),
                    })?,
            ),
            ExprKind::Column {
                table: None, name, ..
            } => alias(name),
            _ => None,
        };
        let Some(named) = named else {
            group_by.push(term.clone());
            continue;
        };
        let mut aggregates = Vec::new();
        super::aggregate::find_aggregates(&named, &mut aggregates);
        if !aggregates.is_empty() {
            return Err(CodegenError::Unsupported {
                reason: "aggregate functions are not allowed in the GROUP BY clause".to_string(),
            });
        }
        changed = true;
        group_by.push(match &term.kind {
            ExprKind::Collate { collation, .. } => Expr {
                kind: ExprKind::Collate {
                    expr: Box::new(named),
                    collation: collation.clone(),
                },
                span: term.span,
            },
            _ => named,
        });
    }
    let having = match &select.having {
        Some(having) => {
            let resolved = resolve_having_aliases(having, &alias)?;
            changed |= resolved != *having;
            Some(resolved)
        }
        None => None,
    };
    Ok(changed.then(|| Select {
        group_by,
        having,
        ..select.clone()
    }))
}

/// [`resolve_group_terms`] for a single-table `select` over `schema`.
pub(super) fn resolve_single_table_group_terms(
    select: &Select,
    schema: &TableSchema,
) -> Result<Option<Select>, CodegenError> {
    resolve_group_terms(select, &order_by_entries(select, schema), |name| {
        column_index(schema, name).is_some()
    })
}

/// [`resolve_group_terms`] over a join: `*`/`table.*` expand across
/// `scope.tables` in FROM order (skipping NATURAL/USING duplicates) to
/// qualified column references, and a name is a FROM column when any
/// table has it.
pub(super) fn resolve_joined_group_terms(
    select: &Select,
    scope: &Scope,
) -> Result<Option<Select>, CodegenError> {
    let column = |binding: &TableBinding, name: &str| OrderByEntry {
        expr: Expr {
            kind: ExprKind::Column {
                table: Some(
                    binding
                        .alias
                        .clone()
                        .unwrap_or_else(|| binding.name.clone()),
                ),
                catalog: None,
                name: name.to_string(),
            },
            span: SYNTHETIC_SPAN,
        },
        alias: None,
    };
    let mut entries = Vec::new();
    for col in &select.columns {
        match col {
            ResultColumn::Star => {
                for (i, binding) in scope.tables.iter().enumerate() {
                    let suppressed = scope.dedup_star.get(i);
                    for name in &binding.schema.columns {
                        if !suppressed.is_some_and(|s| s.contains(&name.to_ascii_lowercase())) {
                            entries.push(column(binding, name));
                        }
                    }
                }
            }
            ResultColumn::TableStar { table } => {
                if let Some(binding) = scope.tables.iter().find(|b| b.matches_qualifier(table)) {
                    entries.extend(
                        binding
                            .schema
                            .columns
                            .iter()
                            .map(|name| column(binding, name)),
                    );
                }
            }
            ResultColumn::Expr { expr, alias } => entries.push(OrderByEntry {
                expr: expr.clone(),
                alias: alias.clone(),
            }),
        }
    }
    resolve_group_terms(select, &entries, |name| {
        scope
            .tables
            .iter()
            .any(|b| column_index(&b.schema, name).is_some())
    })
}

/// `expr` with each bare name `alias` resolves replaced by that result
/// column's expression; a subquery is its own scope and is left alone.
fn resolve_having_aliases<A>(expr: &Expr, alias: &A) -> Result<Expr, CodegenError>
where
    A: Fn(&str) -> Option<Expr>,
{
    if let ExprKind::Column {
        table: None, name, ..
    } = &expr.kind
    {
        return Ok(alias(name).unwrap_or_else(|| expr.clone()));
    }
    super::aggregate::map_subexprs(expr, &mut |sub: &Expr| resolve_having_aliases(sub, alias))
}

/// `1st`, `2nd`, `3rd`, `4th`, ... as sqlite3 numbers a clause's terms.
fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, r) if r != 11 => "st",
        (2, r) if r != 12 => "nd",
        (3, r) if r != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Resolves a result-column expression to its `OrderByTarget`: a bare
/// unqualified column becomes a direct schema index (already present
/// in the sorter's row tuple), anything else becomes a computed
/// expression that `compile_sorted_scan` appends to that tuple.
pub(super) fn order_by_target_for_expr(
    expr: &Expr,
    schema: &TableSchema,
) -> Result<OrderByTarget, CodegenError> {
    match &expr.kind {
        ExprKind::Column {
            table: None, name, ..
        } => column_index(schema, name)
            .map(OrderByTarget::Column)
            .ok_or_else(|| CodegenError::UnknownColumn { name: name.clone() }),
        _ => Ok(OrderByTarget::Expr(expr.clone())),
    }
}

pub(super) fn resolve_order_by_target(
    expr: &Expr,
    select: &Select,
    schema: &TableSchema,
) -> Result<OrderByTarget, CodegenError> {
    match &expr.kind {
        ExprKind::Literal(Literal::Integer(n)) => {
            let entries = order_by_entries(select, schema);
            let zero_based = usize::try_from(*n)
                .ok()
                .and_then(|ordinal| ordinal.checked_sub(1));
            let entry = zero_based
                .and_then(|zero_based| entries.get(zero_based))
                .ok_or_else(|| CodegenError::Unsupported {
                    reason: format!(
                        "ORDER BY position {n} is out of range for a {}-column result set",
                        entries.len()
                    ),
                })?;
            order_by_target_for_expr(&entry.expr, schema)
        }
        ExprKind::Column {
            table: None, name, ..
        } => {
            // Result-column aliases take precedence over table columns
            // for ORDER BY (unlike WHERE, where aliases aren't visible
            // at all).
            let entries = order_by_entries(select, schema);
            if let Some(entry) = entries
                .iter()
                .find(|e| e.alias.as_deref() == Some(name.as_str()))
            {
                return order_by_target_for_expr(&entry.expr, schema);
            }
            column_index(schema, name)
                .map(OrderByTarget::Column)
                .ok_or_else(|| CodegenError::UnknownColumn { name: name.clone() })
        }
        _ => order_by_target_for_expr(expr, schema),
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#219/#235).

    use super::*;
    use crate::parser::row::{parse_select, ParseOutcome};

    fn resolved(sql: &str) -> Option<Select> {
        let select = match parse_select(sql) {
            ParseOutcome::Accepted(select) => *select,
            other => panic!("{sql}: {other:?}"),
        };
        let schema = TableSchema {
            name: "p".to_string(),
            columns: vec!["a".to_string(), "b".to_string()],
            ..TableSchema::default()
        };
        resolve_single_table_group_terms(&select, &schema).unwrap()
    }

    // ---------------------------------------------------------------------
    // resolve_group_terms -- nothing to resolve without GROUP BY or HAVING.
    // ---------------------------------------------------------------------
    #[test]
    fn mcdc__codegen_row_select_order_by_resolve_group_terms_6bfa2166__v1_no_group_by_or_having_is_left_alone(
    ) {
        assert!(resolved("SELECT b % 2 AS par FROM p ORDER BY par").is_none());
    }

    #[test]
    fn mcdc__codegen_row_select_order_by_resolve_group_terms_6bfa2166__v2_a_group_by_position_is_resolved(
    ) {
        let select = resolved("SELECT b % 2 AS par, count(*) FROM p GROUP BY 1").unwrap();
        assert!(
            matches!(
                select.group_by.as_slice(),
                [Expr {
                    kind: ExprKind::Binary { .. },
                    ..
                }]
            ),
            "{:?}",
            select.group_by
        );
    }

    #[test]
    fn mcdc__codegen_row_select_order_by_resolve_group_terms_6bfa2166__v3_a_having_alias_is_resolved(
    ) {
        let select = resolved("SELECT count(*) AS c FROM p HAVING c > 1").unwrap();
        let Some(Expr {
            kind: ExprKind::Binary { lhs, .. },
            ..
        }) = &select.having
        else {
            panic!("{:?}", select.having);
        };
        assert!(matches!(lhs.kind, ExprKind::FunctionCall { .. }), "{lhs:?}");
    }
}
