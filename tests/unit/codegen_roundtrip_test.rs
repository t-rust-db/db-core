// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box compile-then-execute round trips through `codegen::row::
//! dispatch::compile_statement`, the entry point a downstream consumer
//! (sqlite-rs) actually calls: SQL string in, `Program` out, then run
//! against a real cursor and check the resulting rows -- not the
//! opcodes. `dispatch.rs`'s own inline `end_to_end` module already
//! covers a plain `SELECT`/`JOIN`/`INSERT`/`UPDATE`/`DELETE`; this suite
//! fills the gap around it: `GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`+
//! `OFFSET`, and the richer `WHERE`/`SELECT`-list expression shapes
//! (`CASE`, `IN`, `BETWEEN`, a function call, a scalar subquery) that
//! `codegen::row::mod`'s `walk_columns`/`walk_subexprs` only cover
//! exhaustively when a query actually contains them -- both were among
//! the weakest-covered files in the crate (71.5%/79.3% lines) before
//! this suite existed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use db_core::codegen::row::dispatch::compile_statement;
use db_core::codegen::row::TableSchema;
use db_core::value::Value;
use db_core::vm::row::{execute, Cursor, EphemeralTableCursor, Opcode, Program, Vm};

fn schema(name: &str, columns: &[&str]) -> TableSchema {
    schema_with_root(name, columns, 2)
}

fn schema_with_root(name: &str, columns: &[&str], root_page: u32) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        column_types: columns.iter().map(|_| String::new()).collect(),
        root_page,
        sql: format!("CREATE TABLE {name} ({})", columns.join(", ")),
        ..Default::default()
    }
}

/// The cursor slot the compiler assigned to the table rooted at
/// `root_page` -- a query with more than one scan (a `JOIN`, a
/// subquery, ...) doesn't allocate cursor slots in catalog order, so
/// this reads it back from the compiled program's own `OpenRead`
/// instead of assuming a slot number.
fn cursor_slot_for_root(program: &Program, root_page: u32) -> i32 {
    program
        .instructions
        .iter()
        .find(|i| i.opcode == Opcode::OpenRead && i.p2 == i32::try_from(root_page).unwrap())
        .map(|i| i.p1)
        .unwrap_or_else(|| panic!("no OpenRead for root page {root_page} in {program:?}"))
}

fn run(schemas: &[TableSchema], sql: &str, seed: Vec<(i64, Vec<Value>)>) -> Vec<Vec<Value>> {
    let program = compile_statement(sql, schemas, &[]).unwrap();
    let mut vm = Vm::new();
    let mut table = EphemeralTableCursor::new();
    for (rowid, values) in seed {
        table.insert(rowid, values);
    }
    vm.open_cursor(0, Box::new(table)).unwrap();
    execute(&mut vm, &program).unwrap()
}

#[test]
fn group_by_with_having_and_an_aggregate() {
    let rows = run(
        &[schema("t", &["k", "v"])],
        "SELECT k, SUM(v) FROM t GROUP BY k HAVING SUM(v) > 5",
        vec![
            (1, vec![Value::Integer(1), Value::Integer(10)]),
            (2, vec![Value::Integer(2), Value::Integer(1)]),
            (3, vec![Value::Integer(2), Value::Integer(2)]),
        ],
    );
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(10)]]);
}

#[test]
fn order_by_desc_with_limit_and_offset() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT a FROM t ORDER BY a DESC LIMIT 2 OFFSET 1",
        vec![
            (1, vec![Value::Integer(1)]),
            (2, vec![Value::Integer(4)]),
            (3, vec![Value::Integer(2)]),
            (4, vec![Value::Integer(3)]),
        ],
    );
    assert_eq!(rows, vec![vec![Value::Integer(3)], vec![Value::Integer(2)]]);
}

#[test]
fn where_clause_with_between_and_in() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT a FROM t WHERE a BETWEEN 2 AND 4 AND a IN (2, 3)",
        vec![
            (1, vec![Value::Integer(1)]),
            (2, vec![Value::Integer(2)]),
            (3, vec![Value::Integer(3)]),
            (4, vec![Value::Integer(5)]),
        ],
    );
    assert_eq!(rows, vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]);
}

#[test]
fn select_list_case_expression_and_function_call() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT CASE WHEN a > 1 THEN 'big' ELSE 'small' END, abs(a) FROM t",
        vec![(1, vec![Value::Integer(-1)]), (2, vec![Value::Integer(2)])],
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("small".into()), Value::Integer(1)],
            vec![Value::Text("big".into()), Value::Integer(2)],
        ]
    );
}

#[test]
fn where_clause_scalar_subquery() {
    let schemas = [
        schema_with_root("t", &["a"], 2),
        schema_with_root("bound", &["n"], 3),
    ];
    let program = compile_statement(
        "SELECT a FROM t WHERE a > (SELECT n FROM bound)",
        &schemas,
        &[],
    )
    .unwrap();

    // The outer table is the caller's pre-wired cursor 0 (no `OpenRead`
    // of its own -- codegen's compiled-ahead-of-time path); only the
    // nested subquery scan gets an explicit `OpenRead`.
    let mut vm = Vm::new();
    let mut t = EphemeralTableCursor::new();
    t.insert(1, vec![Value::Integer(5)]);
    t.insert(2, vec![Value::Integer(15)]);
    vm.open_cursor(0, Box::new(t)).unwrap();
    let mut bound = EphemeralTableCursor::new();
    bound.insert(1, vec![Value::Integer(10)]);
    vm.open_cursor(cursor_slot_for_root(&program, 3), Box::new(bound))
        .unwrap();

    let rows = execute(&mut vm, &program).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(15)]]);
}

/// #281: the implicit-group aggregate scan peels its first matching row
/// out of the loop. The first *match* is not the first *row*, later
/// matches fold, a bare column snapshots the first matching row, and a
/// zero-match scan still flushes one row (`count(*) = 0`, others NULL).
#[test]
fn implicit_group_aggregate_peels_first_match_and_keeps_zero_row_flush() {
    let t = || schema("t", &["a", "b"]);
    let seed = || -> Vec<(i64, Vec<Value>)> {
        (1..=6)
            .map(|i| {
                (
                    i,
                    vec![Value::Integer(i * 10), Value::Text(format!("b{i}").into())],
                )
            })
            .collect()
    };
    // rows 1-2 fail the predicate; first match is row 3 (a = 30, b = "b3").
    let rows = run(
        &[t()],
        "SELECT count(*), sum(a), b FROM t WHERE a > 25",
        seed(),
    );
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(4),
            Value::Integer(30 + 40 + 50 + 60),
            Value::Text("b3".into()),
        ]]
    );
    // only the last row matches: the steady-state pass runs zero times.
    let rows = run(
        &[t()],
        "SELECT count(*), max(a) FROM t WHERE a > 55",
        seed(),
    );
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(60)]]);
    // nothing matches: one row, count 0, other aggregates NULL.
    let rows = run(
        &[t()],
        "SELECT count(*), sum(a) FROM t WHERE a > 100",
        seed(),
    );
    assert_eq!(rows, vec![vec![Value::Integer(0), Value::Null]]);
    // no WHERE: every row folds.
    let rows = run(&[t()], "SELECT count(*), min(a) FROM t", seed());
    assert_eq!(rows, vec![vec![Value::Integer(6), Value::Integer(10)]]);
}

#[test]
fn insert_then_group_by_select_sees_the_new_row() {
    let schemas = [schema("t", &["k", "v"])];
    let insert = compile_statement("INSERT INTO t VALUES (1, 7)", &schemas, &[]).unwrap();
    let mut vm = Vm::new();
    vm.open_cursor(0, Box::new(EphemeralTableCursor::new()))
        .unwrap();
    execute(&mut vm, &insert).unwrap();

    let select = compile_statement("SELECT k, SUM(v) FROM t GROUP BY k", &schemas, &[]).unwrap();
    let rows = execute(&mut vm, &select).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1), Value::Integer(7)]]);
}

#[test]
fn compile_statement_reports_an_unknown_table() {
    let err = compile_statement("SELECT a FROM missing", &[], &[]).unwrap_err();
    assert!(format!("{err:?}").contains("missing"));
}

// ---------------------------------------------------------------------
// db-core#219 carry-overs: behaviour db-core's re-derived codegen had
// grown that sqlite-rs's tree lacked, re-added on top of the moved code.
// ---------------------------------------------------------------------

/// Two-table variant of [`run`]: `t` on root 2, `u` on root 3, each
/// seeded with `(rowid, values)` rows.
fn run_two(
    sql: &str,
    t: (&[&str], Vec<(i64, Vec<Value>)>),
    u: (&[&str], Vec<(i64, Vec<Value>)>),
) -> Vec<Vec<Value>> {
    let schemas = [schema_with_root("t", t.0, 2), schema_with_root("u", u.0, 3)];
    let program = compile_statement(sql, &schemas, &[]).unwrap();
    let mut vm = Vm::new();
    for (root, seed) in [(2, t.1), (3, u.1)] {
        let mut table = EphemeralTableCursor::new();
        for (rowid, values) in seed {
            table.insert(rowid, values);
        }
        vm.open_cursor(cursor_slot_for_root(&program, root), Box::new(table))
            .unwrap();
    }
    execute(&mut vm, &program).unwrap()
}

fn ints(values: &[i64]) -> Vec<Value> {
    values.iter().map(|v| Value::Integer(*v)).collect()
}

#[test]
fn having_filters_groups_over_a_join() {
    let rows = run_two(
        "SELECT t.k, count(*) FROM t JOIN u ON u.k = t.k GROUP BY t.k HAVING count(*) > 1",
        (&["k"], vec![(1, ints(&[1])), (2, ints(&[2]))]),
        (
            &["k", "v"],
            vec![
                (1, ints(&[1, 10])),
                (2, ints(&[1, 20])),
                (3, ints(&[2, 30])),
            ],
        ),
    );
    assert_eq!(rows, vec![ints(&[1, 2])]);
}

#[test]
fn having_over_a_join_may_name_a_bare_grouped_column() {
    let rows = run_two(
        "SELECT t.k, sum(u.v) FROM t JOIN u ON u.k = t.k GROUP BY t.k HAVING k = 2",
        (&["k"], vec![(1, ints(&[1])), (2, ints(&[2]))]),
        (&["k", "v"], vec![(1, ints(&[1, 10])), (2, ints(&[2, 30]))]),
    );
    assert_eq!(rows, vec![ints(&[2, 30])]);
}

#[test]
fn having_over_a_join_composes_with_order_by() {
    let rows = run_two(
        "SELECT t.k, count(*) FROM t JOIN u ON u.k = t.k GROUP BY t.k HAVING count(*) >= 1 \
         ORDER BY t.k DESC",
        (
            &["k"],
            vec![(1, ints(&[1])), (2, ints(&[2])), (3, ints(&[3]))],
        ),
        (
            &["k", "v"],
            vec![
                (1, ints(&[1, 10])),
                (2, ints(&[1, 20])),
                (3, ints(&[3, 30])),
            ],
        ),
    );
    assert_eq!(rows, vec![ints(&[3, 1]), ints(&[1, 2])]);
}

#[test]
fn distinct_with_order_by_over_a_join_dedups_the_sorted_output() {
    let rows = run_two(
        "SELECT DISTINCT u.v FROM t JOIN u ON u.k = t.k ORDER BY u.v DESC",
        (&["k"], vec![(1, ints(&[1])), (2, ints(&[2]))]),
        (
            &["k", "v"],
            vec![
                (1, ints(&[1, 7])),
                (2, ints(&[2, 7])),
                (3, ints(&[1, 9])),
                (4, ints(&[2, 3])),
            ],
        ),
    );
    assert_eq!(rows, vec![ints(&[9]), ints(&[7]), ints(&[3])]);
}

#[test]
fn distinct_with_order_by_and_limit_over_a_join_counts_only_distinct_rows() {
    let rows = run_two(
        "SELECT DISTINCT u.v FROM t JOIN u ON u.k = t.k ORDER BY u.v LIMIT 2",
        (&["k"], vec![(1, ints(&[1])), (2, ints(&[2]))]),
        (
            &["k", "v"],
            vec![
                (1, ints(&[1, 3])),
                (2, ints(&[2, 3])),
                (3, ints(&[1, 5])),
                (4, ints(&[2, 9])),
            ],
        ),
    );
    assert_eq!(rows, vec![ints(&[3]), ints(&[5])]);
}

#[test]
fn distinct_with_order_by_over_a_full_join_dedups_both_passes() {
    let rows = run_two(
        "SELECT DISTINCT u.v FROM t FULL JOIN u ON u.k = t.k ORDER BY u.v",
        (&["k"], vec![(1, ints(&[1])), (2, ints(&[5]))]),
        (
            &["k", "v"],
            vec![(1, ints(&[1, 7])), (2, ints(&[1, 7])), (3, ints(&[9, 2]))],
        ),
    );
    // t.k = 5 matches nothing (u.v is NULL for that row); u.k = 9 is the
    // unmatched right side (v = 2); the two k = 1 matches collapse.
    assert_eq!(rows, vec![vec![Value::Null], ints(&[2]), ints(&[7])]);
}

#[test]
fn in_subquery_may_project_a_computed_expression() {
    let rows = run_two(
        "SELECT t.k FROM t WHERE t.k IN (SELECT u.v + 1 FROM u)",
        (
            &["k"],
            vec![(1, ints(&[1])), (2, ints(&[2])), (3, ints(&[3]))],
        ),
        (&["k", "v"], vec![(1, ints(&[0, 1])), (2, ints(&[0, 2]))]),
    );
    assert_eq!(rows, vec![ints(&[2]), ints(&[3])]);
}

#[test]
fn scalar_subquery_may_project_a_computed_expression() {
    let rows = run_two(
        "SELECT t.k FROM t WHERE t.k = (SELECT u.v * 2 FROM u WHERE u.k = 1)",
        (&["k"], vec![(1, ints(&[1])), (2, ints(&[4]))]),
        (&["k", "v"], vec![(1, ints(&[1, 2])), (2, ints(&[2, 5]))]),
    );
    assert_eq!(rows, vec![ints(&[4])]);
}

// ---------------------------------------------------------------------
// #376: non-recursive `WITH` clause, rewritten away before codegen
// (`codegen::row::subquery::cte`).
// ---------------------------------------------------------------------

#[test]
fn with_clause_cte_referenced_once() {
    let rows = run(
        &[schema("t", &["a"])],
        "WITH big AS (SELECT a FROM t WHERE a > 1) SELECT a FROM big",
        vec![(1, ints(&[1])), (2, ints(&[2])), (3, ints(&[3]))],
    );
    assert_eq!(rows, vec![ints(&[2]), ints(&[3])]);
}

#[test]
fn with_clause_cte_referenced_without_explicit_alias_keeps_cte_name() {
    // `FROM big` (no `AS b`) still resolves `big.a` against the CTE's
    // own name -- `substitute_table_ref` defaults the alias to it.
    let rows = run(
        &[schema("t", &["a"])],
        "WITH big AS (SELECT a FROM t) SELECT big.a FROM big WHERE big.a > 1",
        vec![(1, ints(&[1])), (2, ints(&[2]))],
    );
    assert_eq!(rows, vec![ints(&[2])]);
}

#[test]
fn with_clause_later_cte_may_reference_an_earlier_one() {
    // `evens` materializes `t` once; `big_evens` materializes `evens`
    // (itself a subquery over `t`) again, so `t` ends up scanned by two
    // separate `OpenRead`s -- unlike this file's other `run()`-based
    // tests, the base table isn't necessarily cursor 0.
    let schemas = [schema("t", &["a"])];
    let program = compile_statement(
        "WITH \
           evens AS (SELECT a FROM t WHERE a % 2 = 0), \
           big_evens AS (SELECT a FROM evens WHERE a > 2) \
         SELECT a FROM big_evens",
        &schemas,
        &[],
    )
    .unwrap();

    let mut vm = Vm::new();
    for instr in &program.instructions {
        if instr.opcode == Opcode::OpenRead {
            let mut t = EphemeralTableCursor::new();
            for (rowid, values) in [
                (1, ints(&[1])),
                (2, ints(&[2])),
                (3, ints(&[4])),
                (4, ints(&[5])),
            ] {
                t.insert(rowid, values);
            }
            vm.open_cursor(instr.p1, Box::new(t)).unwrap();
        }
    }
    let rows = execute(&mut vm, &program).unwrap();
    assert_eq!(rows, vec![ints(&[4])]);
}

#[test]
fn with_clause_applies_explicit_column_aliases() {
    let rows = run(
        &[schema("t", &["a"])],
        "WITH renamed(x) AS (SELECT a FROM t) SELECT x FROM renamed WHERE x > 1",
        vec![(1, ints(&[1])), (2, ints(&[2]))],
    );
    assert_eq!(rows, vec![ints(&[2])]);
}

#[test]
fn with_clause_mismatched_column_alias_count_leaves_natural_names() {
    // `apply_column_aliases` only renames a same-length, all-`Expr`
    // result list; a mismatched count is left alone, so the CTE is
    // still queryable, just under its query's own column name.
    let rows = run(
        &[schema("t", &["a"])],
        "WITH renamed(x, y) AS (SELECT a FROM t) SELECT a FROM renamed",
        vec![(1, ints(&[5]))],
    );
    assert_eq!(rows, vec![ints(&[5])]);
}

#[test]
fn with_clause_over_a_union_all_compound_substitutes_every_arm() {
    // Each `UNION ALL` arm has its own `FROM cte` reference --
    // `substitute_cte_refs` walks `select.compound` as well as the main
    // query's own `FROM`, so both arms resolve `big`.
    let schemas = [schema("t", &["a"])];
    let program = compile_statement(
        "WITH big AS (SELECT a FROM t WHERE a > 1) \
         SELECT a FROM big UNION ALL SELECT a FROM big WHERE a > 2",
        &schemas,
        &[],
    )
    .unwrap();

    let mut vm = Vm::new();
    for instr in &program.instructions {
        if instr.opcode == Opcode::OpenRead {
            let mut t = EphemeralTableCursor::new();
            for (rowid, values) in [(1, ints(&[1])), (2, ints(&[2])), (3, ints(&[3]))] {
                t.insert(rowid, values);
            }
            vm.open_cursor(instr.p1, Box::new(t)).unwrap();
        }
    }
    let rows = execute(&mut vm, &program).unwrap();
    assert_eq!(rows, vec![ints(&[2]), ints(&[3]), ints(&[3])]);
}

#[test]
fn with_clause_shadows_a_real_table_of_the_same_name() {
    // The CTE `t` shadows the real table `t` for this statement only --
    // `substitute_table_ref` rewrites the `FROM t` reference to the
    // CTE's own (filtered) body rather than the real table.
    let rows = run(
        &[schema("t", &["a"])],
        "WITH t AS (SELECT a FROM t WHERE a > 1) SELECT a FROM t",
        vec![(1, ints(&[1])), (2, ints(&[2])), (3, ints(&[3]))],
    );
    assert_eq!(rows, vec![ints(&[2]), ints(&[3])]);
}

#[test]
fn without_a_with_clause_the_query_still_compiles() {
    // `expand_with_clause`'s `Cow::Borrowed` fast path for a `Select`
    // with no `WITH` clause at all.
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT a FROM t WHERE a > 1",
        vec![(1, ints(&[1])), (2, ints(&[2]))],
    );
    assert_eq!(rows, vec![ints(&[2])]);
}

// ---------------------------------------------------------------------
// codegen::row::expr::expr_value value-mode compilation: literal/param/
// operator shapes not otherwise reached by this file's condition-mode
// (`WHERE`) or aggregate tests.
// ---------------------------------------------------------------------

fn run_with_params(sql: &str, params: Vec<Value>, seed: Vec<(i64, Vec<Value>)>) -> Vec<Vec<Value>> {
    let schemas = [schema("t", &["a"])];
    let program = compile_statement(sql, &schemas, &[]).unwrap();
    let mut vm = Vm::new();
    vm.bind_params(params);
    let mut table = EphemeralTableCursor::new();
    for (rowid, values) in seed {
        table.insert(rowid, values);
    }
    vm.open_cursor(0, Box::new(table)).unwrap();
    execute(&mut vm, &program).unwrap()
}

#[test]
fn select_list_true_false_and_blob_literals() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT TRUE, FALSE, x'414243' FROM t",
        vec![(1, ints(&[1]))],
    );
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(0),
            Value::Blob(vec![0x41, 0x42, 0x43].into()),
        ]]
    );
}

#[test]
fn select_list_integer_literal_beyond_i32_uses_int64() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT 5000000000 FROM t",
        vec![(1, ints(&[1]))],
    );
    assert_eq!(rows, vec![vec![Value::Integer(5_000_000_000)]]);
}

#[test]
fn anonymous_and_numbered_parameters_read_bound_values() {
    let rows = run_with_params(
        "SELECT ?, ?1 FROM t",
        vec![Value::Integer(7)],
        vec![(1, ints(&[1]))],
    );
    assert_eq!(rows, vec![vec![Value::Integer(7), Value::Integer(7)]]);
}

#[test]
fn named_colon_parameter_is_a_known_simplification_always_null() {
    // #137's bounded scope: `:name`/`@name`/`$name` aren't wired to an
    // index, so they compile to an always-NULL register rather than an
    // error -- a documented simplification, not a bug.
    let rows = run_with_params("SELECT :missing FROM t", vec![], vec![(1, ints(&[1]))]);
    assert_eq!(rows, vec![vec![Value::Null]]);
}

#[test]
fn like_escape_and_negated_like_and_glob() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT a FROM t WHERE a LIKE '10%' ESCAPE '\\' OR a NOT LIKE '2%' OR a GLOB '3*'",
        vec![
            (1, vec![Value::Text("10x".into())]),
            (2, vec![Value::Text("20x".into())]),
            (3, vec![Value::Text("30x".into())]),
        ],
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("10x".into())],
            vec![Value::Text("30x".into())],
        ]
    );
}

#[test]
fn unary_plus_minus_not_and_bitnot() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT +a, -a, NOT a, ~a FROM t",
        vec![(1, vec![Value::Integer(5)])],
    );
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(5),
            Value::Integer(-5),
            Value::Integer(0),
            Value::Integer(-6),
        ]]
    );
}

#[test]
fn arithmetic_and_bitwise_and_concat_operators() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT a - 1, a / 2, a % 3, a & 1, a | 8, a << 1, a >> 1, a || 'x' FROM t",
        vec![(1, vec![Value::Integer(10)])],
    );
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(9),
            Value::Integer(5),
            Value::Integer(1),
            Value::Integer(0),
            Value::Integer(10),
            Value::Integer(20),
            Value::Integer(5),
            Value::Text("10x".into()),
        ]]
    );
}

#[test]
fn function_call_with_non_contiguously_compiled_arguments() {
    // `coalesce(a, -1)` compiles its second arg (unary minus) via
    // several intermediate registers of its own, so the two top-level
    // args don't land contiguously -- exercises `compile_value`'s
    // copy-into-a-fresh-run fallback for `FunctionCall`.
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT coalesce(a, -1) + coalesce(a, -1) FROM t",
        vec![(1, vec![Value::Null])],
    );
    assert_eq!(rows, vec![vec![Value::Integer(-2)]]);
}

#[test]
fn window_function_in_the_select_list_is_unsupported() {
    // A non-aggregate call with `OVER` reaches `expr_value.rs`'s own
    // `tail.over` check (an aggregate call with `OVER` is instead
    // caught earlier, as an aggregate-with-window rejection).
    let schemas = [schema("t", &["a"])];
    let err = compile_statement("SELECT abs(a) OVER () FROM t", &schemas, &[]).unwrap_err();
    assert!(format!("{err:?}").contains("window"), "{err:?}");
}

#[test]
fn comparison_and_logical_operators_in_the_select_list_materialize_0_1_or_null() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT a = 1, a <> 1, a IS NULL, a IS NOT NULL, a > 0 AND a < 2, a = NULL FROM t",
        vec![(1, vec![Value::Integer(1)]), (2, vec![Value::Null])],
    );
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::Integer(0),
                Value::Integer(0),
                Value::Integer(1),
                Value::Integer(1),
                Value::Null,
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Integer(1),
                Value::Integer(0),
                Value::Null,
                Value::Null,
            ],
        ]
    );
}

#[test]
fn cast_expression_forces_target_affinity() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT CAST(a AS INTEGER), CAST(a AS TEXT) FROM t",
        vec![(1, vec![Value::Text("42".into())])],
    );
    assert_eq!(
        rows,
        vec![vec![Value::Integer(42), Value::Text("42".into())]]
    );
}

#[test]
fn case_without_operand_matches_the_first_true_when() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT CASE WHEN a > 10 THEN 'big' WHEN a > 0 THEN 'small' ELSE 'non-positive' END FROM t",
        vec![
            (1, vec![Value::Integer(20)]),
            (2, vec![Value::Integer(5)]),
            (3, vec![Value::Integer(-1)]),
        ],
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("big".into())],
            vec![Value::Text("small".into())],
            vec![Value::Text("non-positive".into())],
        ]
    );
}

#[test]
fn case_with_operand_compares_equality_against_each_when() {
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT CASE a WHEN 1 THEN 'one' WHEN 2 THEN 'two' END FROM t",
        vec![(1, vec![Value::Integer(2)]), (2, vec![Value::Integer(3)])],
    );
    assert_eq!(
        rows,
        vec![vec![Value::Text("two".into())], vec![Value::Null]]
    );
}

#[test]
fn case_branch_result_kinds_cover_every_emit_branch_into_shape() {
    // Exercises every `emit_branch_into` literal/column/fallback arm in
    // one query: an integer literal, TRUE/FALSE, a string, a float, a
    // blob, an explicit NULL, a bare column, and a computed expression
    // (the `compile_value` + `Copy` fallback).
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT \
           CASE a WHEN 1 THEN 100 WHEN 2 THEN TRUE WHEN 3 THEN FALSE \
                  WHEN 4 THEN 'x' WHEN 5 THEN 1.5 WHEN 6 THEN x'ab' \
                  WHEN 7 THEN NULL WHEN 8 THEN a ELSE a + 1 END \
         FROM t",
        vec![
            (1, vec![Value::Integer(1)]),
            (2, vec![Value::Integer(2)]),
            (3, vec![Value::Integer(3)]),
            (4, vec![Value::Integer(4)]),
            (5, vec![Value::Integer(5)]),
            (6, vec![Value::Integer(6)]),
            (7, vec![Value::Integer(7)]),
            (8, vec![Value::Integer(8)]),
            (9, vec![Value::Integer(9)]),
        ],
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(100)],
            vec![Value::Integer(1)],
            vec![Value::Integer(0)],
            vec![Value::Text("x".into())],
            vec![Value::Real(1.5)],
            vec![Value::Blob(vec![0xab].into())],
            vec![Value::Null],
            vec![Value::Integer(8)],
            vec![Value::Integer(10)],
        ]
    );
}

#[test]
fn case_with_no_matching_when_and_no_else_is_null_and_does_not_leak_across_rows() {
    // #134's fix: `dest` is a reused register across scan iterations,
    // so a no-match/no-ELSE row must not see a prior row's result.
    let rows = run(
        &[schema("t", &["a"])],
        "SELECT CASE WHEN a = 1 THEN 'matched' END FROM t",
        vec![(1, vec![Value::Integer(1)]), (2, vec![Value::Integer(2)])],
    );
    assert_eq!(
        rows,
        vec![vec![Value::Text("matched".into())], vec![Value::Null]]
    );
}

#[test]
fn between_in_exists_and_in_subquery_in_the_select_list() {
    let schemas = [
        schema_with_root("t", &["a"], 2),
        schema_with_root("u", &["a"], 3),
    ];
    let program = compile_statement(
        "SELECT a BETWEEN 1 AND 3, a IN (2, 4), EXISTS (SELECT 1 FROM u), \
                a IN (SELECT a FROM u) \
         FROM t",
        &schemas,
        &[],
    )
    .unwrap();
    let mut vm = Vm::new();
    let mut t = EphemeralTableCursor::new();
    t.insert(1, ints(&[2]));
    t.insert(2, ints(&[9]));
    vm.open_cursor(0, Box::new(t)).unwrap();
    for instr in &program.instructions {
        if instr.opcode == Opcode::OpenRead && instr.p2 == 3 {
            let mut u = EphemeralTableCursor::new();
            u.insert(1, ints(&[2]));
            vm.open_cursor(instr.p1, Box::new(u)).unwrap();
        }
    }
    let rows = execute(&mut vm, &program).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::Integer(1),
                Value::Integer(1),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(0),
                Value::Integer(0),
                Value::Integer(1),
                Value::Integer(0),
            ],
        ]
    );
}

#[test]
fn scalar_subquery_in_the_select_list() {
    let schemas = [
        schema_with_root("t", &["a"], 2),
        schema_with_root("bound", &["n"], 3),
    ];
    let program = compile_statement("SELECT (SELECT n FROM bound) FROM t", &schemas, &[]).unwrap();
    let mut vm = Vm::new();
    let mut t = EphemeralTableCursor::new();
    t.insert(1, ints(&[1]));
    vm.open_cursor(0, Box::new(t)).unwrap();
    let mut bound = EphemeralTableCursor::new();
    bound.insert(1, ints(&[10]));
    vm.open_cursor(cursor_slot_for_root(&program, 3), Box::new(bound))
        .unwrap();
    let rows = execute(&mut vm, &program).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(10)]]);
}

#[test]
fn aggregate_used_as_a_plain_scalar_argument_is_unsupported() {
    // `abs(count(*))` reaches `compile_value` for the *inner* `count(*)`
    // in a context this V2 compiler's aggregate pass never sees.
    let schemas = [schema("t", &["a"])];
    let err = compile_statement("SELECT abs(count(*)) FROM t", &schemas, &[]).unwrap_err();
    assert!(format!("{err:?}").contains("count"), "{err:?}");
}
