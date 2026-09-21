//! CLI entry point for `make fuzz-sql TARGET=row`: generates `N`
//! statements from `grammar.ebnf` with catalog-aware terminals and runs
//! each through parse -> codegen -> vm under panic capture and a
//! per-statement timeout. Only panics and hangs are findings.
//!
//! Environment (same convention as `fuzz-gen`'s `gen`):
//!   N, SEED, MAX_DEPTH      generator knobs
//!   TIMEOUT_MS              per-statement budget (default 2000)
//!   OUT                     findings directory (default target/fuzz)
//!   REPLAY=<seed>:<index>   regenerate exactly that statement and run it
//!   STAGE=parse|codegen|vm  last probe to run (default vm: the whole chain)
//!   ALLOW_IMPLDEF=1         also run RANDOM()/CURRENT_* statements
//!   VERBOSE=1               print every statement and its outcome
//!
//! Exit codes: 0 clean, 3 findings recorded, 2 bad arguments, 1 setup
//! failure.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use fuzz_gen::{load_db_core_grammar, Section, VBlockScope, Walker, WalkerConfig};
use fuzz_run::{
    catalog_of, fixture_path, FindingsSink, Outcome, RowDialect, RunConfig, Runner, Stage,
};

fn env_var(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn fail(code: i32, msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(code)
}

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target = env_var("TARGET", "row");
    if target != "row" {
        fail(
            2,
            &format!("TARGET '{target}' has no runner yet (only row; #547 adds column/stream)"),
        );
    }
    let n: usize = env_var("N", "100").parse().unwrap_or(100);
    let mut seed: u64 = env_var("SEED", "1").parse().unwrap_or(1);
    let max_depth: usize = env_var("MAX_DEPTH", "16").parse().unwrap_or(16);
    let timeout_ms: u64 = env_var("TIMEOUT_MS", "2000").parse().unwrap_or(2000);
    let out_dir = PathBuf::from(env_var("OUT", "target/fuzz"));
    let allow_impldef = env_var("ALLOW_IMPLDEF", "0") == "1";
    let stop_after = Stage::parse(&env_var("STAGE", "vm"))
        .unwrap_or_else(|| fail(2, "STAGE must be parse|codegen|vm"));
    let verbose = env_var("VERBOSE", "0") == "1";
    let replay: Option<usize> = match env::var("REPLAY") {
        Ok(spec) => {
            let (s, i) = spec
                .split_once(':')
                .unwrap_or_else(|| fail(2, "REPLAY must be <seed>:<index>"));
            seed = s
                .parse()
                .unwrap_or_else(|_| fail(2, "REPLAY seed must be an integer"));
            Some(
                i.parse()
                    .unwrap_or_else(|_| fail(2, "REPLAY index must be an integer")),
            )
        }
        Err(_) => None,
    };

    let fixture = fixture_path(manifest_dir);
    let grammar = load_db_core_grammar(manifest_dir).unwrap_or_else(|e| fail(1, &e.to_string()));
    let tables = catalog_of(&fixture).unwrap_or_else(|e| fail(1, &e.to_string()));
    let config = WalkerConfig {
        max_depth,
        scope: VBlockScope::All,
        dialect: Some(Box::new(RowDialect::from_tables(&tables))),
    };
    let mut walker = Walker::new(&grammar, Section::Sqlite, seed, config);

    // Generation is deterministic in (seed, index): to replay statement
    // k, generate k+1 statements and keep the last.
    let want = replay.map_or(n, |i| i.saturating_add(1));
    let mut statements: Vec<(usize, String)> = Vec::with_capacity(want);
    for i in 0..want {
        match walker.generate("sql-stmt") {
            Ok(stmt) => statements.push((i, stmt)),
            Err(e) => eprintln!("[{i}] generation error: {e}"),
        }
    }
    if let Some(i) = replay {
        statements.retain(|(idx, _)| *idx == i);
    }

    let run_config = RunConfig {
        fixture,
        timeout: Duration::from_millis(timeout_ms),
        allow_impldef,
        stop_after,
    };
    let mut runner = Runner::new(run_config).unwrap_or_else(|e| fail(1, &e.to_string()));

    let summary = if replay.is_some() || verbose {
        let mut sink = if replay.is_some() {
            None
        } else {
            Some(FindingsSink::open(&out_dir).unwrap_or_else(|e| fail(1, &e.to_string())))
        };
        let mut acc = fuzz_run::RunSummary::default();
        for (i, sql) in statements {
            let outcome = if allow_impldef || !Runner::is_impldef(&sql) {
                Some(
                    runner
                        .run_one(&sql)
                        .unwrap_or_else(|e| fail(1, &e.to_string())),
                )
            } else {
                None
            };
            let part = summarize(seed, i, &sql, outcome.as_ref(), sink.as_mut());
            let label = match part.findings.first() {
                Some(f) => format!("FINDING {} in {}: {}", f.class, f.stage.as_str(), f.message),
                None if part.ok == 1 => "ok".to_string(),
                None if part.skipped_impldef == 1 => "skipped (impldef)".to_string(),
                None => match &outcome {
                    Some(Outcome::Rejected {
                        stage,
                        kind,
                        message,
                    }) => format!(
                        "rejected at {} ({}): {}",
                        stage.as_str(),
                        kind.as_str(),
                        message.lines().next().unwrap_or_default()
                    ),
                    _ => String::new(),
                },
            };
            println!("[{i}] {sql}\n     -> {label}");
            merge(&mut acc, part);
        }
        acc
    } else {
        let mut sink = FindingsSink::open(&out_dir).unwrap_or_else(|e| fail(1, &e.to_string()));
        runner
            .run_all(seed, statements, Some(&mut sink))
            .unwrap_or_else(|e| fail(1, &e.to_string()))
    };

    let (exercised, in_scope) = walker.coverage();
    eprint!("{}", summary.render());
    eprintln!(
        "grammar coverage: {exercised}/{in_scope} alternatives; workers spawned: {}",
        runner.workers_spawned()
    );
    if !summary.findings.is_empty() {
        if replay.is_none() {
            eprintln!("findings written under {}", out_dir.display());
        }
        std::process::exit(3);
    }
}

/// One-statement summary for the verbose/replay path (the batch path
/// uses `Runner::run_all` directly).
fn summarize(
    seed: u64,
    index: usize,
    sql: &str,
    outcome: Option<&Outcome>,
    sink: Option<&mut FindingsSink>,
) -> fuzz_run::RunSummary {
    let mut part = fuzz_run::RunSummary {
        total: 1,
        ..Default::default()
    };
    match outcome {
        None => part.skipped_impldef = 1,
        Some(Outcome::Ok) => part.ok = 1,
        Some(Outcome::Rejected { stage, kind, .. }) => {
            part.rejected.insert((*stage, kind.clone()), 1);
        }
        Some(o @ (Outcome::Panic { .. } | Outcome::Hang { .. } | Outcome::Corrupted { .. })) => {
            if let Some(f) = fuzz_run::Finding::from_outcome(seed, index, sql, o, 0) {
                if let Some(sink) = sink {
                    sink.record(&f)
                        .unwrap_or_else(|e| fail(1, &format!("writing finding: {e}")));
                }
                part.findings.push(f);
            }
        }
    }
    part
}

fn merge(acc: &mut fuzz_run::RunSummary, part: fuzz_run::RunSummary) {
    acc.total = acc.total.saturating_add(part.total);
    acc.ok = acc.ok.saturating_add(part.ok);
    acc.skipped_impldef = acc.skipped_impldef.saturating_add(part.skipped_impldef);
    for (k, v) in part.rejected {
        let n = acc.rejected.entry(k).or_insert(0);
        *n = n.saturating_add(v);
    }
    acc.findings.extend(part.findings);
}
