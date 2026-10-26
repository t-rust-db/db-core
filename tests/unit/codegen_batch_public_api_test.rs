// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for `codegen::batch`'s public entry points -- the
//! default-feature columnar planner -- which had zero `tests/unit`
//! references before this (db-core#223): `compile`, `compile_join`,
//! `compile_semi_join`, `compile_window`, `explain`, `explain_opcodes`,
//! `output_column_names`, `expand_star`, `split_qualified`,
//! `WindowFunc`, `PlanError`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use db_core::codegen::batch::{
    compile, compile_join, compile_semi_join, compile_window, expand_star, explain,
    explain_opcodes, output_column_names, resolve_join_columns, split_qualified, BuildSourceKind,
    JoinColumnsError, PlanError, TableStats, WindowFunc,
};
use db_core::parser::column::parse;
use db_core::vm::batch::{JoinKind, MapOp, Opcode};

fn select(sql: &str) -> db_core::parser::ast::Select {
    parse(sql).unwrap()
}

#[test]
fn compile_produces_a_program_ending_in_combine() {
    let program = compile(&select(
        "SELECT product, SUM(amount) FROM sales GROUP BY product",
    ))
    .unwrap();
    assert!(program
        .instructions
        .iter()
        .any(|i| matches!(i.opcode, Opcode::Combine { .. })));
    assert!(program.columns_to_load().contains(&"product".to_string()));
}

#[test]
fn compile_where_like_emits_a_like_map_op() {
    let program = compile(&select("SELECT x FROM t WHERE x LIKE '%foo%'")).unwrap();
    assert!(program.instructions.iter().any(|i| matches!(
        i.opcode,
        Opcode::Map {
            op: MapOp::Like {
                negated: false,
                nocase: false
            },
            ..
        }
    )));
}

#[test]
fn compile_where_not_like_emits_a_negated_like_map_op() {
    let program = compile(&select("SELECT x FROM t WHERE x NOT LIKE '%foo%'")).unwrap();
    assert!(program.instructions.iter().any(|i| matches!(
        i.opcode,
        Opcode::Map {
            op: MapOp::Like {
                negated: true,
                nocase: false
            },
            ..
        }
    )));
}

#[test]
fn compile_join_builds_build_and_probe_programs() {
    let plan = compile_join(
        &select("SELECT a.x, b.y FROM a JOIN b ON a.id = b.fk"),
        BuildSourceKind::InMemory,
    )
    .unwrap();
    assert!(!plan.left_columns.is_empty());
    assert!(!plan.right_columns.is_empty());
    assert!(!plan.build.instructions.is_empty());
    assert!(!plan.probe.instructions.is_empty());
}

#[test]
fn compile_join_rejects_a_query_with_no_join() {
    let err = compile_join(&select("SELECT x FROM a"), BuildSourceKind::InMemory).unwrap_err();
    assert!(matches!(err, PlanError::NoJoinClause));
}

#[test]
fn compile_semi_join_plans_a_bare_in_subquery_where_clause() {
    let plan =
        compile_semi_join(&select("SELECT x FROM a WHERE id IN (SELECT id FROM b)")).unwrap();
    assert_eq!(plan.key_column, "id");
    assert!(!plan.body.instructions.is_empty());
}

#[test]
fn compile_semi_join_rejects_a_non_semi_join_where_clause() {
    let err = compile_semi_join(&select("SELECT x FROM a WHERE x > 1")).unwrap_err();
    assert!(matches!(err, PlanError::UnsupportedSemiJoin(_)));
}

#[test]
fn compile_window_plans_row_number_over_a_partition() {
    let program = compile_window(&select(
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY grp ORDER BY id) FROM t",
    ))
    .unwrap();
    assert!(program
        .instructions
        .iter()
        .any(|i| matches!(i.opcode, Opcode::Window { .. })));
}

#[test]
fn output_column_names_labels_every_select_item() {
    let names = output_column_names(&select("SELECT a, b FROM t"));
    assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn split_qualified_separates_a_table_prefix_from_a_column_name() {
    assert_eq!(split_qualified("t.a"), (Some("t"), "a"));
    assert_eq!(split_qualified("a"), (None, "a"));
}

#[test]
fn expand_star_replaces_a_bare_star_with_every_schema_column() {
    let expanded = expand_star(
        &select("SELECT * FROM t"),
        &["a".to_string(), "b".to_string()],
    )
    .unwrap();
    assert_eq!(output_column_names(&expanded), vec!["a", "b"]);
}

#[test]
fn window_func_from_name_and_niladic_classification() {
    assert_eq!(
        WindowFunc::from_name("row_number"),
        Some(WindowFunc::RowNumber)
    );
    assert_eq!(WindowFunc::from_name("lag"), Some(WindowFunc::Lag));
    assert_eq!(WindowFunc::from_name("not_a_function"), None);
    assert!(WindowFunc::RowNumber.is_niladic());
    assert!(!WindowFunc::Lag.is_niladic());
}

#[test]
fn explain_builds_a_scan_plan_tree_for_a_flat_query() {
    let plan = explain(&select("SELECT a FROM t"), |_| TableStats {
        row_groups: 1,
        rows: 10,
        source: None,
    })
    .unwrap();
    assert!(plan.iter().any(|node| node.detail.contains("SCAN")));
}

/// #315, ADR-0019: a cross-mode join's plan tree must name each side's
/// mode and file, not just its table name -- `SCAN log` and `SCAN hosts`
/// alone don't say one is a live tail and the other a SQLite lookup table.
/// `TableStats::source` is the per-table label the caller supplies (the
/// row→batch adapter's/stream adapter's own metadata); a single-mode
/// engine passes `None` and gets today's plain `SCAN` detail unchanged.
#[test]
fn explain_labels_each_side_of_a_join_by_source_mode_and_file() {
    let plan = explain(
        &select("SELECT log.message, hosts.region FROM log JOIN hosts ON log.host = hosts.name"),
        |table| match table {
            "log" => TableStats {
                row_groups: 3,
                rows: 4096,
                source: Some("stream app.log".to_string()),
            },
            _ => TableStats {
                row_groups: 1,
                rows: 3,
                source: Some("sqlite hosts.sqlite".to_string()),
            },
        },
    )
    .unwrap();
    let details: Vec<&str> = plan.iter().map(|n| n.detail.as_str()).collect();
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SCAN log") && d.ends_with("[stream app.log]")),
        "{details:?}"
    );
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SCAN hosts") && d.ends_with("[sqlite hosts.sqlite]")),
        "{details:?}"
    );
}

#[test]
fn explain_opcodes_reports_one_section_for_a_flat_query() {
    let sections = explain_opcodes(&select("SELECT a FROM t")).unwrap();
    assert_eq!(sections.len(), 1);
    assert!(!sections[0].rows.is_empty());
}

#[test]
fn explain_opcodes_reports_build_and_probe_sections_for_a_join() {
    let sections =
        explain_opcodes(&select("SELECT a.x, b.y FROM a JOIN b ON a.id = b.fk")).unwrap();
    assert!(sections.iter().any(|s| s.label.contains("JOIN build")));
    assert!(sections.iter().any(|s| s.label.contains("JOIN probe")));
}

#[test]
fn compile_join_reports_unsupported_join_kind_as_a_typed_error() {
    // `JoinKind` re-exported from `vm::batch` for the planner's own
    // `PlanError::UnsupportedJoinKind` payload -- `Right`/`Full`/`Cross`
    // parse but only `Inner`/`Left` execute.
    let _ = JoinKind::Inner;
    let err = compile_join(
        &select("SELECT a.x, b.y FROM a RIGHT JOIN b ON a.id = b.fk"),
        BuildSourceKind::InMemory,
    )
    .unwrap_err();
    assert!(matches!(err, PlanError::UnsupportedJoinKind(_)));
}

/// db-core#232: `compile`/`explain` are fallible. A select item the batch
/// planner cannot classify -- an unknown function name, neither an
/// aggregate, a window function, nor one of the #307 scalar functions --
/// is a `PlanError`, not a program that silently emits nothing.
#[test]
fn compile_and_explain_reject_a_select_item_the_planner_cannot_classify() {
    let db_core::parser::row::ParseOutcome::Accepted(select) =
        db_core::parser::row::parse_select("SELECT frobnicate(amount) FROM sales")
    else {
        panic!("row grammar accepts a function call");
    };
    assert!(matches!(
        compile(&select),
        Err(PlanError::UnsupportedSelectItem(_))
    ));
    assert!(matches!(
        explain(&select, |_| TableStats {
            row_groups: 1,
            rows: 10,
            source: None,
        }),
        Err(PlanError::UnsupportedSelectItem(_))
    ));
}

/// #307: a `SELECT`-list column alias (`AS <alias>`) now compiles and
/// renders its output header as the alias, rather than being rejected
/// outright (the pre-#307 behavior `compile_and_explain_reject_a_select_
/// item_the_planner_cannot_classify` above used to cover).
#[test]
fn compile_accepts_a_column_alias_and_uses_it_as_the_output_header() {
    let db_core::parser::row::ParseOutcome::Accepted(select) =
        db_core::parser::row::parse_select("SELECT amount AS total FROM sales")
    else {
        panic!("row grammar accepts a column alias");
    };
    assert!(compile(&select).is_ok());
    assert_eq!(output_column_names(&select), vec!["total".to_string()]);
}

// ---- resolve_join_columns (#778): join name resolution without an engine ----

fn tables() -> Vec<(String, Vec<String>)> {
    let cols = |c: &[&str]| c.iter().map(|s| s.to_string()).collect();
    vec![
        ("a".to_string(), cols(&["id", "x"])),
        ("b".to_string(), cols(&["id", "y"])),
    ]
}

fn resolved(sql: &str) -> Result<(Vec<String>, Vec<String>), JoinColumnsError> {
    let r = resolve_join_columns(&select(sql), &tables())?;
    let rendered = r.select.columns.iter().map(|c| format!("{c:?}")).collect();
    Ok((r.headers, rendered))
}

#[test]
fn join_columns_ambiguous_name_is_an_error() {
    let err = resolved("SELECT id FROM a JOIN b ON a.id = b.id").unwrap_err();
    assert_eq!(
        err.to_string(),
        "ambiguous column name: id (in a, b); qualify it with its table"
    );
    let err = resolved("SELECT x FROM a JOIN b ON a.id = b.id WHERE id = 1").unwrap_err();
    assert!(matches!(err, JoinColumnsError::AmbiguousColumn { .. }));
}

#[test]
fn join_columns_using_key_is_the_first_tables() {
    let (headers, items) = resolved("SELECT id FROM a JOIN b USING (id)").unwrap();
    assert_eq!(headers, ["id"]);
    assert!(items[0].contains("\"a\""), "{items:?}");
}

#[test]
fn join_columns_unique_name_resolves_to_its_table() {
    let (_, items) = resolved("SELECT y FROM a JOIN b ON a.id = b.id").unwrap();
    assert!(items[0].contains("\"b\""), "{items:?}");
    let (_, items) = resolved("SELECT y FROM b JOIN a ON a.id = b.id").unwrap();
    assert!(items[0].contains("\"b\""), "{items:?}");
}

#[test]
fn join_columns_star_lists_each_table_with_bare_headers() {
    let (headers, _) = resolved("SELECT * FROM a JOIN b ON a.id = b.id").unwrap();
    assert_eq!(headers, ["id", "x", "id", "y"]);
    let (headers, _) = resolved("SELECT * FROM a JOIN b USING (id)").unwrap();
    assert_eq!(headers, ["id", "x", "y"]);
    let (headers, _) = resolved("SELECT b.* FROM a JOIN b USING (id)").unwrap();
    assert_eq!(headers, ["id", "y"]);
    let err = resolved("SELECT z.* FROM a JOIN b USING (id)").unwrap_err();
    assert_eq!(err.to_string(), "unknown table in z.*: z");
}

#[test]
fn join_columns_headers_are_taken_before_qualification() {
    let (headers, _) = resolved("SELECT SUM(y) FROM a JOIN b ON a.id = b.id").unwrap();
    assert_eq!(headers, ["SUM(y)"]);
}

#[test]
fn join_columns_qualifies_every_expression_shape() {
    let (headers, items) = resolved(
        "SELECT (y), x FROM a JOIN b ON a.id = b.id \
         WHERE x BETWEEN 1 AND 9 AND y IN (1, 2) AND y LIKE 'a%' AND x > 0",
    )
    .unwrap();
    assert_eq!(headers, ["y", "x"]);
    assert!(
        items.iter().all(|i| !i.contains("table: None")),
        "{items:?}"
    );
    let (headers, _) = resolved(
        "SELECT y, COUNT(DISTINCT x) FILTER (WHERE y > 0) FROM a JOIN b ON a.id = b.id GROUP BY y",
    )
    .unwrap();
    assert_eq!(headers, ["y", "COUNT(DISTINCT x)"]);
}

#[test]
fn join_columns_group_by_resolves_and_keeps_headers() {
    let (headers, _) =
        resolved("SELECT y, SUM(x) FROM a JOIN b ON a.id = b.id GROUP BY y ORDER BY y").unwrap();
    assert_eq!(headers, ["y", "SUM(x)"]);
}

#[test]
fn join_columns_wraps_a_planner_error() {
    let err = JoinColumnsError::from(PlanError::NoJoinClause);
    assert_eq!(err, JoinColumnsError::Plan(PlanError::NoJoinClause));
    assert_eq!(err.to_string(), PlanError::NoJoinClause.to_string());
}
