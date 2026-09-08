// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! See `super`. Vectors for `select/aggregate.rs`, `select/aggregate/
//! join.rs`, `select/joins.rs`, `select/join_full.rs` and
//! `select/entry.rs`. Everything is driven through the public
//! `dispatch::compile_statement` entry point (the `select::*` submodules
//! are private to `select.rs`), so a vector the dispatch itself rules
//! out before the decision is reached is noted as such in its test.

use crate::codegen::row::dispatch::{compile_statement, DispatchError};
use crate::codegen::row::{compile_select_with_catalog, IndexSchema, IndexedColumn, TableSchema};
use crate::parser::ast::Select;
use crate::parser::row::{parse_select, ParseOutcome};
use crate::value::Collation;
use crate::vm::row::{Opcode, Program};

fn table(name: &str, root_page: u32, cols: &[&str]) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        root_page,
        columns: cols.iter().map(|c| (*c).to_string()).collect(),
        column_types: cols.iter().map(|_| "INTEGER".to_string()).collect(),
        column_collations: cols.iter().map(|_| Collation::Binary).collect(),
        sql: format!("CREATE TABLE {name} ({})", cols.join(", ")),
        ..Default::default()
    }
}

fn with_index(mut schema: TableSchema, index: &str, root_page: u32, col: &str) -> TableSchema {
    schema.indexes.push(IndexSchema {
        name: index.to_string(),
        unique: false,
        columns: vec![IndexedColumn {
            name: col.to_string(),
            desc: false,
            collation: Collation::Binary,
        }],
        root_page,
    });
    schema
}

/// `t(a, b)` at root 2 with index `ia(a)` at root 5.
fn t_indexed_a() -> TableSchema {
    with_index(table("t", 2, &["a", "b"]), "ia", 5, "a")
}

/// `a(k, v)` at root 2 and `b(k, w)` at root 3.
fn two_tables() -> Vec<TableSchema> {
    vec![table("a", 2, &["k", "v"]), table("b", 3, &["k", "w"])]
}

fn compile(sql: &str, schemas: &[TableSchema]) -> Result<Program, DispatchError> {
    compile_statement(sql, schemas, &[])
}

fn ok(sql: &str, schemas: &[TableSchema]) -> Program {
    match compile(sql, schemas) {
        Ok(p) => p,
        Err(e) => panic!("{sql}: expected Ok, got {e:?}"),
    }
}

fn err_text(sql: &str, schemas: &[TableSchema]) -> String {
    match compile(sql, schemas) {
        Ok(p) => panic!("{sql}: expected Err, got program {p:?}"),
        Err(e) => format!("{e:?}"),
    }
}

fn has(program: &Program, opcode: Opcode) -> bool {
    program.instructions.iter().any(|i| i.opcode == opcode)
}

/// Whether `program` opens a read cursor on `root_page`.
fn opens(program: &Program, root_page: i32) -> bool {
    program
        .instructions
        .iter()
        .any(|i| i.opcode == Opcode::OpenRead && i.p2 == root_page)
}

/// The index-only aggregate fast path: the table cursor is opened but
/// never walked -- every row visit is an `IdxRewind`/`IdxNext` over the
/// index and every `Column` read comes off that index cursor.
fn index_only(program: &Program) -> bool {
    has(program, Opcode::IdxRewind)
        && !has(program, Opcode::Rewind)
        && program
            .instructions
            .iter()
            .filter(|i| i.opcode == Opcode::Column)
            .all(|i| i.p1 == 1)
}

fn parsed(sql: &str) -> Select {
    match parse_select(sql) {
        ParseOutcome::Accepted(s) => *s,
        other => panic!("{sql}: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// aggregate_51 -- `try_compile_index_only_count`'s clause guard:
// `having.is_some() || limit.is_some() || !order_by.is_empty()`.
// Observable: the fast path emits `Opcode::Count`; the fallback scans.
// ---------------------------------------------------------------------

#[test]
fn mcdc__aggregate_51__v1_bare_count_star_takes_the_count_fast_path() {
    let p = ok("SELECT count(*) FROM t", &[t_indexed_a()]);
    assert!(has(&p, Opcode::Count));
}

#[test]
fn mcdc__aggregate_51__v2_having_falls_back_to_a_scan() {
    let p = ok(
        "SELECT count(*) FROM t HAVING count(*) > 0",
        &[t_indexed_a()],
    );
    assert!(!has(&p, Opcode::Count));
}

#[test]
fn mcdc__aggregate_51__v3_limit_falls_back_to_a_scan() {
    let p = ok("SELECT count(*) FROM t LIMIT 1", &[t_indexed_a()]);
    assert!(!has(&p, Opcode::Count));
}

/// The `ORDER BY` leaf: `compile_select_scan` rejects "ORDER BY combined
/// with an aggregate (no GROUP BY)" before this decision is reached, so
/// the fast path is never taken -- observed as the rejection itself.
#[test]
fn mcdc__aggregate_51__v4_order_by_never_reaches_the_count_fast_path() {
    let e = err_text("SELECT count(*) FROM t ORDER BY 1", &[t_indexed_a()]);
    assert!(e.contains("ORDER BY combined with an aggregate"), "{e}");
}

// ---------------------------------------------------------------------
// aggregate_66 -- the aggregate-shape guard of the same fast path:
// `*distinct || !name == count || !args == Star`.
// ---------------------------------------------------------------------

#[test]
fn mcdc__aggregate_66__v1_count_star_matches_the_shape() {
    let p = ok("SELECT count(*) FROM t", &[t_indexed_a()]);
    assert!(has(&p, Opcode::Count));
}

#[test]
fn mcdc__aggregate_66__v2_count_distinct_is_not_index_only() {
    let p = ok("SELECT count(DISTINCT a) FROM t", &[t_indexed_a()]);
    assert!(!has(&p, Opcode::Count));
}

#[test]
fn mcdc__aggregate_66__v3_other_function_name_is_not_a_count() {
    let p = ok("SELECT max(a) FROM t", &[t_indexed_a()]);
    assert!(!has(&p, Opcode::Count));
}

#[test]
fn mcdc__aggregate_66__v4_count_of_a_column_is_not_count_star() {
    let p = ok("SELECT count(a) FROM t", &[t_indexed_a()]);
    assert!(!has(&p, Opcode::Count));
}

// ---------------------------------------------------------------------
// aggregate_198 -- `try_compile_index_only_sum`'s clause guard (WHERE /
// HAVING / LIMIT / ORDER BY / GROUP BY). Observable: the fast path reads
// only the index (root 5), never opening the table (root 2).
// ---------------------------------------------------------------------

#[test]
fn mcdc__aggregate_198__v1_bare_sum_reads_only_the_index() {
    let p = ok("SELECT sum(a) FROM t", &[t_indexed_a()]);
    assert!(index_only(&p), "{p:?}");
}

#[test]
fn mcdc__aggregate_198__v2_where_opens_the_table() {
    let p = ok("SELECT sum(a) FROM t WHERE b > 1", &[t_indexed_a()]);
    assert!(opens(&p, 2), "{p:?}");
}

#[test]
fn mcdc__aggregate_198__v3_having_opens_the_table() {
    let p = ok("SELECT sum(a) FROM t HAVING sum(a) > 1", &[t_indexed_a()]);
    assert!(opens(&p, 2), "{p:?}");
}

#[test]
fn mcdc__aggregate_198__v4_limit_opens_the_table() {
    let p = ok("SELECT sum(a) FROM t LIMIT 1", &[t_indexed_a()]);
    assert!(opens(&p, 2), "{p:?}");
}

/// `ORDER BY` with an ungrouped aggregate is rejected upstream by
/// `compile_select_scan`; the fast path is never consulted.
#[test]
fn mcdc__aggregate_198__v5_order_by_never_reaches_the_sum_fast_path() {
    let e = err_text("SELECT sum(a) FROM t ORDER BY 1", &[t_indexed_a()]);
    assert!(e.contains("ORDER BY combined with an aggregate"), "{e}");
}

/// A `GROUP BY` routes to the grouped-scan branch of
/// `compile_select_scan` before the ungrouped fast paths; the table is
/// scanned (or index-walked in key order) and the aggregate accumulated
/// per group rather than summed off the index alone.
#[test]
fn mcdc__aggregate_198__v6_group_by_never_reaches_the_sum_fast_path() {
    let p = ok("SELECT b, sum(a) FROM t GROUP BY b", &[t_indexed_a()]);
    assert!(opens(&p, 2), "{p:?}");
}

// ---------------------------------------------------------------------
// aggregate_218 -- the same fast path's function guard:
// `*distinct || !(name == sum || name == avg)`.
// ---------------------------------------------------------------------

#[test]
fn mcdc__aggregate_218__v1_plain_sum_is_index_only() {
    let p = ok("SELECT sum(a) FROM t", &[t_indexed_a()]);
    assert!(index_only(&p), "{p:?}");
}

#[test]
fn mcdc__aggregate_218__v2_sum_distinct_opens_the_table() {
    let p = ok("SELECT sum(DISTINCT a) FROM t", &[t_indexed_a()]);
    assert!(opens(&p, 2), "{p:?}");
}

#[test]
fn mcdc__aggregate_218__v3_count_is_neither_sum_nor_avg() {
    let p = ok("SELECT count(a) FROM t", &[t_indexed_a()]);
    assert!(!index_only(&p), "{p:?}");
}

// ---------------------------------------------------------------------
// aggregate_1117 -- `group_by_index_ordering`'s
// `implicit_group || select.group_by.is_empty()`. Only reached from
// `compile_select_scan`'s explicit-GROUP-BY branch (with
// `implicit_group == false`) and from EQP (same), so `(false, false)` is
// the one vector the code can produce; the other two leaves describe
// the routing that keeps them unreachable. Observable: an index-ordered
// GROUP BY needs no `SorterOpen`.
// ---------------------------------------------------------------------

#[test]
fn mcdc__aggregate_1117__v1_explicit_group_by_on_indexed_column_walks_the_index() {
    let p = ok("SELECT a, count(*) FROM t GROUP BY a", &[t_indexed_a()]);
    assert!(!has(&p, Opcode::SorterOpen) && opens(&p, 5), "{p:?}");
}

/// `implicit_group == true` (an aggregate with no GROUP BY) never asks
/// for index ordering: there is one group, nothing to order.
#[test]
fn mcdc__aggregate_1117__v2_implicit_group_never_asks_for_index_ordering() {
    let p = ok("SELECT count(*) FROM t", &[t_indexed_a()]);
    assert!(!has(&p, Opcode::SorterOpen) && !opens(&p, 5), "{p:?}");
}

/// `group_by.is_empty()` with no aggregate is a plain scan; the grouped
/// branch (and with it this decision) is skipped entirely.
#[test]
fn mcdc__aggregate_1117__v3_no_group_by_and_no_aggregate_is_a_plain_scan() {
    let p = ok("SELECT a FROM t", &[t_indexed_a()]);
    assert!(!has(&p, Opcode::SorterOpen), "{p:?}");
}

// ---------------------------------------------------------------------
// aggregate_1123 -- `select.where_clause.is_some() || schema.without_rowid`
// (same function): either disqualifies the index-ordered GROUP BY.
// ---------------------------------------------------------------------

#[test]
fn mcdc__aggregate_1123__v1_no_where_on_a_rowid_table_is_index_ordered() {
    let p = ok("SELECT a, count(*) FROM t GROUP BY a", &[t_indexed_a()]);
    assert!(!has(&p, Opcode::SorterOpen), "{p:?}");
}

#[test]
fn mcdc__aggregate_1123__v2_where_clause_needs_a_sorter() {
    let p = ok(
        "SELECT a, count(*) FROM t WHERE b > 0 GROUP BY a",
        &[t_indexed_a()],
    );
    assert!(has(&p, Opcode::SorterOpen), "{p:?}");
}

#[test]
fn mcdc__aggregate_1123__v3_without_rowid_table_needs_a_sorter() {
    let mut schema = t_indexed_a();
    schema.without_rowid = true;
    let p = ok("SELECT a, count(*) FROM t GROUP BY a", &[schema]);
    assert!(has(&p, Opcode::SorterOpen), "{p:?}");
}

// ---------------------------------------------------------------------
// join_796 -- `matches_agg_slot`: an ORDER BY aggregate matches a
// collected slot only when `name` and `DISTINCT`-ness both agree
// (`!name_eq || distinct != slot_distinct`). A match sorts on the
// finalized aggregate; no match falls through to bare-column
// resolution, which rejects a function call.
// ---------------------------------------------------------------------

#[test]
fn mcdc__join_796__v1_same_name_and_distinctness_matches_the_slot() {
    let p = ok(
        "SELECT a.k, count(b.w) FROM a JOIN b ON a.k = b.k GROUP BY a.k ORDER BY count(b.w)",
        &two_tables(),
    );
    assert!(has(&p, Opcode::SorterOpen), "{p:?}");
}

#[test]
fn mcdc__join_796__v2_different_name_does_not_match() {
    let e = err_text(
        "SELECT a.k, count(b.w) FROM a JOIN b ON a.k = b.k GROUP BY a.k ORDER BY sum(b.w)",
        &two_tables(),
    );
    assert!(
        e.contains("Unsupported") || e.contains("UnknownColumn"),
        "{e}"
    );
}

#[test]
fn mcdc__join_796__v3_same_name_but_different_distinctness_does_not_match() {
    let e = err_text(
        "SELECT a.k, count(b.w) FROM a JOIN b ON a.k = b.k GROUP BY a.k \
         ORDER BY count(DISTINCT b.w)",
        &two_tables(),
    );
    assert!(
        e.contains("Unsupported") || e.contains("UnknownColumn"),
        "{e}"
    );
}

// ---------------------------------------------------------------------
// joins_81 -- `compile_select_joined`'s FULL JOIN dispatch:
// `joins.len() == 1 && first.op == Full`.
// ---------------------------------------------------------------------

#[test]
fn mcdc__joins_81__v1_single_full_join_takes_the_dedicated_emitter() {
    let p = ok(
        "SELECT a.k, b.k FROM a FULL JOIN b ON a.k = b.k",
        &two_tables(),
    );
    // The two-pass FULL JOIN emitter tracks matched `b` rowids in an
    // ephemeral index.
    assert!(has(&p, Opcode::OpenEphemeral), "{p:?}");
}

#[test]
fn mcdc__joins_81__v2_full_join_among_two_joins_is_rejected() {
    let mut schemas = two_tables();
    schemas.push(table("c", 4, &["k", "x"]));
    let e = err_text(
        "SELECT a.k FROM a FULL JOIN b ON a.k = b.k JOIN c ON c.k = a.k",
        &schemas,
    );
    assert!(e.contains("single two-table FULL JOIN"), "{e}");
}

#[test]
fn mcdc__joins_81__v3_single_inner_join_takes_the_ordinary_join_tree() {
    let p = ok("SELECT a.k, b.k FROM a JOIN b ON a.k = b.k", &two_tables());
    assert!(!has(&p, Opcode::OpenEphemeral), "{p:?}");
}

// ---------------------------------------------------------------------
// joins_443 -- `!group_by.is_empty() || select_has_aggregate(select)`
// routes a join to the grouped emitter, which rejects DISTINCT.
// ---------------------------------------------------------------------

#[test]
fn mcdc__joins_443__v1_group_by_routes_to_the_grouped_join() {
    let e = err_text(
        "SELECT DISTINCT a.k FROM a JOIN b ON a.k = b.k GROUP BY a.k",
        &two_tables(),
    );
    assert!(
        e.contains("GROUP BY/aggregate combined with DISTINCT and a JOIN"),
        "{e}"
    );
}

#[test]
fn mcdc__joins_443__v2_aggregate_without_group_by_routes_to_the_grouped_join() {
    let e = err_text(
        "SELECT DISTINCT count(*) FROM a JOIN b ON a.k = b.k",
        &two_tables(),
    );
    assert!(
        e.contains("GROUP BY/aggregate combined with DISTINCT and a JOIN"),
        "{e}"
    );
}

#[test]
fn mcdc__joins_443__v3_neither_is_a_plain_joined_scan() {
    let p = ok(
        "SELECT DISTINCT a.k FROM a JOIN b ON a.k = b.k",
        &two_tables(),
    );
    assert!(!has(&p, Opcode::SorterOpen), "{p:?}");
}

// ---------------------------------------------------------------------
// entry_142 -- `compile_select_no_from`'s clause guard: any of WHERE /
// GROUP BY / HAVING / ORDER BY / LIMIT / DISTINCT / compound rejects a
// FROM-less SELECT. Each clause is grafted onto `SELECT 1` from a parsed
// donor so the parser's own FROM-less grammar isn't what's under test.
// ---------------------------------------------------------------------

const NO_FROM_REJECTION: &str = "a FROM-less SELECT only supports a bare expression list";

fn from_less(sql_with_from: &str) -> Select {
    let mut select = parsed(sql_with_from);
    select.from = None;
    select
}

fn compile_no_from(select: &Select) -> Result<Program, String> {
    compile_select_with_catalog(select, &TableSchema::default(), &[]).map_err(|e| format!("{e:?}"))
}

#[test]
fn mcdc__entry_142__v1_bare_expression_list_compiles_to_one_row() {
    let p = compile_no_from(&parsed("SELECT 1 + 1")).unwrap();
    assert!(
        has(&p, Opcode::ResultRow) && !has(&p, Opcode::OpenRead),
        "{p:?}"
    );
}

#[test]
fn mcdc__entry_142__v2_where_is_rejected() {
    let e = compile_no_from(&from_less("SELECT 1 FROM t WHERE 1 = 1")).unwrap_err();
    assert!(e.contains(NO_FROM_REJECTION), "{e}");
}

#[test]
fn mcdc__entry_142__v3_group_by_is_rejected() {
    let e = compile_no_from(&from_less("SELECT 1 FROM t GROUP BY 1")).unwrap_err();
    assert!(e.contains(NO_FROM_REJECTION), "{e}");
}

#[test]
fn mcdc__entry_142__v4_having_is_rejected() {
    let mut select = parsed("SELECT 1");
    select.having = parsed("SELECT 1 FROM t GROUP BY 1 HAVING 1 = 1").having;
    let e = compile_no_from(&select).unwrap_err();
    assert!(e.contains(NO_FROM_REJECTION), "{e}");
}

#[test]
fn mcdc__entry_142__v5_order_by_is_rejected() {
    let e = compile_no_from(&from_less("SELECT 1 FROM t ORDER BY 1")).unwrap_err();
    assert!(e.contains(NO_FROM_REJECTION), "{e}");
}

#[test]
fn mcdc__entry_142__v6_limit_is_rejected() {
    let e = compile_no_from(&from_less("SELECT 1 FROM t LIMIT 1")).unwrap_err();
    assert!(e.contains(NO_FROM_REJECTION), "{e}");
}

#[test]
fn mcdc__entry_142__v7_distinct_is_rejected() {
    let e = compile_no_from(&parsed("SELECT DISTINCT 1")).unwrap_err();
    assert!(e.contains(NO_FROM_REJECTION), "{e}");
}

#[test]
fn mcdc__entry_142__v8_compound_is_rejected() {
    let mut select = parsed("SELECT 1");
    select.compound = from_less("SELECT 1 FROM t UNION ALL SELECT 2 FROM t").compound;
    let e = compile_no_from(&select).unwrap_err();
    assert!(e.contains(NO_FROM_REJECTION), "{e}");
}
