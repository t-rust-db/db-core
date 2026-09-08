// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! See `super`. Vectors for `subquery/{scalar,flatten,pushdown,views,
//! correlation}.rs`, `stmt/{insert,update}.rs`, and `select/eqp.rs`.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::codegen::row::{
    compile_insert, compile_select_with_catalog, compile_update_with_catalog, explain_query_plan,
    flatten_from_subqueries, push_down_where_predicates, resolve_views, CodegenError, ExpandViews,
    IndexSchema, IndexedColumn, TableSchema, ViewSchema,
};
use crate::parser::ast::{Select, TableRefKind};
use crate::parser::row::{parse_insert, parse_select, parse_update, ParseOutcome};
use crate::vm::row::{Opcode, Program};

fn table(name: &str, root_page: u32, columns: &[&str]) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        root_page,
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        column_types: columns.iter().map(|_| "INTEGER".to_string()).collect(),
        sql: format!("CREATE TABLE {name} ({})", columns.join(", ")),
        ..Default::default()
    }
}

fn with_index(mut schema: TableSchema, index: &str, root_page: u32, column: &str) -> TableSchema {
    schema.indexes.push(IndexSchema {
        name: index.to_string(),
        root_page,
        unique: false,
        columns: vec![IndexedColumn {
            name: column.to_string(),
            desc: false,
            collation: Default::default(),
        }],
    });
    schema
}

fn sel(sql: &str) -> Select {
    match parse_select(sql) {
        ParseOutcome::Accepted(select) => *select,
        other => panic!("{sql:?} must parse, got {other:?}"),
    }
}

fn has(program: &Program, opcode: Opcode) -> bool {
    program.instructions.iter().any(|i| i.opcode == opcode)
}

/// `t(a, b)` at root 2, `u(b)` at root 3.
fn t_and_u() -> Vec<TableSchema> {
    vec![table("t", 2, &["a", "b"]), table("u", 3, &["b"])]
}

fn compile_tu(sql: &str) -> Result<Program, CodegenError> {
    let catalog = t_and_u();
    compile_select_with_catalog(&sel(sql), &catalog[0], &catalog)
}

fn is_scalar_order_limit_rejection(result: &Result<Program, CodegenError>) -> bool {
    matches!(
        result,
        Err(CodegenError::Unsupported { reason })
            if reason.contains("ORDER BY/LIMIT in a scalar subquery")
    )
}

// scalar_91: `!subselect.order_by.is_empty() || subselect.limit.is_some()`

#[test]
fn mcdc__scalar_91__v1_plain_scalar_subquery_compiles() {
    let result = compile_tu("SELECT a FROM t WHERE a = (SELECT b FROM u)");
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn mcdc__scalar_91__v2_order_by_in_scalar_subquery_is_rejected() {
    let result = compile_tu("SELECT a FROM t WHERE a = (SELECT b FROM u ORDER BY b)");
    assert!(is_scalar_order_limit_rejection(&result), "{result:?}");
}

#[test]
fn mcdc__scalar_91__v3_limit_in_scalar_subquery_is_rejected() {
    let result = compile_tu("SELECT a FROM t WHERE a = (SELECT b FROM u LIMIT 1)");
    assert!(is_scalar_order_limit_rejection(&result), "{result:?}");
}

// flatten_173: the six-way "rows-changing clause" guard in
// `subquery_flatten_safe`. Observable: the FROM-subquery is replaced by
// the base table (`TableRefKind::Name`) only when every leaf is false.

fn flattened(inner: &str) -> bool {
    let mut select = sel(&format!("SELECT x FROM ({inner}) AS s"));
    flatten_from_subqueries(&mut select);
    matches!(
        select.from.as_ref().unwrap().first.kind,
        TableRefKind::Name(_)
    )
}

#[test]
fn mcdc__flatten_173__v1_plain_subquery_is_flattened() {
    assert!(flattened("SELECT a AS x FROM u"));
}

#[test]
fn mcdc__flatten_173__v2_distinct_blocks_flattening() {
    assert!(!flattened("SELECT DISTINCT a AS x FROM u"));
}

/// `HAVING` necessarily rides on a `GROUP BY`; the `having` leaf is the
/// one being exercised true here.
#[test]
fn mcdc__flatten_173__v3_having_blocks_flattening() {
    assert!(!flattened("SELECT a AS x FROM u GROUP BY a HAVING a > 1"));
}

#[test]
fn mcdc__flatten_173__v4_group_by_blocks_flattening() {
    assert!(!flattened("SELECT a AS x FROM u GROUP BY a"));
}

#[test]
fn mcdc__flatten_173__v5_limit_blocks_flattening() {
    assert!(!flattened("SELECT a AS x FROM u LIMIT 1"));
}

#[test]
fn mcdc__flatten_173__v6_compound_blocks_flattening() {
    assert!(!flattened(
        "SELECT a AS x FROM u UNION ALL SELECT a AS x FROM u"
    ));
}

#[test]
fn mcdc__flatten_173__v7_aggregate_blocks_flattening() {
    assert!(!flattened("SELECT max(a) AS x FROM u"));
}

// pushdown_133: the same six-way guard in `subquery_pushdown_safe`.
// Observable: the outer `WHERE x > 1` lands in the subquery's own
// `WHERE` only when every leaf is false.

fn pushed_down(inner: &str) -> bool {
    let mut select = sel(&format!("SELECT x FROM ({inner}) AS s WHERE x > 1"));
    push_down_where_predicates(&mut select);
    match &select.from.as_ref().unwrap().first.kind {
        TableRefKind::Subquery(inner) => inner.where_clause.is_some(),
        TableRefKind::Name(_) => panic!("pushdown must not flatten"),
    }
}

#[test]
fn mcdc__pushdown_133__v1_plain_subquery_receives_the_predicate() {
    assert!(pushed_down("SELECT a AS x FROM u"));
}

#[test]
fn mcdc__pushdown_133__v2_distinct_blocks_pushdown() {
    assert!(!pushed_down("SELECT DISTINCT a AS x FROM u"));
}

#[test]
fn mcdc__pushdown_133__v3_having_blocks_pushdown() {
    assert!(!pushed_down("SELECT a AS x FROM u GROUP BY a HAVING a > 0"));
}

#[test]
fn mcdc__pushdown_133__v4_group_by_blocks_pushdown() {
    assert!(!pushed_down("SELECT a AS x FROM u GROUP BY a"));
}

#[test]
fn mcdc__pushdown_133__v5_limit_blocks_pushdown() {
    assert!(!pushed_down("SELECT a AS x FROM u LIMIT 1"));
}

#[test]
fn mcdc__pushdown_133__v6_compound_blocks_pushdown() {
    assert!(!pushed_down(
        "SELECT a AS x FROM u UNION ALL SELECT a AS x FROM u"
    ));
}

#[test]
fn mcdc__pushdown_133__v7_aggregate_blocks_pushdown() {
    assert!(!pushed_down("SELECT max(a) AS x FROM u"));
}

// views_83: `views.is_empty() || !select_references_any_view(self, views)`

fn view_v() -> Vec<ViewSchema> {
    vec![ViewSchema {
        name: "v".to_string(),
        sql: "CREATE VIEW v AS SELECT b FROM u".to_string(),
    }]
}

#[test]
fn mcdc__views_83__v1_no_views_in_scope_borrows() {
    let select = sel("SELECT b FROM v");
    let resolved = resolve_views(&[]);
    assert!(matches!(
        select.expand_views(&resolved),
        Ok(Cow::Borrowed(_))
    ));
}

#[test]
fn mcdc__views_83__v2_views_in_scope_but_unreferenced_borrows() {
    let select = sel("SELECT a FROM t");
    let resolved = resolve_views(&view_v());
    assert!(matches!(
        select.expand_views(&resolved),
        Ok(Cow::Borrowed(_))
    ));
}

#[test]
fn mcdc__views_83__v3_referenced_view_is_expanded_into_an_owned_copy() {
    let select = sel("SELECT b FROM v");
    let resolved = resolve_views(&view_v());
    match select.expand_views(&resolved) {
        Ok(Cow::Owned(expanded)) => assert!(matches!(
            expanded.from.as_ref().unwrap().first.kind,
            TableRefKind::Subquery(_)
        )),
        other => panic!("expected an owned expansion, got {other:?}"),
    }
}

// correlation_82: `!qualifier_ok || !schema.columns...any(name)` inside
// `subquery_is_correlated`. Observable through hoisting (#306): an
// uncorrelated scalar subquery in the outer WHERE is evaluated once,
// before the outer table's `Rewind`; a correlated one is re-evaluated
// per row, i.e. its `OpenRead` on `u` (root 3) comes after that
// `Rewind`.

fn subquery_open_precedes_outer_rewind(program: &Program) -> bool {
    let rewind_t = program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Rewind && i.p1 == 0)
        .unwrap_or_else(|| panic!("no outer Rewind in {program:?}"));
    let open_u = program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::OpenRead && i.p2 == 3)
        .unwrap_or_else(|| panic!("no OpenRead on u in {program:?}"));
    open_u < rewind_t
}

#[test]
fn mcdc__correlation_82__v1_outer_qualifier_marks_the_subquery_correlated() {
    let p = compile_tu("SELECT a FROM t WHERE a = (SELECT b FROM u WHERE u.b = t.a)").unwrap();
    assert!(!subquery_open_precedes_outer_rewind(&p), "{p:?}");
}

#[test]
fn mcdc__correlation_82__v2_bare_column_not_in_the_subquery_table_is_correlated() {
    let p = compile_tu("SELECT a FROM t WHERE a = (SELECT b FROM u WHERE a = 1)").unwrap();
    assert!(!subquery_open_precedes_outer_rewind(&p), "{p:?}");
}

#[test]
fn mcdc__correlation_82__v3_own_column_with_own_qualifier_is_uncorrelated_and_hoisted() {
    let p = compile_tu("SELECT a FROM t WHERE a = (SELECT b FROM u WHERE u.b = 1)").unwrap();
    assert!(subquery_open_precedes_outer_rewind(&p), "{p:?}");
}

// insert_304: `!row.is_empty() && row.len() != target_columns.len()`

fn insert_result(sql: &str) -> Result<Program, CodegenError> {
    let insert = match parse_insert(sql) {
        ParseOutcome::Accepted(insert) => *insert,
        other => panic!("{sql:?} must parse, got {other:?}"),
    };
    compile_insert(&insert, &table("t", 2, &["a", "b"]), None)
}

#[test]
fn mcdc__insert_304__v1_short_row_is_a_shape_mismatch() {
    assert!(matches!(
        insert_result("INSERT INTO t VALUES (1)"),
        Err(CodegenError::RowShapeMismatch {
            expected: 2,
            found: 1,
            ..
        })
    ));
}

#[test]
fn mcdc__insert_304__v2_full_row_compiles() {
    assert!(insert_result("INSERT INTO t VALUES (1, 2)").is_ok());
}

/// The `!row.is_empty()` leaf: an empty `VALUES ()` row is not a shape
/// mismatch at this check (whatever the later stages make of it). If the
/// grammar refuses an empty row outright the leaf is unreachable from
/// SQL, and this vector records that instead.
#[test]
fn mcdc__insert_304__v3_empty_row_is_not_a_shape_mismatch() {
    let ParseOutcome::Accepted(insert) = parse_insert("INSERT INTO t VALUES ()") else {
        return;
    };
    let result = compile_insert(&insert, &table("t", 2, &["a", "b"]), None);
    assert!(!matches!(
        result,
        Err(CodegenError::RowShapeMismatch { .. })
    ));
}

// update_346: `used_range_seek && range_seek_touches_scanned_index`.
// Observable: the two-pass plan opens an ephemeral rowid table.

fn update_program(sql: &str) -> Program {
    let update = match parse_update(sql) {
        ParseOutcome::Accepted(update) => *update,
        other => panic!("{sql:?} must parse, got {other:?}"),
    };
    let schema = with_index(table("t", 2, &["a", "b"]), "ia", 5, "a");
    compile_update_with_catalog(&update, &schema, std::slice::from_ref(&schema)).unwrap()
}

#[test]
fn mcdc__update_346__v1_range_seek_over_an_index_the_set_touches_uses_two_passes() {
    let p = update_program("UPDATE t SET a = 9 WHERE a BETWEEN 1 AND 5");
    assert!(has(&p, Opcode::OpenEphemeral), "{p:?}");
}

#[test]
fn mcdc__update_346__v2_range_seek_over_an_untouched_index_is_single_pass() {
    let p = update_program("UPDATE t SET b = 9 WHERE a BETWEEN 1 AND 5");
    assert!(
        has(&p, Opcode::IdxRowid) && !has(&p, Opcode::OpenEphemeral),
        "{p:?}"
    );
}

#[test]
fn mcdc__update_346__v3_no_range_seek_is_a_plain_scan() {
    let p = update_program("UPDATE t SET a = 9 WHERE b = 1");
    assert!(
        !has(&p, Opcode::IdxRowid) && !has(&p, Opcode::OpenEphemeral),
        "{p:?}"
    );
}

// eqp_251 / eqp_263 / eqp_273 / eqp_423 -- `explain_query_plan` details.

/// `t(a, b)` at root 2 with `ia(a)` at 5 and `ib(b)` at 6; `u(b)` at 3.
fn eqp_catalog() -> Vec<TableSchema> {
    let t = with_index(
        with_index(table("t", 2, &["a", "b"]), "ia", 5, "a"),
        "ib",
        6,
        "b",
    );
    vec![t, with_index(table("u", 3, &["b"]), "iub", 7, "b")]
}

fn eqp_details(sql: &str) -> Vec<String> {
    let catalog = eqp_catalog();
    let select = sel(sql);
    let mut schemas = vec![catalog[0].clone()];
    if select.from.as_ref().is_some_and(|f| !f.joins.is_empty()) {
        schemas.push(catalog[1].clone());
    }
    explain_query_plan(&select, &schemas, &HashMap::new(), &catalog)
        .unwrap()
        .into_iter()
        .map(|r| r.detail)
        .collect()
}

// eqp_251: `level == 0 && access.is_none()` (automatic-index probe)

#[test]
fn mcdc__eqp_251__v1_outer_table_without_a_seek_is_a_scan() {
    let d = eqp_details("SELECT a FROM t WHERE a + b = 1");
    assert!(d[0].starts_with("SCAN t"), "{d:?}");
}

#[test]
fn mcdc__eqp_251__v2_outer_table_with_a_rowid_seek_is_a_search() {
    let d = eqp_details("SELECT a FROM t WHERE rowid = 1");
    assert!(
        d[0].contains("SEARCH t") && d[0].contains("rowid=?"),
        "{d:?}"
    );
}

#[test]
fn mcdc__eqp_251__v3_inner_join_level_reports_its_own_access() {
    let d = eqp_details("SELECT a FROM t JOIN u ON u.b = t.a");
    assert!(d.iter().any(|x| x.contains('u')), "{d:?}");
}

// eqp_263 / eqp_273: `level == 0 && access.is_none() && covering.is_none()`

fn range_seek_detail_present(d: &[String]) -> bool {
    d[0].contains("SEARCH t USING INDEX ib")
}

#[test]
fn mcdc__eqp_263__v1_all_true_reaches_the_range_seek_report() {
    let d = eqp_details("SELECT a, b FROM t WHERE b BETWEEN 1 AND 5");
    assert!(range_seek_detail_present(&d), "{d:?}");
}

#[test]
fn mcdc__eqp_263__v2_inner_level_never_reports_a_range_seek() {
    // Only the outermost table's WHERE is consulted for a range seek, so
    // the inner level (`u`, indexed on `b`) reports its join access, never
    // a `b>? AND b<?` range.
    let d = eqp_details("SELECT a FROM t JOIN u ON u.b = t.a WHERE a + b = 1");
    assert!(d.len() == 2 && !d[1].contains("b>?"), "{d:?}");
}

#[test]
fn mcdc__eqp_263__v3_rowid_seek_takes_precedence() {
    let d = eqp_details("SELECT a, b FROM t WHERE rowid = 1");
    assert!(
        d[0].contains("rowid=?") && !range_seek_detail_present(&d),
        "{d:?}"
    );
}

#[test]
fn mcdc__eqp_263__v4_covering_index_takes_precedence() {
    let d = eqp_details("SELECT a FROM t WHERE a = 1");
    assert!(d[0].contains("COVERING INDEX ia"), "{d:?}");
}

#[test]
fn mcdc__eqp_273__v1_all_true_reaches_the_range_seek_report() {
    let d = eqp_details("SELECT a, b FROM t WHERE b BETWEEN 1 AND 5");
    assert!(range_seek_detail_present(&d), "{d:?}");
}

#[test]
fn mcdc__eqp_273__v2_inner_level_never_reports_a_range_seek() {
    // Only the outermost table's WHERE is consulted for a range seek, so
    // the inner level (`u`, indexed on `b`) reports its join access, never
    // a `b>? AND b<?` range.
    let d = eqp_details("SELECT a FROM t JOIN u ON u.b = t.a WHERE a + b = 1");
    assert!(d.len() == 2 && !d[1].contains("b>?"), "{d:?}");
}

#[test]
fn mcdc__eqp_273__v3_rowid_seek_takes_precedence() {
    let d = eqp_details("SELECT a, b FROM t WHERE rowid = 1");
    assert!(
        d[0].contains("rowid=?") && !range_seek_detail_present(&d),
        "{d:?}"
    );
}

#[test]
fn mcdc__eqp_273__v4_covering_index_takes_precedence() {
    let d = eqp_details("SELECT a FROM t WHERE a = 1");
    assert!(d[0].contains("COVERING INDEX ia"), "{d:?}");
}

// eqp_423: `from.joins.is_empty() && !select.group_by.is_empty()`

const TEMP_BTREE: &str = "USE TEMP B-TREE FOR GROUP BY";

#[test]
fn mcdc__eqp_423__v1_single_table_group_by_without_an_index_uses_a_temp_btree() {
    let d = eqp_details("SELECT a + b, count(*) FROM t GROUP BY a + b");
    assert!(d.iter().any(|x| x == TEMP_BTREE), "{d:?}");
}

#[test]
fn mcdc__eqp_423__v2_joined_group_by_is_not_reported() {
    let d = eqp_details("SELECT t.a, count(*) FROM t JOIN u ON u.b = t.a GROUP BY t.a");
    assert!(!d.iter().any(|x| x == TEMP_BTREE), "{d:?}");
}

#[test]
fn mcdc__eqp_423__v3_single_table_without_group_by_is_not_reported() {
    let d = eqp_details("SELECT a FROM t");
    assert!(!d.iter().any(|x| x == TEMP_BTREE), "{d:?}");
}
