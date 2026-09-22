//! db-core#546: rendering, verdicts, ddmin, and the sqlite3 oracle end to
//! end (skipped when no sqlite3 binary is on PATH).

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
    reason = "test code fails fast (db-core#230); the runner under test is what must not panic"
)]

use std::time::Duration;

use db_core::engine::{Cell, QueryResult};
use fuzz_run::oracle::split_quote_row;
use fuzz_run::{
    compare, ddmin, fixture_path, render_cell, row_order, OracleConfig, OracleOutcome, Outcome,
    Rejection, RowOrder, RunConfig, Runner, Stage, Verdict,
};

fn result(rows: Vec<Vec<Cell>>) -> QueryResult {
    QueryResult {
        columns: vec![],
        rows,
        ..Default::default()
    }
}

#[test]
fn cells_render_like_sqlite3_quote_mode() {
    assert_eq!(render_cell(&Cell::Null), "NULL");
    assert_eq!(render_cell(&Cell::Int(-7)), "-7");
    assert_eq!(render_cell(&Cell::Real(1.0)), "1.0");
    assert_eq!(render_cell(&Cell::Real(0.5)), "0.5");
    assert_eq!(render_cell(&Cell::Text("it's".to_string())), "'it''s'");
    assert_eq!(render_cell(&Cell::Blob(vec![0x00, 0xff])), "X'00ff'");
}

#[test]
fn quote_rows_split_on_commas_outside_quotes_and_lowercase_blobs() {
    assert_eq!(
        split_quote_row("1,'a,b','it''s',NULL,X'00FF',2.5"),
        vec!["1", "'a,b'", "'it''s'", "NULL", "X'00ff'", "2.5"]
    );
    assert_eq!(split_quote_row("''"), vec!["''"]);
}

#[test]
fn row_order_follows_order_by_and_limit() {
    assert_eq!(row_order("SELECT id FROM t ORDER BY id"), RowOrder::Ordered);
    assert_eq!(row_order("SELECT id FROM t"), RowOrder::Unordered);
    assert_eq!(
        row_order("SELECT id FROM t LIMIT 2"),
        RowOrder::ArbitrarySubset
    );
    assert_eq!(
        row_order("INSERT INTO t(i) VALUES (1)"),
        RowOrder::Unordered
    );
}

#[test]
fn verdicts_cover_every_quadrant() {
    let ok = Outcome::Ok;
    let rejected = |kind| Outcome::Rejected {
        stage: Stage::Codegen,
        kind,
        message: String::new(),
    };
    let rows = |v: &[&[&str]]| {
        OracleOutcome::Rows(
            v.iter()
                .map(|r| r.iter().map(|c| c.to_string()).collect())
                .collect(),
        )
    };
    // pass, sorted because no ORDER BY
    let ours = result(vec![vec![Cell::Int(2)], vec![Cell::Int(1)]]);
    let cmp = compare(
        "SELECT i FROM t",
        &ok,
        Some(&ours),
        Some(&rows(&[&["1"], &["2"]])),
    );
    assert_eq!(cmp.verdict, Verdict::Pass);
    assert_eq!(cmp.mode, "sorted");
    // ordered: same rows, different order -> wrong answer
    let cmp = compare(
        "SELECT i FROM t ORDER BY i",
        &ok,
        Some(&ours),
        Some(&rows(&[&["1"], &["2"]])),
    );
    assert_eq!(cmp.verdict, Verdict::WrongAnswer);
    // count-only under LIMIT without ORDER BY
    let cmp = compare(
        "SELECT i FROM t LIMIT 2",
        &ok,
        Some(&ours),
        Some(&rows(&[&["9"], &["8"]])),
    );
    assert_eq!(cmp.verdict, Verdict::Pass);
    assert_eq!(cmp.mode, "count-only");
    // oracle prints round-trip precision; same double -> pass
    let ours = result(vec![vec![Cell::Real(2.75)]]);
    let cmp = compare(
        "SELECT r FROM t",
        &ok,
        Some(&ours),
        Some(&rows(&[&["2.750000000000000124"]])),
    );
    assert_eq!(cmp.verdict, Verdict::Pass, "{cmp:?}");
    // a different double within 1e-12 relative -> float drift, not wrong
    let ours = result(vec![vec![Cell::Real(1.0)]]);
    let cmp = compare(
        "SELECT r FROM t",
        &ok,
        Some(&ours),
        Some(&rows(&[&["1.0000000000001"]])),
    );
    assert_eq!(cmp.verdict, Verdict::FloatDrift, "{cmp:?}");
    let cmp = compare(
        "SELECT r FROM t",
        &ok,
        Some(&ours),
        Some(&rows(&[&["1.001"]])),
    );
    assert_eq!(cmp.verdict, Verdict::WrongAnswer, "{cmp:?}");
    // gap vs unsupported vs both-reject
    let err = OracleOutcome::Error("Parse error: x".to_string());
    assert_eq!(
        compare(
            "SELECT 1",
            &rejected(Rejection::Compile),
            None,
            Some(&rows(&[&["1"]]))
        )
        .verdict,
        Verdict::Gap
    );
    assert_eq!(
        compare(
            "SELECT 1",
            &rejected(Rejection::Unsupported),
            None,
            Some(&rows(&[&["1"]]))
        )
        .verdict,
        Verdict::Unsupported
    );
    assert_eq!(
        compare("SELECT 1", &rejected(Rejection::Invalid), None, Some(&err)).verdict,
        Verdict::BothReject
    );
    // over-permissive
    let cmp = compare("SELECT 1", &ok, Some(&result(vec![])), Some(&err));
    assert_eq!(cmp.verdict, Verdict::OverPermissive);
    assert_eq!(cmp.oracle_message.as_deref(), Some("Parse error: x"));
    // totality: oracle not consulted
    let panic = Outcome::Panic {
        stage: Stage::Vm,
        message: String::new(),
    };
    assert_eq!(
        compare("SELECT 1", &panic, None, Some(&rows(&[]))).verdict,
        Verdict::Totality
    );
    assert!(Verdict::WrongAnswer.is_finding() && !Verdict::Unsupported.is_finding());
}

#[test]
fn ddmin_finds_a_one_minimal_subsequence() {
    let script: Vec<String> = (0..20).map(|i| format!("s{i}")).collect();
    // Fails iff both s3 and s11 are present.
    let (reduced, probes) = ddmin(
        &script,
        |c| c.iter().any(|s| s == "s3") && c.iter().any(|s| s == "s11"),
        1000,
    );
    assert_eq!(reduced, vec!["s3".to_string(), "s11".to_string()]);
    assert!(probes < 100, "{probes}");
    // Always-failing: reduces to one statement. Never-failing input stays.
    let (one, _) = ddmin(&script, |_| true, 1000);
    assert_eq!(one.len(), 1);
    let (same, _) = ddmin(&script, |_| false, 1000);
    assert_eq!(same.len(), 20);
}

fn sqlite3_available() -> bool {
    std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn oracle_runner_agrees_with_engine_on_the_fixture() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3 on PATH");
        return;
    }
    let mut r = Runner::new(RunConfig {
        fixture: fixture_path(env!("CARGO_MANIFEST_DIR")),
        timeout: Duration::from_secs(5),
        allow_impldef: false,
        stop_after: Stage::Vm,
        oracle: Some(OracleConfig {
            bin: "sqlite3".to_string(),
            allow_version_mismatch: true,
        }),
    })
    .expect("runner with oracle");
    assert!(r.oracle_version().is_some_and(|v| v.starts_with("3.")));
    for (sql, want) in [
        ("SELECT id, i, s, r, b FROM t ORDER BY id", Verdict::Pass),
        ("SELECT count(*) FROM t", Verdict::Pass),
        ("INSERT INTO t(i, s) VALUES (42, 'x''y')", Verdict::Pass),
        ("SELECT i, s FROM t WHERE i = 42", Verdict::Pass),
        (
            "SELECT typeof(b), b FROM t WHERE b IS NOT NULL",
            Verdict::Pass,
        ),
        ("SELEKT 1", Verdict::BothReject),
        // We reject DROP VIEW at codegen (#552); sqlite3 has no view v either.
        ("DROP VIEW v", Verdict::BothReject),
    ] {
        let (outcome, cmp) = r.run_one_compared(sql).unwrap();
        let cmp = cmp.unwrap_or_else(|| panic!("no comparison for {sql}: {outcome:?}"));
        assert_eq!(cmp.verdict, want, "{sql}: {outcome:?} {cmp:?}");
    }
    // A state-changing statement only the oracle accepts resyncs the
    // oracle from our applied history, so later reads still agree.
    let (_, cmp) = r
        .run_one_compared("CREATE VIEW v AS SELECT id FROM t")
        .unwrap();
    let (_, cmp2) = r.run_one_compared("SELECT count(*) FROM t").unwrap();
    assert_eq!(cmp2.unwrap().verdict, Verdict::Pass, "{cmp:?}");
}

#[test]
fn finding_shape_groups_by_leading_keywords_and_normalized_message() {
    use fuzz_run::runner::finding_shape;
    assert_eq!(
        finding_shape(
            "EXPLAIN QUERY PLAN ROLLBACK TRANSACTION",
            "no such table: t (line 1, column 22)"
        ),
        "EXPLAIN QUERY PLAN ROLLBACK TRANSACTION | no such table: t"
    );
    assert_eq!(
        finding_shape(
            "CREATE TABLE IF NOT EXISTS zq ( a , CHECK ( 1 ) )",
            "Parse error near line 7: table \"zq\" has more than one primary key"
        ),
        "CREATE TABLE IF NOT EXISTS | Parse error table 'S' has more than one primary key"
    );
    assert_eq!(
        finding_shape("PRAGMA journal_mode = WAL", ""),
        finding_shape("PRAGMA journal_mode = WAL", "")
    );
    assert_ne!(
        finding_shape("INSERT INTO t ( i ) DEFAULT VALUES", "x"),
        finding_shape("INSERT INTO t ( r ) DEFAULT VALUES", "y")
    );
}
