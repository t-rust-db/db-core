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
//!   JOBS=n                  parallel lanes, each with its own fixture copy
//!                           (default 1; statements dealt round-robin;
//!                           ignored under VERBOSE/REPLAY)
//!   ALLOW_IMPLDEF=1         also run RANDOM()/CURRENT_* statements
//!   ORACLE=1                also run each statement on sqlite3 and compare
//!   ORACLE_BIN=path         the sqlite3 binary (default sqlite3; 3.53.4 expected)
//!   ALLOW_ORACLE_VERSION=1  accept another sqlite3 version
//!   REDUCE=0                skip ddmin reduction of repro scripts (JOBS=1 only)
//!   VERBOSE=1               print every statement and its outcome
//!   UNEXERCISED=1           list in-scope grammar alternatives never chosen
//!
//! Exit codes: 0 clean, 3 totality findings (panic/hang/corruption),
//! 4 only differential findings (wrong-answer/gap/over-permissive), 2 bad
//! arguments, 1 setup failure.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use fuzz_gen::{load_db_core_grammar, Section, VBlockScope, Walker, WalkerConfig};
use fuzz_run::{
    catalog_of, fixture_path, reduce_finding, run_parallel, FindingsSink, OracleConfig, Outcome,
    RowDialect, RunConfig, Runner, Stage,
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
    let jobs: usize = env_var("JOBS", "1").parse().unwrap_or(1).max(1);
    let oracle = if env_var("ORACLE", "0") == "1" {
        Some(OracleConfig {
            bin: env_var("ORACLE_BIN", "sqlite3"),
            allow_version_mismatch: env_var("ALLOW_ORACLE_VERSION", "0") == "1",
        })
    } else {
        None
    };
    let reduce = env_var("REDUCE", "1") != "0";
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
        oracle,
    };
    let (summary, workers_spawned) = if replay.is_some() || verbose {
        let mut runner =
            Runner::new(run_config.clone()).unwrap_or_else(|e| fail(1, &e.to_string()));
        let mut sink = if replay.is_some() {
            None
        } else {
            Some(FindingsSink::open(&out_dir).unwrap_or_else(|e| fail(1, &e.to_string())))
        };
        let mut acc = fuzz_run::RunSummary::default();
        for (i, sql) in statements {
            let (outcome, cmp) = if allow_impldef || !Runner::is_impldef(&sql) {
                let (o, c) = runner
                    .run_one_compared(&sql)
                    .unwrap_or_else(|e| fail(1, &e.to_string()));
                (Some(o), c)
            } else {
                (None, None)
            };
            let mut part = summarize(seed, i, &sql, outcome.as_ref(), sink.as_mut());
            if let Some(c) = &cmp {
                part.verdicts.insert(c.verdict, 1);
                if c.verdict.is_finding() {
                    let f = fuzz_run::Finding::from_comparison(seed, i, &sql, c, 0);
                    if let Some(sink) = sink.as_mut() {
                        sink.record(&f)
                            .unwrap_or_else(|e| fail(1, &format!("writing finding: {e}")));
                    }
                    part.findings.push(f);
                }
            }
            let verdict = cmp
                .as_ref()
                .map(|c| format!(" [{}]", c.verdict.as_str()))
                .unwrap_or_default();
            let label = match part.findings.first() {
                Some(f) => format!(
                    "FINDING {} in {}: {}{verdict}",
                    f.class,
                    f.stage.as_str(),
                    f.message
                ),
                None if part.ok == 1 => format!("ok{verdict}"),
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
            acc.merge(part);
        }
        acc.oracle_version = runner.oracle_version().map(str::to_string);
        (acc, runner.workers_spawned())
    } else {
        let mut sink = FindingsSink::open(&out_dir).unwrap_or_else(|e| fail(1, &e.to_string()));
        run_parallel(&run_config, jobs, seed, statements, Some(&mut sink))
            .unwrap_or_else(|e| fail(1, &e.to_string()))
    };

    let mut summary = summary;
    if reduce && jobs == 1 && replay.is_none() {
        let reducible = ["wrong-answer", "over-permissive", "gap", "corruption"];
        let sink = FindingsSink::open(&out_dir).unwrap_or_else(|e| fail(1, &e.to_string()));
        // One reduction per (class, statement shape): the other members
        // of a group are the same bug with different literals.
        let firsts: Vec<usize> = summary
            .deduped()
            .into_iter()
            .map(|(_, _, f)| f.index)
            .collect();
        for f in &mut summary.findings {
            if firsts.contains(&f.index) && reducible.contains(&f.class) && f.script.len() > 1 {
                let (reduced, probes) = reduce_finding(&run_config, f, 200);
                if !reduced.is_empty() && reduced.len() < f.script.len() {
                    eprintln!(
                        "reduced seed {} #{}: {} -> {} statements ({probes} probes)",
                        f.seed,
                        f.index,
                        f.script.len(),
                        reduced.len()
                    );
                    f.reduced = reduced;
                    sink.write_sql(f)
                        .unwrap_or_else(|e| fail(1, &format!("rewriting finding: {e}")));
                }
            }
        }
    }
    if replay.is_none() {
        let sink = FindingsSink::open(&out_dir).unwrap_or_else(|e| fail(1, &e.to_string()));
        sink.write_report(&report(seed, n, max_depth, &summary))
            .unwrap_or_else(|e| fail(1, &format!("writing report: {e}")));
    }

    let (exercised, in_scope) = walker.coverage();
    eprint!("{}", summary.render());
    eprintln!(
        "grammar coverage: {exercised}/{in_scope} alternatives; jobs: {jobs}; workers spawned: {workers_spawned}"
    );
    let unexercised = walker.unexercised();
    if !unexercised.is_empty() && (verbose || env_var("UNEXERCISED", "0") == "1") {
        eprintln!("unexercised alternatives:");
        for (rule, idx, text) in &unexercised {
            eprintln!("  {rule}[{idx}] ::= {text}");
        }
    }
    if !summary.findings.is_empty() {
        if replay.is_none() {
            eprintln!("findings written under {}", out_dir.display());
        }
        let totality = summary
            .findings_of("panic")
            .saturating_add(summary.findings_of("hang"))
            .saturating_add(summary.findings_of("corruption"));
        std::process::exit(if totality > 0 { 3 } else { 4 });
    }
}

/// `report.md`: the run's parameters, the summary block, and one section
/// per finding with its diff and repro file.
fn report(seed: u64, n: usize, max_depth: usize, summary: &fuzz_run::RunSummary) -> String {
    let mut r = format!(
        "# fuzz-sql report\n\nseed {seed}, N {n}, max depth {max_depth}{}\n\n```\n{}```\n",
        summary
            .oracle_version
            .as_deref()
            .map(|v| format!(", oracle sqlite {v}"))
            .unwrap_or_default(),
        summary.render()
    );
    for (_, count, f) in summary.deduped() {
        r.push_str(&format!(
            "\n## {} in {} -- seed {} #{}{} (`{}-{}.sql`)\n\n```sql\n{}\n```\n",
            f.class,
            f.stage.as_str(),
            f.seed,
            f.index,
            if count > 1 {
                format!(", {count} occurrences")
            } else {
                String::new()
            },
            f.seed,
            f.index,
            f.sql
        ));
        if !f.message.is_empty() {
            r.push_str(&format!("\n{}\n", f.message));
        }
        if !f.detail.is_empty() {
            r.push_str(&format!("\n```\n{}```\n", f.detail));
        }
        if !f.reduced.is_empty() {
            r.push_str(&format!(
                "\nreduced to {} of {} statements\n",
                f.reduced.len(),
                f.script.len()
            ));
        }
    }
    r
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
