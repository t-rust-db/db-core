//! Shared source-location primitive for t-rust-db error types.
//!
//! # What this crate is
//!
//! [`Span`] only. That's a deliberate choice, made by studying sqlite-rs
//! (the more mature codebase in this family) rather than guessing: every
//! error type there (`ParseFail`/`ParseOutcome` in `src/parser/error.rs`,
//! `PagerError`, `BtreeError`, `RecordError`, ...) is its own hand-rolled
//! enum with a manual `Display`/`std::error::Error` impl, wrapping
//! lower-layer errors as variants (e.g. `PagerError::Wal { path, source:
//! WalError }`) rather than funneling everything through one shared base
//! type. There is no `thiserror`/`anyhow` anywhere in sqlite-rs's
//! `Cargo.toml`, and no crate-wide `Error` enum to unify them.
//!
//! # What this crate deliberately is NOT
//!
//! - **Not a unifying `Error` enum.** Following sqlite-rs's own
//!   architecture, each t-rust-db crate keeps its own error type
//!   (`sql_parser::ParseError`, `sql_vm::batch::VmError`, ...), composed
//!   by wrapping, not collapsed into one shared variant list.
//! - **Not a derive-macro/error-builder crate.** sqlite-rs hand-writes
//!   every `Display` impl; so does everything in `db-core` so far. Adding
//!   our own macro machinery here would be new complexity sqlite-rs
//!   itself doesn't carry, for a problem (repetitive `Display` impls)
//!   that's real but small at this codebase's actual size.
//! - **Not a dependency on any external error crate.** Zero dependencies,
//!   by request and by precedent — every other `db-core` crate is
//!   zero-dep already.
//!
//! # What [`Span`] is for
//!
//! A source location a parser/tokenizer error can point at, so a
//! consumer (a REPL, an IDE) can highlight *where* something went wrong
//! -- not just read a message. Field-for-field identical to sqlite-rs's
//! own `Span` (`src/parser/tokenizer.rs`): 1-based line/column (what a
//! human reading source text expects), plus byte offset/length (what
//! code needs to slice the original source). Consumers embed `span`
//! fields directly in their own error variants, the same way sqlite-rs's
//! `ParseFail::Invalid { message, span }` does -- there's no generic
//! `Located<E>` wrapper here, because sqlite-rs doesn't have one either;
//! inlining the field is simpler and matches the reference.

#![forbid(unsafe_code)]

/// Source location of a token, mirroring sqlite-rs's own `Span` exactly
/// (`src/parser/tokenizer.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    /// 1-based line number of the token's first character.
    pub line: u32,
    /// 1-based column number of the token's first character.
    pub column: u32,
    /// Byte offset of the token's first character in the source.
    pub offset: u32,
    /// Length in bytes of the token's source text.
    pub len: u32,
}

impl Span {
    /// A span with no real location -- for errors raised before any
    /// token exists (e.g. an empty input), or by code that hasn't been
    /// migrated to track real spans yet. Distinct from a real
    /// `Span { line: 1, column: 1, .. }`, which is a genuine location;
    /// callers that care about the distinction should treat this as
    /// "unknown," not "start of input."
    pub const UNKNOWN: Span = Span {
        line: 0,
        column: 0,
        offset: 0,
        len: 0,
    };

    pub fn is_unknown(&self) -> bool {
        *self == Span::UNKNOWN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_span_is_distinguishable_from_a_real_one_at_line_one() {
        let real = Span {
            line: 1,
            column: 1,
            offset: 0,
            len: 1,
        };
        assert!(Span::UNKNOWN.is_unknown());
        assert!(!real.is_unknown());
        assert_ne!(Span::UNKNOWN, real);
    }

    #[test]
    fn default_equals_unknown() {
        assert_eq!(Span::default(), Span::UNKNOWN);
    }
}
