//! `EXPLAIN QUERY PLAN` output (db-core#94) -- ported from sqlite-rs's
//! `src/codegen/select/eqp.rs`.
//!
//! Reports, per `FROM`-clause table, the access path
//! [`super::select::compile_select`] actually compiles: `SCAN` for a
//! `Rewind`/`Next` scan, `SEARCH ... USING INDEX ...` for one of
//! [`super::index_scan`]/[`super::range_scan`]'s index walks. The
//! decision is made by calling those modules' own predicates, so the
//! report can never drift from what the compiler emits.
//!
//! **Scoped down** the same way this module's scans are: sqlite-rs's
//! `explain_query_plan` takes a `HashMap<String, planner::Stats>` and
//! delegates every non-outermost table to `join_access`'s cost-model
//! chooser, neither of which db-core has (see [`super`]'s note on
//! `planner.rs`). A join's inner table is therefore always reported as
//! a `SCAN`, which is exactly what [`super::select::compile_select_join`]
//! compiles today. `USING TEMP B-TREE FOR ORDER BY` is reported for an
//! `ORDER BY` no index satisfies, matching the sorter that path opens.

use super::{index_scan, range_scan, Result, TableSchema};
use crate::expr::Query;

/// One row of `EXPLAIN QUERY PLAN` output -- SQLite's own EQP shape
/// (`id, parent, notused, detail`), distinct from plain `EXPLAIN`'s
/// per-instruction `vm::row::explain::ExplainRow`. `detail` reads like
/// the oracle's own EQP but isn't guaranteed byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqpRow {
    /// This row's identifier within the plan.
    pub id: i32,
    /// The `id` of this row's parent in the plan tree (0 for a top-level
    /// row).
    pub parent: i32,
    /// Unused column, kept for SQLite's `EXPLAIN QUERY PLAN` shape.
    pub notused: i32,
    /// Human-readable plan step, e.g. `SCAN t`/`SEARCH t USING INDEX
    /// t_a (a>?)`.
    pub detail: String,
}

impl EqpRow {
    fn new(id: i32, detail: String) -> Self {
        EqpRow {
            id,
            parent: 0,
            notused: 0,
            detail,
        }
    }
}

/// Builds `EXPLAIN QUERY PLAN`'s output for `query` against `schema`
/// (the `FROM` table) and `right_schema` (`query.joins[0].table`, when
/// the query joins).
pub fn explain_query_plan(
    query: &Query,
    schema: &TableSchema,
    right_schema: Option<&TableSchema>,
) -> Result<Vec<EqpRow>> {
    let mut rows = Vec::new();
    let mut next_id: i32 = 0;
    let mut push = |rows: &mut Vec<EqpRow>, detail: String| {
        next_id = next_id.saturating_add(1);
        rows.push(EqpRow::new(next_id, detail));
    };

    push(&mut rows, outer_access_detail(query, schema));

    if let Some(right_schema) = right_schema {
        // Every inner table is a `SCAN`: `compile_select_join` compiles a
        // plain nested loop, and choosing anything else needs the cost
        // model this module doesn't have.
        push(&mut rows, format!("SCAN {}", right_schema.name));
    }

    if uses_sorter_for_order_by(query, schema) {
        push(&mut rows, "USE TEMP B-TREE FOR ORDER BY".to_string());
    }
    Ok(rows)
}

/// The outermost table's access path: the index walk one of this
/// module's fast paths would take, or a full scan.
fn outer_access_detail(query: &Query, schema: &TableSchema) -> String {
    let name = &schema.name;
    if query.joins.is_empty() && query.where_clause.is_none() && !query.distinct {
        if let Some(order_by) = &query.order_by {
            if let Some(position) = index_scan::find_ordering_index(schema, &order_by.column) {
                if let Some(index) = schema.indexes.get(position) {
                    return format!("SCAN {name} USING INDEX {}", index.name);
                }
            }
        }
    }
    if let Some((column, op)) = range_scan::seek_detail(query, schema) {
        if let Some(position) = range_scan::find_leading_index(schema, column) {
            if let Some(index) = schema.indexes.get(position) {
                return format!("SEARCH {name} USING INDEX {} ({column}{op}?)", index.name);
            }
        }
    }
    format!("SCAN {name}")
}

/// Whether the query's `ORDER BY` still needs the sorter -- i.e. it has
/// one and no index-ordered scan satisfies it.
fn uses_sorter_for_order_by(query: &Query, schema: &TableSchema) -> bool {
    let Some(order_by) = &query.order_by else {
        return false;
    };
    if query.joins.is_empty() && query.where_clause.is_none() && !query.distinct {
        return index_scan::find_ordering_index(schema, &order_by.column).is_none();
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::codegen::row::IndexSchema;
    use crate::expr::{BinOp, Expr, OrderBy, SelectItem};
    use crate::types::Literal;

    fn schema(indexes: Vec<IndexSchema>) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            columns: vec!["a".to_string(), "b".to_string()],
            column_types: vec!["INTEGER".to_string(), "TEXT".to_string()],
            rowid_alias: None,
            root_page: 2,
            indexes,
        }
    }

    fn index_on_a() -> IndexSchema {
        IndexSchema {
            name: "t_a".to_string(),
            root_page: 3,
            columns: vec!["a".to_string()],
        }
    }

    fn query() -> Query {
        Query {
            columns: vec![SelectItem::Column("a".to_string())],
            from: "t".to_string(),
            joins: vec![],
            where_clause: None,
            distinct: false,
            group_by: vec![],
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        }
    }

    #[test]
    fn a_bare_select_reports_a_full_scan() {
        let rows = explain_query_plan(&query(), &schema(vec![]), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].detail, "SCAN t");
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[0].parent, 0);
    }

    #[test]
    fn an_indexed_order_by_reports_an_index_scan_and_no_temp_b_tree() {
        let mut q = query();
        q.order_by = Some(OrderBy {
            column: "a".to_string(),
            descending: false,
        });
        let rows = explain_query_plan(&q, &schema(vec![index_on_a()]), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].detail, "SCAN t USING INDEX t_a");
    }

    #[test]
    fn an_unindexed_order_by_reports_a_temp_b_tree() {
        let mut q = query();
        q.order_by = Some(OrderBy {
            column: "b".to_string(),
            descending: false,
        });
        let rows = explain_query_plan(&q, &schema(vec![index_on_a()]), None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].detail, "SCAN t");
        assert_eq!(rows[1].detail, "USE TEMP B-TREE FOR ORDER BY");
    }

    #[test]
    fn a_bounded_indexed_column_reports_a_search() {
        let mut q = query();
        q.where_clause = Some(Expr::BinaryOp(
            Box::new(Expr::Column("a".to_string())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(5))),
        ));
        let rows = explain_query_plan(&q, &schema(vec![index_on_a()]), None).unwrap();
        assert_eq!(rows[0].detail, "SEARCH t USING INDEX t_a (a>?)");
    }

    #[test]
    fn an_equality_on_an_indexed_column_reports_an_equality_search() {
        let mut q = query();
        q.where_clause = Some(Expr::BinaryOp(
            Box::new(Expr::Column("a".to_string())),
            BinOp::Eq,
            Box::new(Expr::Literal(Literal::Int(5))),
        ));
        let rows = explain_query_plan(&q, &schema(vec![index_on_a()]), None).unwrap();
        assert_eq!(rows[0].detail, "SEARCH t USING INDEX t_a (a=?)");
    }

    #[test]
    fn an_unindexed_where_clause_reports_a_full_scan() {
        let mut q = query();
        q.where_clause = Some(Expr::BinaryOp(
            Box::new(Expr::Column("b".to_string())),
            BinOp::Gt,
            Box::new(Expr::Literal(Literal::Int(5))),
        ));
        let rows = explain_query_plan(&q, &schema(vec![index_on_a()]), None).unwrap();
        assert_eq!(rows[0].detail, "SCAN t");
    }

    #[test]
    fn a_joined_query_reports_one_row_per_table() {
        let mut q = query();
        q.joins = vec![crate::expr::Join {
            kind: crate::expr::JoinKind::Inner,
            table: "u".to_string(),
            left_col: "a".to_string(),
            right_col: "x".to_string(),
        }];
        let mut right = schema(vec![]);
        right.name = "u".to_string();
        let rows = explain_query_plan(&q, &schema(vec![index_on_a()]), Some(&right)).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].detail, "SCAN t");
        assert_eq!(rows[1].detail, "SCAN u");
    }
}
