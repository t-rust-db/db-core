//! CLI entry point for `make fuzz-sql`: generates `N` well-formed
//! statements for `TARGET` (row/column/stream) from `src/parser/
//! grammar.ebnf` and prints them plus a coverage report.
//!
//! This is generation only (db-core#544) -- no execution or oracle
//! comparison yet. That lands with the totality runner (db-core#545)
//! and the differential runner (db-core#546); this binary exists so the
//! generator itself is runnable and inspectable before those land.

use std::env;

use fuzz_gen::{load_db_core_grammar, Section, VBlockScope, Walker, WalkerConfig};

fn env_var(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse_target(raw: &str) -> Result<(Section, &'static str), String> {
    match raw {
        "row" => Ok((Section::Sqlite, "sql-stmt")),
        "column" => Ok((Section::Column, "sql-stmt")),
        "stream" => Ok((Section::Sqlite, "expr")),
        other => Err(format!(
            "unknown TARGET '{other}' (expected row|column|stream)"
        )),
    }
}

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_raw = env_var("TARGET", "row");
    let n: usize = env_var("N", "10").parse().unwrap_or(10);
    let seed: u64 = env_var("SEED", "1").parse().unwrap_or(1);
    let max_depth: usize = env_var("MAX_DEPTH", "16").parse().unwrap_or(16);

    let (section, entry_rule) = match parse_target(&target_raw) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
    };

    let grammar = match load_db_core_grammar(manifest_dir) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    // No externally-tracked "landed V-blocks" list exists yet -- runs
    // everything in scope until a later ticket wires `VBlockScope::
    // Landed` in from CI config (db-core#544's WalkerConfig doc comment).
    let config = WalkerConfig {
        max_depth,
        scope: VBlockScope::All,
    };
    let mut walker = Walker::new(&grammar, section, seed, config);

    for i in 0..n {
        match walker.generate(entry_rule) {
            Ok(stmt) => println!("[{i}] {stmt}"),
            Err(e) => eprintln!("[{i}] generation error: {e}"),
        }
    }

    let (exercised, in_scope) = walker.coverage();
    let pct = if in_scope == 0 {
        0.0
    } else {
        100.0 * exercised as f64 / in_scope as f64
    };
    eprintln!("\ncoverage: {exercised}/{in_scope} alternatives ({pct:.1}%)");
}
