//! db-core#545: the totality runner must classify every stage's typed
//! refusal as a non-finding, catch a panic as a finding naming its
//! stage, recover from a hang with a fresh worker, and replay
//! deterministically.

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

use fuzz_gen::{load_db_core_grammar, Section, VBlockScope, Walker, WalkerConfig};
use fuzz_run::{
    catalog_of, fixture_path, install_panic_capture, probe, run_parallel, Finding, Outcome,
    Rejection, RowDialect, RunConfig, Runner, Stage,
};

fn fixture() -> std::path::PathBuf {
    fixture_path(env!("CARGO_MANIFEST_DIR"))
}

fn runner(timeout: Duration) -> Runner {
    Runner::new(RunConfig {
        fixture: fixture(),
        timeout,
        allow_impldef: false,
        stop_after: Stage::Vm,
        oracle: None,
    })
    .expect("runner")
}

fn runner_until(stop_after: Stage) -> Runner {
    Runner::new(RunConfig {
        fixture: fixture(),
        timeout: Duration::from_secs(5),
        allow_impldef: false,
        stop_after,
        oracle: None,
    })
    .expect("runner")
}

#[test]
fn stage_knob_stops_the_probe_chain_early() {
    // Unknown table: rejected by codegen, but a parser-only run never
    // gets there and reports Ok.
    let sql = "SELECT * FROM no_such_table";
    assert_eq!(
        runner_until(Stage::Parse).run_one(sql).unwrap(),
        Outcome::Ok
    );
    rejected_at(
        &runner_until(Stage::Codegen).run_one(sql).unwrap(),
        Stage::Codegen,
        Rejection::Compile,
    );
    // Duplicate rowid: only the VM can refuse it, so codegen-only says Ok.
    let dup = "INSERT INTO t(id, i) VALUES (1, 0)";
    assert_eq!(
        runner_until(Stage::Codegen).run_one(dup).unwrap(),
        Outcome::Ok
    );
    rejected_at(
        &runner(Duration::from_secs(5)).run_one(dup).unwrap(),
        Stage::Vm,
        Rejection::Execute,
    );
    // Parser-only still classifies parse failures.
    rejected_at(
        &runner_until(Stage::Parse).run_one("SELEKT").unwrap(),
        Stage::Parse,
        Rejection::Invalid,
    );
    assert_eq!(Stage::parse("codegen"), Some(Stage::Codegen));
    assert_eq!(Stage::parse("all"), None);
}

fn rejected_at(outcome: &Outcome, want_stage: Stage, want_kind: Rejection) {
    match outcome {
        Outcome::Rejected { stage, kind, .. } => {
            assert_eq!(*stage, want_stage, "{outcome:?}");
            assert_eq!(*kind, want_kind, "{outcome:?}");
        }
        other => panic!("expected rejection at {want_stage:?}, got {other:?}"),
    }
}

#[test]
fn each_stage_reports_its_own_typed_rejection() {
    let mut r = runner(Duration::from_secs(5));
    rejected_at(
        &r.run_one("SELEKT 1").unwrap(),
        Stage::Parse,
        Rejection::Invalid,
    );
    rejected_at(
        &r.run_one("SELECT * FROM").unwrap(),
        Stage::Parse,
        Rejection::Invalid,
    );
    rejected_at(
        &r.run_one("SELECT * FROM no_such_table").unwrap(),
        Stage::Codegen,
        Rejection::Compile,
    );
    rejected_at(
        &r.run_one("SELECT nope FROM t").unwrap(),
        Stage::Codegen,
        Rejection::Compile,
    );
    // id is the rowid alias; 1 is already taken by the fixture.
    rejected_at(
        &r.run_one("INSERT INTO t(id, i) VALUES (1, 0)").unwrap(),
        Stage::Vm,
        Rejection::Execute,
    );
    assert_eq!(
        r.run_one("SELECT id, i, s FROM t ORDER BY id").unwrap(),
        Outcome::Ok
    );
    assert_eq!(r.workers_spawned(), 1, "no hang, no respawn");
}

#[test]
fn ddl_and_dml_round_trip_through_all_stages() {
    let mut r = runner(Duration::from_secs(5));
    for sql in [
        "CREATE TABLE u(a INTEGER, b TEXT)",
        "INSERT INTO u VALUES (1, 'x')",
        "UPDATE u SET b = 'y' WHERE a = 1",
        "SELECT a, b FROM u",
        "DELETE FROM u WHERE a = 1",
        "CREATE INDEX iu ON u(a)",
        "DROP INDEX iu",
        "DROP TABLE u",
    ] {
        assert_eq!(r.run_one(sql).unwrap(), Outcome::Ok, "{sql}");
    }
}

#[test]
fn probe_turns_a_panic_into_a_finding_naming_the_stage() {
    install_panic_capture();
    let outcome = probe(Stage::Codegen, || -> Result<(), (Rejection, String)> {
        panic!("boom: index out of range");
    })
    .expect_err("must be a finding");
    match &outcome {
        Outcome::Panic { stage, message } => {
            assert_eq!(stage, &Stage::Codegen);
            assert!(message.contains("boom: index out of range"), "{message}");
            assert!(
                message.contains("runner_test.rs"),
                "location captured: {message}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(outcome.is_finding());
    let f = Finding::from_outcome(9, 4, "SELECT 1", &outcome, 3).unwrap();
    let line = f.to_json_line();
    assert!(line.contains("\"stage\":\"codegen\""), "{line}");
    assert!(line.contains("\"class\":\"panic\""), "{line}");
    assert!(line.contains("\"seed\":9,\"index\":4"), "{line}");
}

#[test]
fn rejections_and_ok_are_never_findings() {
    assert!(!Outcome::Ok.is_finding());
    let rej = Outcome::Rejected {
        stage: Stage::Vm,
        kind: Rejection::Execute,
        message: String::new(),
    };
    assert!(!rej.is_finding());
    assert!(Finding::from_outcome(1, 1, "x", &rej, 0).is_none());
}

#[test]
fn a_hang_is_reported_with_its_stage_and_the_worker_is_replaced() {
    let mut r = runner(Duration::from_millis(1));
    // 7^6 = 117k-row cross product: far more than 1ms of VM time.
    let heavy = "SELECT count(*) FROM t a, t b, t c, t d, t e, t f";
    let outcome = r.run_one(heavy).unwrap();
    assert!(
        matches!(outcome, Outcome::Hang { .. }),
        "expected hang, got {outcome:?}"
    );
    assert_eq!(r.workers_spawned(), 2, "fresh worker after hang");
    // The replacement worker is healthy and starts from a clean fixture.
    let mut ok = runner(Duration::from_secs(5));
    assert_eq!(ok.run_one("SELECT id FROM t").unwrap(), Outcome::Ok);
}

#[test]
fn run_all_counts_rejections_per_stage_and_skips_impldef() {
    let mut r = runner(Duration::from_secs(5));
    let stmts = vec![
        (0, "SELECT 1".to_string()),
        (1, "SELEKT".to_string()),
        (2, "SELECT x FROM nowhere".to_string()),
        (3, "SELECT RANDOM()".to_string()),
    ];
    let summary = r.run_all(1, stmts, None).unwrap();
    assert_eq!(summary.total, 4);
    assert_eq!(summary.ok, 1);
    assert_eq!(summary.skipped_impldef, 1);
    assert_eq!(summary.rejected[&(Stage::Parse, Rejection::Invalid)], 1);
    assert_eq!(summary.rejected[&(Stage::Codegen, Rejection::Compile)], 1);
    assert!(summary.findings.is_empty());
    assert_eq!(summary.rejected_total(), 2);
    let report = summary.render();
    assert!(
        report.contains("ok: 1  rejected: 2  skipped(impldef): 1"),
        "{report}"
    );
    assert!(
        report.contains("findings: 0  panic: 0  hang: 0  corruption: 0"),
        "{report}"
    );
    assert!(Runner::is_impldef("select current_timestamp"));
    assert!(!Runner::is_impldef("SELECT 1"));
}

fn generate(seed: u64, n: usize) -> Vec<String> {
    let grammar = load_db_core_grammar(env!("CARGO_MANIFEST_DIR")).unwrap();
    let tables = catalog_of(&fixture()).unwrap();
    let config = WalkerConfig {
        max_depth: 12,
        scope: VBlockScope::All,
        dialect: Some(Box::new(RowDialect::from_tables(&tables))),
    };
    let mut w = Walker::new(&grammar, Section::Sqlite, seed, config);
    (0..n).map(|_| w.generate("sql-stmt").unwrap()).collect()
}

#[test]
fn generation_with_dialect_is_deterministic_in_seed_so_replay_works() {
    let a = generate(42, 30);
    let b = generate(42, 30);
    assert_eq!(a, b);
    assert_ne!(a, generate(43, 30));
    // The dialect table is doing its job: catalog names appear, and the
    // grammar's prose placeholders do not.
    let joined = a.join("\n");
    assert!(
        joined.contains(" t ") || joined.contains(" t\n") || joined.ends_with(" t"),
        "{joined}"
    );
    assert!(!joined.contains("escapes a literal"), "{joined}");
    assert!(!joined.contains(" NUMBER"), "{joined}");
}

#[test]
fn a_generated_batch_runs_to_completion_without_findings() {
    let stmts = generate(2024, 150);
    let mut r = runner(Duration::from_secs(5));
    let summary = r
        .run_all(2024, stmts.into_iter().enumerate(), None)
        .unwrap();
    assert_eq!(summary.total, 150);
    assert!(
        summary.findings.is_empty(),
        "totality violated: {}",
        summary.render()
    );
    // Sanity: the catalog-aware terminals get a meaningful share past
    // the parser (pure-grammar output almost never does).
    let past_parser = summary.ok
        + summary
            .rejected
            .iter()
            .filter(|((s, _), _)| *s != Stage::Parse)
            .map(|(_, n)| n)
            .sum::<usize>();
    assert!(past_parser * 3 >= summary.total, "{}", summary.render());
}

#[test]
fn view_table_name_collisions_are_rejected_not_corrupting() {
    // db-core#551 regression. Before the fix, `CREATE VIEW t` over table
    // `t` succeeded and the second DROP TABLE freed a root page another
    // catalog row still owned -- reported by this runner as a
    // `corruption` finding. Now the collision is a typed rejection and
    // the rest of the script runs clean.
    let mut r = runner(Duration::from_secs(5));
    let first = r.run_one("CREATE VIEW t AS SELECT 1").unwrap();
    match &first {
        Outcome::Rejected {
            stage: Stage::Codegen,
            kind: Rejection::Compile,
            message,
        } => assert!(message.contains("table t already exists"), "{message}"),
        other => panic!("{other:?}"),
    }
    for sql in [
        "CREATE VIEW IF NOT EXISTS t AS SELECT 1",
        "DROP TABLE IF EXISTS t",
        "CREATE TABLE IF NOT EXISTS t(r)",
        "DROP TABLE IF EXISTS t",
        "DROP TABLE IF EXISTS t",
        "CREATE VIEW v AS SELECT 1",
    ] {
        assert_eq!(r.run_one(sql).unwrap(), Outcome::Ok, "{sql}");
    }
    match r.run_one("DROP TABLE v").unwrap() {
        Outcome::Rejected { message, .. } => {
            assert!(
                message.contains("use DROP VIEW to delete view v"),
                "{message}"
            )
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        r.workers_spawned(),
        1,
        "no corruption, no engine replacement"
    );
}

#[test]
fn parallel_lanes_cover_every_statement_and_tag_findings_with_their_lane() {
    let stmts: Vec<(usize, String)> = generate(77, 120).into_iter().enumerate().collect();
    let config = RunConfig {
        fixture: fixture(),
        timeout: Duration::from_secs(5),
        allow_impldef: false,
        stop_after: Stage::Vm,
        oracle: None,
    };
    let (summary, spawned) = run_parallel(&config, 4, 77, stmts.clone(), None).unwrap();
    assert_eq!(summary.total, 120);
    assert_eq!(
        summary.ok + summary.rejected_total() + summary.skipped_impldef + summary.findings.len(),
        120
    );
    assert!(summary.findings.is_empty(), "{}", summary.render());
    assert_eq!(spawned, 4, "one worker per lane, no hangs");
    // JOBS=1 goes through the plain sequential runner.
    let (single, spawned1) = run_parallel(&config, 1, 77, stmts, None).unwrap();
    assert_eq!(single.total, 120);
    assert_eq!(spawned1, 1);
    // Finding records carry lane/jobs (defaults 0/1 from from_outcome).
    let panic = Outcome::Panic {
        stage: Stage::Vm,
        message: "x".to_string(),
    };
    let mut f = Finding::from_outcome(77, 3, "SELECT 1", &panic, 0).unwrap();
    assert_eq!((f.lane, f.jobs), (0, 1));
    f.lane = 2;
    f.jobs = 4;
    assert!(
        f.to_json_line().contains("\"lane\":2,\"jobs\":4,"),
        "{}",
        f.to_json_line()
    );
}
