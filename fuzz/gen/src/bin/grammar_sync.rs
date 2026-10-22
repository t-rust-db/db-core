//! CLI entry point for `make check-grammar-sync` (db-core#565): compares
//! db-core's SQLITE-RS ONLY grammar section with sqlite-rs's
//! `.openspec/grammar/sqlite.ebnf`, prints every disagreement, and exits 1
//! if there is any.
//!
//! Usage: `grammar_sync <path to sqlite-rs's sqlite.ebnf>`.

use std::process::exit;

use fuzz_gen::{ebnf, load_db_core_grammar, sync};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: grammar_sync <sqlite-rs/.openspec/grammar/sqlite.ebnf>");
        exit(2);
    };
    let db_core = load_db_core_grammar(env!("CARGO_MANIFEST_DIR")).unwrap_or_else(|e| {
        eprintln!("db-core grammar: {e}");
        exit(2);
    });
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        exit(2);
    });
    let sqlite_rs = ebnf::parse_section(&src).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        exit(2);
    });
    let drift = sync::compare(&db_core, &sqlite_rs);
    if drift.is_empty() {
        println!(
            "check-grammar-sync: ok -- {} rules agree with {path}",
            sqlite_rs.len()
        );
        return;
    }
    for d in &drift {
        println!("  {d}");
    }
    println!(
        "check-grammar-sync: {} disagreement(s) between src/parser/grammar.ebnf and {path}",
        drift.len()
    );
    exit(1);
}
