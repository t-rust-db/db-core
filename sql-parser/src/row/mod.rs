//! sqlite-rs's section -- one of `sql-parser`'s two grammar sections (see
//! crate root docs and `ADR 0002`, as amended).
//!
//! **Implemented**, migrated in from sqlite-rs's own parser
//! (`src/parser/*`) as #23's slices landed: [`tokenizer`], [`ast`]
//! (its own AST, not `sql_expr::Query` -- see its doc comment and `ADR
//! 0002`'s amendment for why), [`error`] (the three-way
//! [`ParseOutcome`] and internal [`error::ParseFail`]/[`error::PResult`]),
//! and [`grammar`] (the recursive-descent [`grammar::Parser`] itself).
//! `src/parser/printer.rs` (pretty-printing the AST back to SQL text) is
//! not yet ported -- tracked as a further #23 slice, independent of the
//! parse path above.
//!
//! [`super::column`] is the reference for what "done" looks like: shares
//! this crate's [`crate::Span`], its own `ParseError` (`column`'s, not
//! `row`'s [`ParseOutcome`]/[`error::ParseFail`] -- see `ADR 0002`), and
//! real test coverage -- all ported from sqlite-rs's existing parser
//! test suite, unchanged.

pub mod ast;
pub mod error;
pub mod grammar;
pub mod tokenizer;

pub use error::{
    parse_analyze, parse_begin, parse_commit, parse_create_index, parse_create_table,
    parse_create_view, parse_delete, parse_drop_index, parse_drop_table, parse_drop_view,
    parse_explain, parse_insert, parse_pragma, parse_rollback, parse_select, parse_update,
    ParseOutcome,
};
pub use tokenizer::{ends_with_semicolon, split_statements};
