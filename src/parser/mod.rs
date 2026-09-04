//! Tokenizer and recursive-descent parser producing `crate::expr::Query`, as
//! two grammar sections (see `ADR 0002` in `db-core`'s `.openspec/adr/`),
//! mirroring how `sql-vm` already splits into `batch`/`row`/`stream`
//! executors (ADR 0001):
//!
//! - [`column`] -- column-rs's analytics-subset grammar (`SELECT ...`,
//!   restricted to what the query VM executes). **Implemented.** On by
//!   default.
//! - [`row`] -- sqlite-rs's full SQLite grammar (DDL, DML, transactions,
//!   `PRAGMA`, ...). **Not yet implemented** -- see its own doc comment.
//!
//! Both sections share this crate's [`Span`] and produce `crate::expr::Query`
//! -- the AST types themselves live in `sql-expr`, not here. Each section
//! is gated behind its own Cargo feature (`column`/`row`), so a consumer
//! that only needs column-rs's subset (column-rs itself) compiles with
//! `default-features = false, features = ["column"]` and never builds
//! `row`'s (eventually much larger) grammar or its dependencies.
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
pub use column::{parse, parse_explain, ParseError, Result};

#[cfg(feature = "parser-row")]
pub mod row;
