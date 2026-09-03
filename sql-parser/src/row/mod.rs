//! sqlite-rs's section -- one of `sql-parser`'s two grammar sections (see
//! crate root docs and `ADR 0002`, as amended).
//!
//! **Partially implemented**, migrated in from sqlite-rs's own parser
//! (`src/parser/*`) as #23's slices land:
//! - [`tokenizer`] -- done.
//! - [`ast`] -- done. Its own AST, not `sql_expr::Query` (see its doc
//!   comment and `ADR 0002`'s amendment for why).
//! - The grammar itself (`src/parser/grammar.rs`, 3,106 lines: DDL, DML,
//!   transactions, `PRAGMA`, ...) and its error/printer modules are not
//!   yet ported -- tracked as further #23 slices.
//!
//! [`super::column`] is the reference for what "done" looks like once
//! the grammar lands: shares this crate's [`crate::Span`], its own
//! `ParseError` (not `column`'s -- see `ADR 0002`), and real test
//! coverage ported from sqlite-rs's existing parser test suite.

pub mod ast;
pub mod tokenizer;
