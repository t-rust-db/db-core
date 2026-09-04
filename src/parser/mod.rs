//! Tokenizer and recursive-descent parser, as two grammar sections (see
//! `ADR 0002` in `db-core`'s `.openspec/adr/`), mirroring how `sql-vm`
//! already splits into `batch`/`row`/`stream` executors (ADR 0001):
//!
//! - [`row`] -- sqlite-rs's full SQLite grammar (DDL, DML, transactions,
//!   `PRAGMA`, ...), ~7,400 lines (`ast`/`grammar`/`tokenizer`/`error`/
//!   `printer`). Produces `ast::Select`. This is now the *only* tokenizer
//!   and grammar in the crate (#57).
//! - [`column`] -- column-rs's analytics-subset grammar (`SELECT ...`,
//!   restricted to what the query VM executes). On by default. Has no
//!   tokenizer or parser of its own: [`column::parse`]/
//!   [`column::parse_explain`] parse with `row`'s ([`row::parse_select`]/
//!   [`row::parse_explain`]) and then lower the resulting `ast::Select`
//!   into `crate::expr::Query` -- the shape `codegen::batch`/`emit::batch`
//!   still expect -- rejecting anything outside the analytics subset
//!   (`WITH`, `UNION`, a real multi-way join, ...) at that lowering step
//!   with `ParseError`. This is ADR 0002's second amendment: one grammar,
//!   with `column`'s subset *enforced*, not parsed by a second grammar
//!   that simply can't recognize the rest.
//!
//! Because `column` is now a thin adapter over `row`, the `parser-column`
//! Cargo feature implies `parser-row` (see `Cargo.toml`) -- there is no
//! longer a way to compile `column`'s grammar without also compiling
//! `row`'s tokenizer/grammar/AST underneath it. A consumer that only
//! wants `row`'s own DDL/DML/transaction grammar (not column-rs's
//! analytics subset) still compiles with `default-features = false,
//! features = ["parser-row"]` alone, skipping `column`'s adapter.
//!
//! See `grammar.ebnf` (this crate's root) for the actual EBNF grammar
//! both sections implement, and `ALIGNMENT.md` for what was checked
//! against sqlite-rs's own conventions along the way.
//!
//! `column`'s public items (`parse`, `parse_explain`, `ParseError`) are
//! re-exported at the crate root, unchanged from before this split, so
//! existing consumers don't need to update call sites.

#![forbid(unsafe_code)]

mod span;
pub use span::Span;

#[cfg(feature = "parser-column")]
pub mod column;
#[cfg(feature = "parser-column")]
pub use column::{parse, parse_explain, Explain, ParseError, Result};

#[cfg(any(feature = "parser-column", feature = "parser-row"))]
pub mod row;
