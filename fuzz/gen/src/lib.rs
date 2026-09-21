//! EBNF rule-graph parser and seeded generator over db-core's
//! `src/parser/grammar.ebnf` (db-core#544, first slice of epic #543).
//!
//! This crate is a quality-assessment instrument, not a benchmark: it
//! turns the grammar that already documents each frontend's accepted
//! syntax into a source of well-formed random statements for downstream
//! totality/differential runners (db-core#545+).

pub mod ebnf;
pub mod walker;

pub use ebnf::{Grammar, GrammarError, Section};
pub use walker::{Dialect, Rng, VBlockScope, WalkError, Walker, WalkerConfig};

/// Parses db-core's own `src/parser/grammar.ebnf` from its canonical
/// location relative to `manifest_dir` (pass `env!("CARGO_MANIFEST_DIR")`
/// from the caller so the path resolves regardless of the current
/// working directory).
pub fn load_db_core_grammar(manifest_dir: &str) -> Result<Grammar, GrammarError> {
    let path = std::path::Path::new(manifest_dir)
        .join("..")
        .join("..")
        .join("src")
        .join("parser")
        .join("grammar.ebnf");
    let src = std::fs::read_to_string(&path)
        .map_err(|e| GrammarError(format!("reading {}: {e}", path.display())))?;
    ebnf::parse_source(&src)
}
