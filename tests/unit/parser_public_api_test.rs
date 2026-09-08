// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Black-box tests for the parser's public interface: `parser::column::
//! parse`/`parse_explain` (the entry points [`crate::codegen::row`]
//! actually calls) and `parser::row`'s per-statement free functions
//! (`parse_insert`, `parse_pragma`, ...), which `codegen::row::dispatch`
//! calls directly for everything that isn't a `SELECT`. Existing
//! coverage of these comes only incidentally, from whatever AST shapes
//! codegen's own tests happen to construct along the way -- `parser::
//! ast::Pragma::span`/`TableRef::name`'s subquery arm in particular are
//! unreached without a query built specifically to hit them. This suite
//! drives every entry point directly with representative accepted and
//! malformed SQL, independent of codegen.

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

use db_core::parser::ast::TableRefKind;
use db_core::parser::column::{parse, parse_explain, Explain};
use db_core::parser::row::{
    parse_analyze, parse_begin, parse_commit, parse_create_index, parse_create_table,
    parse_create_view, parse_delete, parse_drop_index, parse_drop_table, parse_drop_view,
    parse_insert, parse_pragma, parse_rollback, parse_update, ParseOutcome,
};

fn accepted<T>(outcome: ParseOutcome<T>) -> T {
    match outcome {
        ParseOutcome::Accepted(v) => *v,
        ParseOutcome::Unsupported { message, .. } => {
            panic!("expected Accepted, got Unsupported: {message}")
        }
        ParseOutcome::Invalid { message, .. } => {
            panic!("expected Accepted, got Invalid: {message}")
        }
    }
}

fn rejected<T>(outcome: ParseOutcome<T>) {
    assert!(
        matches!(
            outcome,
            ParseOutcome::Unsupported { .. } | ParseOutcome::Invalid { .. }
        ),
        "expected the parse to be rejected"
    );
}

// --- parser::column::parse / parse_explain ---------------------------------

#[test]
fn parse_accepts_a_plain_select() {
    let select = parse("SELECT id, name FROM users WHERE id > 1").unwrap();
    assert_eq!(select.columns.len(), 2);
}

#[test]
fn parse_accepts_select_variants() {
    assert!(parse("SELECT * FROM t").is_ok());
    assert!(parse("SELECT a FROM t JOIN u ON t.a = u.a").is_ok());
    assert!(parse("SELECT a, COUNT(*) FROM t GROUP BY a").is_ok());
}

#[test]
fn parse_resolves_a_from_subquery_alias_via_table_ref_name() {
    // `TableRef::name()` returns `None` for a `Subquery` kind -- the
    // resolved alias is what a caller uses instead; exercised here via
    // the outer `FROM`'s `TableRef`, since column-rs itself never calls
    // `TableRef::name()` on a subquery entry.
    let select = parse("SELECT x FROM (SELECT x FROM t) sub").unwrap();
    let from = select.from.as_ref().unwrap();
    assert!(matches!(from.first.kind, TableRefKind::Subquery(_)));
    assert_eq!(from.first.name(), None);
    assert_eq!(from.first.alias.as_deref(), Some("sub"));
}

#[test]
fn parse_rejects_malformed_sql() {
    assert!(parse("SELECT FROM").is_err());
    assert!(parse("SELEKT 1").is_err());
    assert!(parse("").is_err());
    assert!(parse("SELECT 1 FROM t WHERE").is_err());
}

#[test]
fn parse_rejects_a_bare_column_alongside_an_aggregate_without_group_by() {
    // column-rs's own post-parse validation (`validate_select`), not the
    // grammar -- distinguishes this from a syntax error.
    let err = parse("SELECT a, COUNT(*) FROM t").unwrap_err();
    assert!(format!("{err:?}").contains("GROUP BY"));
}

#[test]
fn parse_explain_distinguishes_all_three_forms() {
    let (form, select) = parse_explain("SELECT id FROM t").unwrap();
    assert_eq!(form, Explain::None);
    assert_eq!(select.from.as_ref().unwrap().first.name(), Some("t"));

    let (form, _) = parse_explain("EXPLAIN SELECT id FROM t").unwrap();
    assert_eq!(form, Explain::Opcodes);

    let (form, _) = parse_explain("EXPLAIN QUERY PLAN SELECT id FROM t").unwrap();
    assert_eq!(form, Explain::QueryPlan);
}

#[test]
fn parse_explain_rejects_malformed_sql_after_the_prefix() {
    assert!(parse_explain("EXPLAIN SELEKT 1").is_err());
    assert!(parse_explain("EXPLAIN QUERY PLAN").is_err());
}

// --- parser::row's per-statement entry points -------------------------------

#[test]
fn parse_insert_accepts_and_rejects() {
    let insert = accepted(parse_insert("INSERT INTO t (a, b) VALUES (1, 2)"));
    assert_eq!(insert.table, "t");
    rejected(parse_insert("INSERT INTO"));
}

#[test]
fn parse_update_accepts_and_rejects() {
    let update = accepted(parse_update("UPDATE t SET a = 1 WHERE b = 2"));
    assert_eq!(update.table, "t");
    rejected(parse_update("UPDATE SET"));
}

#[test]
fn parse_delete_accepts_and_rejects() {
    let delete = accepted(parse_delete("DELETE FROM t WHERE a = 1"));
    assert_eq!(delete.table, "t");
    rejected(parse_delete("DELETE t"));
}

#[test]
fn parse_create_table_accepts_and_rejects() {
    let create = accepted(parse_create_table("CREATE TABLE t (a INTEGER, b TEXT)"));
    assert_eq!(create.name, "t");
    rejected(parse_create_table("CREATE TABLE"));
}

#[test]
fn parse_create_index_accepts_and_rejects() {
    let create = accepted(parse_create_index("CREATE INDEX idx_t_a ON t (a)"));
    assert_eq!(create.name, "idx_t_a");
    rejected(parse_create_index("CREATE INDEX ON"));
}

#[test]
fn parse_create_view_accepts_and_rejects() {
    let create = accepted(parse_create_view("CREATE VIEW v AS SELECT a FROM t"));
    assert_eq!(create.name, "v");
    rejected(parse_create_view("CREATE VIEW"));
}

#[test]
fn parse_drop_statements_accept_and_reject() {
    assert_eq!(accepted(parse_drop_table("DROP TABLE t")).name, "t");
    assert_eq!(
        accepted(parse_drop_index("DROP INDEX idx_t_a")).name,
        "idx_t_a"
    );
    assert_eq!(accepted(parse_drop_view("DROP VIEW v")).name, "v");
    rejected(parse_drop_table("DROP TABLE"));
    rejected(parse_drop_index("DROP INDEX"));
    rejected(parse_drop_view("DROP VIEW"));
}

#[test]
fn parse_transaction_statements_accept_and_reject() {
    assert!(matches!(parse_begin("BEGIN"), ParseOutcome::Accepted(_)));
    assert!(matches!(
        parse_begin("BEGIN IMMEDIATE"),
        ParseOutcome::Accepted(_)
    ));
    assert!(matches!(parse_commit("COMMIT"), ParseOutcome::Accepted(_)));
    assert!(matches!(
        parse_rollback("ROLLBACK"),
        ParseOutcome::Accepted(_)
    ));
    rejected(parse_begin("BEGIN NONSENSE MODE"));
}

#[test]
fn parse_pragma_covers_every_variant_and_span_reads_back() {
    use db_core::parser::ast::Pragma;

    let wal = accepted(parse_pragma("PRAGMA journal_mode = WAL"));
    assert!(matches!(wal, Pragma::JournalMode { .. }));
    let _ = wal.span();

    let check = accepted(parse_pragma("PRAGMA integrity_check"));
    assert!(matches!(check, Pragma::IntegrityCheck { .. }));
    let _ = check.span();

    let sync = accepted(parse_pragma("PRAGMA synchronous = FULL"));
    assert!(matches!(sync, Pragma::Synchronous { .. }));
    let _ = sync.span();

    rejected(parse_pragma("PRAGMA"));
}

#[test]
fn parse_analyze_accepts_bare_and_scoped_forms_and_rejects_malformed() {
    let bare = accepted(parse_analyze("ANALYZE"));
    assert!(bare.target.is_none());
    let scoped = accepted(parse_analyze("ANALYZE t"));
    assert_eq!(scoped.target.as_deref(), Some("t"));
    rejected(parse_analyze("ANALYZE 1 2 3"));
}

// db-core#223: `parser::row`'s own `parse_select`/`parse_explain` (the
// entry points `parser::column::parse`/`parse_explain` delegate to
// internally) plus `split_statements`/`ends_with_semicolon`, `Span`, and
// `column::ParseError` -- none of these were called directly by name
// from `tests/unit` before this.
#[test]
fn row_parse_select_accepts_and_rejects() {
    use db_core::parser::row::parse_select as row_parse_select;

    assert!(matches!(
        row_parse_select("SELECT 1"),
        ParseOutcome::Accepted(_)
    ));
    assert!(matches!(
        row_parse_select("SELECT FROM t"),
        ParseOutcome::Invalid { .. }
    ));
}

#[test]
fn row_parse_explain_distinguishes_opcodes_from_query_plan() {
    use db_core::parser::ast::Explain as RowExplain;
    use db_core::parser::row::parse_explain as row_parse_explain;

    let opcodes = accepted(row_parse_explain("EXPLAIN SELECT 1"));
    assert!(!opcodes.query_plan);
    let plan = accepted(row_parse_explain("EXPLAIN QUERY PLAN SELECT 1"));
    assert!(plan.query_plan);
    let _: RowExplain = opcodes;
}

#[test]
fn split_statements_splits_on_semicolons_and_ends_with_semicolon_detects_trailer() {
    use db_core::parser::row::{ends_with_semicolon, split_statements};

    let stmts = split_statements("SELECT 1; SELECT 2 ;");
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].contains("SELECT 1"));
    assert!(stmts[1].contains("SELECT 2"));

    assert!(ends_with_semicolon("SELECT 1;"));
    assert!(!ends_with_semicolon("SELECT 1"));
}

#[test]
fn span_reads_back_line_and_column_and_parse_error_carries_one() {
    use db_core::parser::column::{parse, ParseError};
    use db_core::parser::Span;

    let err = parse("SELECT FROM t").unwrap_err();
    let span: Span = err.span();
    assert!(span.line >= 1);
    match err {
        ParseError::UnexpectedEof { .. } | ParseError::Unexpected { .. } => {}
    }
}
