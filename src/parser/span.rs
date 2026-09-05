//! Source-location primitive for `sql-parser`'s [`crate::ParseError`].
//!
//! Formerly its own `sql-error` crate; folded in here (db-core#8) once it
//! became clear `Span` has exactly one consumer. `crate::vm::batch::VmError`
//! deliberately does NOT adopt `Span` -- VM runtime errors use an opcode
//! name as their "location" concept, not a source-text span (there's no
//! source position left at execution time). If a second real consumer for
//! `Span` materializes later, promoting this module back out to its own
//! crate is a cheap, well-understood reversal.
//!
//! Field-for-field identical to sqlite-rs's own `Span`
//! (`src/parser/tokenizer.rs`): 1-based line/column (what a human reading
//! source text expects), plus byte offset/length (what code needs to slice
//! the original source).

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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
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
