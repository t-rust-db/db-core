//! sqlite-rs's section -- one of `sql-parser`'s two grammar sections (see
//! crate root docs and `ADR 0002`).
//!
//! **Not yet implemented.** This is the eventual home for SQLite's full
//! grammar (DDL, DML, transactions, `PRAGMA`, ...) as parsed by
//! sqlite-rs's own parser (`src/parser/*`, 7,361 lines) -- migrated in,
//! not reimplemented from scratch, once that source and the
//! `t-rust-db/grammar` repo are available to work from. `sql_expr::Query`
//! likely needs new variants for constructs that have no equivalent in
//! column-rs's `SELECT`-shaped analytics subset (see `ADR 0002`'s
//! Consequences) -- deliberately not added speculatively here.
//!
//! [`super::column`] is the reference for what "done" looks like here:
//! shares this crate's [`crate::Span`], its own `ParseError` (not
//! `column`'s -- see `ADR 0002`), and real test coverage ported from
//! sqlite-rs's existing parser test suite.
