//! sqlite-rs's section -- one of `sql-parser`'s two grammar sections (see
//! crate root docs and `ADR 0002`).
//!
//! **Partially implemented.** [`tokenizer`] is migrated in from
//! sqlite-rs's own (`src/parser/tokenizer.rs`, #23's first slice). The
//! grammar itself (`src/parser/grammar.rs`, 3,106 lines: DDL, DML,
//! transactions, `PRAGMA`, ...) is not yet ported -- tracked as a
//! follow-up to #23. `sql_expr::Query` likely needs new variants for
//! constructs that have no equivalent in column-rs's `SELECT`-shaped
//! analytics subset (see `ADR 0002`'s Consequences) -- deliberately not
//! added speculatively here, ahead of the grammar that would need them.
//!
//! [`super::column`] is the reference for what "done" looks like once
//! the grammar lands: shares this crate's [`crate::Span`], its own
//! `ParseError` (not `column`'s -- see `ADR 0002`), and real test
//! coverage ported from sqlite-rs's existing parser test suite.

pub mod tokenizer;
