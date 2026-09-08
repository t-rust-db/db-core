// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! See `super`. Vectors for the range-seek fast paths
//! (`select/range_scan.rs`), the post-`ORDER BY` pseudo-cursor projection
//! (`select/projection.rs`) and rowid-name recognition
//! (`select/limit_scan.rs`).

use std::collections::HashMap;

use crate::codegen::row::explain_query_plan;
use crate::codegen::row::select::is_rowid_reference;
use crate::codegen::row::select::range_seek_index_position;
use crate::codegen::row::{
    compile_select_with_catalog, compile_update, IndexSchema, IndexedColumn, TableSchema,
};
use crate::parser::ast::{Expr, ExprKind, Select, Update};
use crate::parser::row::error::{parse_select, parse_update, ParseOutcome};
use crate::parser::Span;
use crate::value::Collation;
use crate::vm::row::{Opcode, Program};

const INDEX_ROOT: i32 = 3;

fn select(sql: &str) -> Select {
    match parse_select(sql) {
        ParseOutcome::Accepted(s) => *s,
        other => panic!("expected {sql:?} to parse as a SELECT, got {other:?}"),
    }
}

fn update(sql: &str) -> Update {
    match parse_update(sql) {
        ParseOutcome::Accepted(u) => *u,
        other => panic!("expected {sql:?} to parse as an UPDATE, got {other:?}"),
    }
}

fn where_of(sql: &str) -> Expr {
    select(sql).where_clause.expect("WHERE clause")
}

fn column_expr(name: &str) -> Expr {
    Expr {
        kind: ExprKind::Column {
            table: None,
            catalog: None,
            name: name.to_string(),
        },
        span: Span {
            line: 0,
            column: 0,
            offset: 0,
            len: 0,
        },
    }
}

/// Table `t(a <a_type>, b TEXT)` rooted at page 2, with one index `idx`
/// on `a` (root page 3) when `indexed`, and `a` as the rowid alias when
/// `alias`.
fn schema(a_type: &str, indexed: bool, alias: bool) -> TableSchema {
    let indexes = if indexed {
        vec![IndexSchema {
            name: "idx".to_string(),
            unique: false,
            columns: vec![IndexedColumn {
                name: "a".to_string(),
                desc: false,
                collation: Collation::Binary,
            }],
            root_page: 3,
        }]
    } else {
        vec![]
    };
    TableSchema {
        name: "t".to_string(),
        root_page: 2,
        columns: vec!["a".to_string(), "b".to_string()],
        column_types: vec![a_type.to_string(), "TEXT".to_string()],
        column_collations: vec![Collation::Binary, Collation::Binary],
        sql: format!("CREATE TABLE t (a {a_type}, b TEXT)"),
        indexes,
        rowid_alias: if alias { Some(0) } else { None },
        ..Default::default()
    }
}

fn compile(sql: &str, schema: &TableSchema) -> Program {
    compile_select_with_catalog(&select(sql), schema, std::slice::from_ref(schema))
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn compile_upd(sql: &str, schema: &TableSchema) -> Program {
    compile_update(&update(sql), schema).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn has(program: &Program, opcode: Opcode) -> bool {
    program.instructions.iter().any(|i| i.opcode == opcode)
}

fn seeks(program: &Program) -> bool {
    has(program, Opcode::SeekIndexGE)
}

/// The cursor slot `OpenRead` bound to the index root page, if any.
fn index_cursor(program: &Program) -> Option<i32> {
    program
        .instructions
        .iter()
        .find(|i| i.opcode == Opcode::OpenRead && i.p2 == INDEX_ROOT)
        .map(|i| i.p1)
}

fn reads_column_from_index(program: &Program) -> bool {
    let Some(cursor) = index_cursor(program) else {
        return false;
    };
    program
        .instructions
        .iter()
        .any(|i| i.opcode == Opcode::Column && i.p1 == cursor)
}

fn eqp_detail(sql: &str, schema: &TableSchema) -> String {
    let rows = explain_query_plan(
        &select(sql),
        std::slice::from_ref(schema),
        &HashMap::new(),
        std::slice::from_ref(schema),
    )
    .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    rows.first().map(|r| r.detail.clone()).unwrap_or_default()
}

// --- range_scan_284: `name == indexed_col && rowid_alias != idx` -------

#[test]
fn mcdc__range_scan_284__v1_indexed_non_alias_column_is_read_from_the_index_cursor() {
    let p = compile(
        "SELECT a, b FROM t WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(seeks(&p));
    assert!(reads_column_from_index(&p));
}

#[test]
fn mcdc__range_scan_284__v2_indexed_rowid_alias_column_is_read_via_rowid() {
    let p = compile(
        "SELECT a, b FROM t WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, true),
    );
    assert!(seeks(&p));
    assert!(has(&p, Opcode::Rowid));
    assert!(!reads_column_from_index(&p));
}

#[test]
fn mcdc__range_scan_284__v3_non_indexed_column_is_read_from_the_table_cursor() {
    let p = compile(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(seeks(&p));
    assert!(!reads_column_from_index(&p));
}

// --- range_scan_338: BETWEEN operands supported -----------------------

#[test]
fn mcdc__range_scan_338__v1_both_bounds_literal_seeks() {
    let p = compile(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(seeks(&p));
}

#[test]
fn mcdc__range_scan_338__v2_computed_lower_bound_falls_back_to_scan() {
    let p = compile(
        "SELECT b FROM t WHERE a BETWEEN 1 + 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(!seeks(&p));
    assert!(has(&p, Opcode::Rewind));
}

#[test]
fn mcdc__range_scan_338__v3_computed_upper_bound_falls_back_to_scan() {
    let p = compile(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 5 + 1",
        &schema("INTEGER", true, false),
    );
    assert!(!seeks(&p));
    assert!(has(&p, Opcode::Rewind));
}

// --- range_scan_348: BETWEEN operands match column affinity ----------

#[test]
fn mcdc__range_scan_348__v1_both_bounds_match_integer_affinity_seeks() {
    let p = compile(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(seeks(&p));
}

#[test]
fn mcdc__range_scan_348__v2_text_lower_bound_against_integer_column_scans() {
    let p = compile(
        "SELECT b FROM t WHERE a BETWEEN 'x' AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(!seeks(&p));
}

#[test]
fn mcdc__range_scan_348__v3_text_upper_bound_against_integer_column_scans() {
    let p = compile(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 'x'",
        &schema("INTEGER", true, false),
    );
    assert!(!seeks(&p));
}

// --- range_scan_445: row-seek (UPDATE) BETWEEN operands supported -----

#[test]
fn mcdc__range_scan_445__v1_update_between_literals_seeks() {
    let p = compile_upd(
        "UPDATE t SET b = 'z' WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(seeks(&p));
}

#[test]
fn mcdc__range_scan_445__v2_update_computed_lower_bound_scans() {
    let p = compile_upd(
        "UPDATE t SET b = 'z' WHERE a BETWEEN 1 + 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(!seeks(&p));
}

#[test]
fn mcdc__range_scan_445__v3_update_computed_upper_bound_scans() {
    let p = compile_upd(
        "UPDATE t SET b = 'z' WHERE a BETWEEN 1 AND 5 + 1",
        &schema("INTEGER", true, false),
    );
    assert!(!seeks(&p));
}

// --- range_scan_455: row-seek (UPDATE) BETWEEN affinity ---------------

#[test]
fn mcdc__range_scan_455__v1_update_bounds_match_affinity_seeks() {
    let p = compile_upd(
        "UPDATE t SET b = 'z' WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(seeks(&p));
}

#[test]
fn mcdc__range_scan_455__v2_update_text_lower_bound_scans() {
    let p = compile_upd(
        "UPDATE t SET b = 'z' WHERE a BETWEEN 'x' AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(!seeks(&p));
}

#[test]
fn mcdc__range_scan_455__v3_update_text_upper_bound_scans() {
    let p = compile_upd(
        "UPDATE t SET b = 'z' WHERE a BETWEEN 1 AND 'x'",
        &schema("INTEGER", true, false),
    );
    assert!(!seeks(&p));
}

// --- range_scan_586: LIKE prefix contains wildcard / single-char ------

#[test]
fn mcdc__range_scan_586__v1_plain_prefix_seeks() {
    let p = compile(
        "SELECT b FROM t WHERE a LIKE 'ab%'",
        &schema("TEXT", true, false),
    );
    assert!(seeks(&p));
}

#[test]
fn mcdc__range_scan_586__v2_prefix_with_inner_percent_scans() {
    let p = compile(
        "SELECT b FROM t WHERE a LIKE 'a%b%'",
        &schema("TEXT", true, false),
    );
    assert!(!seeks(&p));
}

#[test]
fn mcdc__range_scan_586__v3_prefix_with_underscore_scans() {
    let p = compile(
        "SELECT b FROM t WHERE a LIKE 'a_b%'",
        &schema("TEXT", true, false),
    );
    assert!(!seeks(&p));
}

// --- range_scan_589: `!glob && prefix has backslash` ------------------

#[test]
fn mcdc__range_scan_589__v1_like_with_backslash_in_prefix_scans() {
    let p = compile(
        "SELECT b FROM t WHERE a LIKE 'a\\b%'",
        &schema("TEXT", true, false),
    );
    assert!(!seeks(&p));
}

#[test]
fn mcdc__range_scan_589__v2_glob_with_backslash_in_prefix_seeks() {
    let p = compile(
        "SELECT b FROM t WHERE a GLOB 'a\\b*'",
        &schema("TEXT", true, false),
    );
    assert!(seeks(&p));
}

#[test]
fn mcdc__range_scan_589__v3_like_without_backslash_seeks() {
    let p = compile(
        "SELECT b FROM t WHERE a LIKE 'ab%'",
        &schema("TEXT", true, false),
    );
    assert!(seeks(&p));
}

// --- range_scan_1013: range_seek_index_position BETWEEN operands ------

#[test]
fn mcdc__range_scan_1013__v1_literal_bounds_pick_the_index() {
    let s = schema("INTEGER", true, false);
    let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 AND 5");
    assert_eq!(range_seek_index_position(&w, &s), Some(0));
}

#[test]
fn mcdc__range_scan_1013__v2_computed_lower_bound_picks_no_index() {
    let s = schema("INTEGER", true, false);
    let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 + 1 AND 5");
    assert_eq!(range_seek_index_position(&w, &s), None);
}

#[test]
fn mcdc__range_scan_1013__v3_computed_upper_bound_picks_no_index() {
    let s = schema("INTEGER", true, false);
    let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 AND 5 + 1");
    assert_eq!(range_seek_index_position(&w, &s), None);
}

// --- range_scan_1018: range_seek_index_position BETWEEN affinity ------

#[test]
fn mcdc__range_scan_1018__v1_matching_affinity_picks_the_index() {
    let s = schema("INTEGER", true, false);
    let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 AND 5");
    assert_eq!(range_seek_index_position(&w, &s), Some(0));
}

#[test]
fn mcdc__range_scan_1018__v2_text_lower_bound_picks_no_index() {
    let s = schema("INTEGER", true, false);
    let w = where_of("SELECT b FROM t WHERE a BETWEEN 'x' AND 5");
    assert_eq!(range_seek_index_position(&w, &s), None);
}

#[test]
fn mcdc__range_scan_1018__v3_text_upper_bound_picks_no_index() {
    let s = schema("INTEGER", true, false);
    let w = where_of("SELECT b FROM t WHERE a BETWEEN 1 AND 'x'");
    assert_eq!(range_seek_index_position(&w, &s), None);
}

// --- range_scan_1048: range_seek_index_position IN list ---------------

#[test]
fn mcdc__range_scan_1048__v1_non_empty_literal_list_picks_the_index() {
    let s = schema("INTEGER", true, false);
    let w = where_of("SELECT b FROM t WHERE a IN (1, 2)");
    assert_eq!(range_seek_index_position(&w, &s), Some(0));
}

#[test]
fn mcdc__range_scan_1048__v2_empty_list_picks_no_index() {
    let s = schema("INTEGER", true, false);
    let w = Expr {
        kind: ExprKind::In {
            expr: Box::new(column_expr("a")),
            list: vec![],
            negated: false,
        },
        span: column_expr("a").span,
    };
    assert_eq!(range_seek_index_position(&w, &s), None);
}

#[test]
fn mcdc__range_scan_1048__v3_computed_list_member_picks_no_index() {
    let s = schema("INTEGER", true, false);
    let w = where_of("SELECT b FROM t WHERE a IN (1, 1 + 1)");
    assert_eq!(range_seek_index_position(&w, &s), None);
}

// --- range_scan_1099: EQP BETWEEN operands supported ------------------

#[test]
fn mcdc__range_scan_1099__v1_eqp_reports_index_search_for_literal_bounds() {
    let d = eqp_detail(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(d.contains("USING INDEX idx"), "{d}");
}

#[test]
fn mcdc__range_scan_1099__v2_eqp_reports_scan_for_computed_lower_bound() {
    let d = eqp_detail(
        "SELECT b FROM t WHERE a BETWEEN 1 + 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(!d.contains("USING INDEX"), "{d}");
}

#[test]
fn mcdc__range_scan_1099__v3_eqp_reports_scan_for_computed_upper_bound() {
    let d = eqp_detail(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 5 + 1",
        &schema("INTEGER", true, false),
    );
    assert!(!d.contains("USING INDEX"), "{d}");
}

// --- range_scan_1105: EQP BETWEEN affinity ----------------------------

#[test]
fn mcdc__range_scan_1105__v1_eqp_reports_index_search_when_affinity_matches() {
    let d = eqp_detail(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(d.contains("USING INDEX idx"), "{d}");
}

#[test]
fn mcdc__range_scan_1105__v2_eqp_reports_scan_for_text_lower_bound() {
    let d = eqp_detail(
        "SELECT b FROM t WHERE a BETWEEN 'x' AND 5",
        &schema("INTEGER", true, false),
    );
    assert!(!d.contains("USING INDEX"), "{d}");
}

#[test]
fn mcdc__range_scan_1105__v3_eqp_reports_scan_for_text_upper_bound() {
    let d = eqp_detail(
        "SELECT b FROM t WHERE a BETWEEN 1 AND 'x'",
        &schema("INTEGER", true, false),
    );
    assert!(!d.contains("USING INDEX"), "{d}");
}

// --- range_scan_1142: EQP IN list -------------------------------------

#[test]
fn mcdc__range_scan_1142__v1_eqp_reports_index_search_for_literal_list() {
    let d = eqp_detail(
        "SELECT b FROM t WHERE a IN (1, 2)",
        &schema("INTEGER", true, false),
    );
    assert!(d.contains("USING INDEX idx"), "{d}");
}

#[test]
fn mcdc__range_scan_1142__v2_eqp_reports_scan_for_empty_list() {
    let s = schema("INTEGER", true, false);
    let mut sel = select("SELECT b FROM t WHERE a IN (1)");
    sel.where_clause = Some(Expr {
        kind: ExprKind::In {
            expr: Box::new(column_expr("a")),
            list: vec![],
            negated: false,
        },
        span: column_expr("a").span,
    });
    let rows = explain_query_plan(
        &sel,
        std::slice::from_ref(&s),
        &HashMap::new(),
        std::slice::from_ref(&s),
    )
    .unwrap();
    let d = rows.first().map(|r| r.detail.clone()).unwrap_or_default();
    assert!(!d.contains("USING INDEX"), "{d}");
}

#[test]
fn mcdc__range_scan_1142__v3_eqp_reports_scan_for_computed_list_member() {
    let d = eqp_detail(
        "SELECT b FROM t WHERE a IN (1, 1 + 1)",
        &schema("INTEGER", true, false),
    );
    assert!(!d.contains("USING INDEX"), "{d}");
}

// --- projection_78: `pseudo && rowid_alias == idx` --------------------

#[test]
fn mcdc__projection_78__v1_sorted_rowid_alias_is_re_read_as_a_pseudo_column() {
    let p = compile(
        "SELECT a FROM t ORDER BY b",
        &schema("INTEGER", false, true),
    );
    assert!(has(&p, Opcode::OpenPseudo));
    // Pass 1 reads the alias with `Rowid` off the real cursor exactly
    // once; pass 2 (pseudo) reads it back as a plain `Column`.
    let rowids = p
        .instructions
        .iter()
        .filter(|i| i.opcode == Opcode::Rowid)
        .count();
    assert_eq!(rowids, 1);
}

#[test]
fn mcdc__projection_78__v2_unsorted_rowid_alias_is_read_via_rowid() {
    let p = compile("SELECT a FROM t", &schema("INTEGER", false, true));
    assert!(!has(&p, Opcode::OpenPseudo));
    assert!(has(&p, Opcode::Rowid));
}

#[test]
fn mcdc__projection_78__v3_sorted_ordinary_column_never_touches_rowid() {
    let p = compile(
        "SELECT a FROM t ORDER BY b",
        &schema("INTEGER", false, false),
    );
    assert!(has(&p, Opcode::OpenPseudo));
    assert!(!has(&p, Opcode::Rowid));
}

// --- limit_scan_101: rowid / _rowid_ / oid ----------------------------

#[test]
fn mcdc__limit_scan_101__v1_rowid_is_a_rowid_reference() {
    assert!(is_rowid_reference(
        &schema("INTEGER", false, false),
        &column_expr("ROWID")
    ));
}

#[test]
fn mcdc__limit_scan_101__v2_underscore_rowid_is_a_rowid_reference() {
    assert!(is_rowid_reference(
        &schema("INTEGER", false, false),
        &column_expr("_rowid_")
    ));
}

#[test]
fn mcdc__limit_scan_101__v3_oid_is_a_rowid_reference() {
    assert!(is_rowid_reference(
        &schema("INTEGER", false, false),
        &column_expr("oid")
    ));
}

#[test]
fn mcdc__limit_scan_101__v4_ordinary_column_without_alias_is_not() {
    assert!(!is_rowid_reference(
        &schema("INTEGER", false, false),
        &column_expr("b")
    ));
}
