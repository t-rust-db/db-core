// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Totality under pathological nesting (db-core#226): every public
//! entry point that recurses over the AST -- the row parser, the row
//! planner (`codegen::row::dispatch::compile_statement`, including its
//! `subquery::{flatten, pushdown}` passes) and the batch planner
//! (`codegen::batch::compile`) -- must either return a typed error or
//! succeed, never overflow the stack, on a **2 MiB thread** (cargo
//! test's own default, and what an embedding application's worker
//! thread typically has; the 8 MiB main thread hides the problem).
//!
//! The parser is the single choke point: `MAX_EXPR_DEPTH` bounds
//! expression nesting and, since #226, `SELECT_DEPTH_COST` charges each
//! subquery level against the same budget. Codegen therefore has no
//! guard of its own; what these tests prove is that codegen survives
//! *everything the parser accepts* -- the shapes here sit at or just
//! under the parser's cap, which is the deepest input codegen can ever
//! see.

#![allow(clippy::unwrap_used, clippy::panic)]

use db_core::codegen::batch;
use db_core::codegen::row::dispatch::compile_statement;
use db_core::codegen::row::TableSchema;
use db_core::parser::column;
use db_core::parser::row::{parse_select, ParseOutcome};

fn schema() -> TableSchema {
    TableSchema {
        name: "t".into(),
        columns: vec!["a".into()],
        column_types: vec![String::new()],
        root_page: 2,
        sql: "CREATE TABLE t (a)".into(),
        ..Default::default()
    }
}

/// `SELECT a FROM (SELECT a FROM (... FROM t) AS s1) AS s0`, `depth`
/// levels of derived table.
fn nested_from(depth: usize) -> String {
    let mut s = "SELECT a FROM t".to_string();
    for i in 0..depth {
        s = format!("SELECT a FROM ({s}) AS s{i}");
    }
    s
}

/// `SELECT (SELECT (... (SELECT a FROM t))) FROM t`, `depth` levels of
/// scalar subquery.
fn nested_scalar(depth: usize) -> String {
    let mut s = "SELECT a FROM t".to_string();
    for _ in 0..depth {
        s = format!("SELECT ({s})");
    }
    format!("{s} FROM t")
}

/// `SELECT (((...a...))) FROM t`, `depth` parentheses.
fn nested_paren(depth: usize) -> String {
    format!("SELECT {}a{} FROM t", "(".repeat(depth), ")".repeat(depth))
}

/// `SELECT a FROM t WHERE (((a = 1 AND a = 1) AND a = 1) ...)`, `depth`
/// nested conjunctions -- the shape `subquery::pushdown` walks.
fn nested_and(depth: usize) -> String {
    let mut s = "a = 1".to_string();
    for _ in 0..depth {
        s = format!("({s} AND a = 1)");
    }
    format!("SELECT a FROM t WHERE {s}")
}

fn on_2mib_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

fn is_too_deep(sql: &str) -> bool {
    matches!(
        parse_select(sql),
        ParseOutcome::Invalid { message, .. } if message.ends_with("nesting too deep")
    )
}

#[test]
fn parser_rejects_runaway_nesting_of_every_shape_with_a_typed_error() {
    on_2mib_stack(|| {
        for depth in [100usize, 1_000] {
            assert!(is_too_deep(&nested_from(depth)), "FROM {depth}");
            assert!(is_too_deep(&nested_scalar(depth)), "scalar {depth}");
            assert!(is_too_deep(&nested_paren(depth)), "paren {depth}");
            assert!(is_too_deep(&nested_and(depth)), "AND {depth}");
        }
    });
}

#[test]
fn row_codegen_survives_the_deepest_subquery_nesting_the_parser_accepts() {
    on_2mib_stack(|| {
        // Just under each shape's measured cap (31 derived tables, 21
        // scalar subqueries -- pinned in `grammar.rs`'s
        // `select_depth_tests`), and 10x deeper than any real query.
        for sql in [nested_from(31), nested_scalar(21)] {
            assert!(
                matches!(parse_select(&sql), ParseOutcome::Accepted(_)),
                "parser must accept this depth or the test proves nothing: {sql}"
            );
            let program = compile_statement(&sql, &[schema()], &[]).unwrap();
            assert!(!program.instructions.is_empty());
        }
    });
}

#[test]
fn row_codegen_survives_the_deepest_expression_nesting_the_parser_accepts() {
    on_2mib_stack(|| {
        // 50 parens / 50 conjunctions sit just under the expression cap
        // (60+ is rejected); the AND shape drives `pushdown`'s conjunct
        // walk, the paren shape `expr::compile_value`'s recursion.
        for sql in [nested_paren(50), nested_and(50)] {
            assert!(
                matches!(parse_select(&sql), ParseOutcome::Accepted(_)),
                "{sql}"
            );
            let program = compile_statement(&sql, &[schema()], &[]).unwrap();
            assert!(!program.instructions.is_empty());
        }
    });
}

#[test]
fn batch_codegen_survives_the_deepest_expression_nesting_the_parser_accepts() {
    on_2mib_stack(|| {
        // `parser::column` is a thin adapter over the row grammar (#57),
        // so it shares the same depth budget; `codegen::batch::compile`
        // recurses over the accepted AST with its own frame sizes.
        for sql in [nested_paren(50), nested_and(50)] {
            let select = column::parse(&sql).unwrap();
            let program = batch::compile(&select).unwrap();
            assert!(program.opcodes().count() > 0, "{sql}");
        }
    });
}

/// The deepest nesting of `wrap` around `json('[1]')` that the parser
/// accepts, as a `SELECT ... FROM t`.
fn deepest_accepted(wrap: impl Fn(&str) -> String) -> String {
    let mut expr = "json('[1]')".to_string();
    let mut deepest = None;
    for _ in 0..200 {
        let next = wrap(&expr);
        let sql = format!("SELECT {next} FROM t");
        if !matches!(parse_select(&sql), ParseOutcome::Accepted(_)) {
            break;
        }
        deepest = Some(sql);
        expr = next;
    }
    deepest.unwrap()
}

#[test]
fn row_codegen_survives_the_deepest_json_subtype_nesting_the_parser_accepts() {
    // #651: the JSON-subtype classifier and the `iif`-as-`CASE` lowering
    // recurse alongside `compile_value`.
    on_2mib_stack(|| {
        for sql in [
            deepest_accepted(|e| format!("json_array(iif(a, {e}, 'x'))")),
            deepest_accepted(|e| format!("json_array(coalesce(a, {e}, 'x'))")),
            deepest_accepted(|e| {
                format!("json_object('k', CASE WHEN a THEN {e} ELSE json_extract(a, '$') END)")
            }),
            deepest_accepted(|e| format!("json_array(({e}))")),
        ] {
            let program = compile_statement(&sql, &[schema()], &[]).unwrap();
            assert!(!program.instructions.is_empty());
        }
    });
}

/// `SELECT a FROM t WHERE a [NOT] IN (<lit>, ...)` with `n` literals.
fn in_list(n: usize, negated: bool, lit: impl Fn(usize) -> String) -> String {
    let items: Vec<String> = (0..n).map(lit).collect();
    let not = if negated { "NOT " } else { "" };
    format!("SELECT a FROM t WHERE a {not}IN ({})", items.join(", "))
}

/// A long literal `IN` list is one flat `InList` (#901): the column parser
/// used to lower it to an `OR` chain as deep as the list is long, and the
/// passes over that tree overflowed a 2 MiB stack at about 1,000 items.
#[test]
fn batch_codegen_survives_a_long_literal_in_list_on_a_2mib_stack() {
    on_2mib_stack(|| {
        for n in [1_000, 10_000] {
            for negated in [false, true] {
                let lits: [&dyn Fn(usize) -> String; 3] =
                    [&|i| i.to_string(), &|i| format!("'v{i}'"), &|i| {
                        format!("x'{i:08x}'")
                    }];
                for lit in lits {
                    let sql = in_list(n, negated, lit);
                    let select = column::parse(&sql).unwrap();
                    batch::compile(&select).unwrap();
                }
            }
        }
    });
}

/// The row engine keeps `IN` as one flat node, so a long list costs no
/// stack there (#901): parse and compile 10,000 literals on a 2 MiB thread.
#[test]
fn row_codegen_survives_a_long_literal_in_list_on_a_2mib_stack() {
    on_2mib_stack(|| {
        for negated in [false, true] {
            let sql = in_list(10_000, negated, |i| i.to_string());
            assert!(
                matches!(parse_select(&sql), ParseOutcome::Accepted(_)),
                "the row parser must accept a long IN list"
            );
            let program = compile_statement(&sql, &[schema()], &[]).unwrap();
            assert!(!program.instructions.is_empty());
        }
    });
}
