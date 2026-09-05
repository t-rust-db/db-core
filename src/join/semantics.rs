//! Join-kind emit semantics: `JoinKind` and [`should_emit`].
//!
//! NULL-safe key equality is deliberately **not** here. `JoinHashTable<K,
//! V>` is generic over `K` -- the caller chooses the key representation
//! (see `hash_table` module docs) -- so `sql-join` never sees a
//! `crate::types::Value` to special-case `NULL` on. column-rs's own
//! `JoinKey` enum (`column-rs/src/query.rs`) already does this at the
//! point where a `Value` is converted to a hashable key: `Value::Null`
//! maps to `JoinKey::Null`, and since `JoinKey` derives `PartialEq`,
//! `Null != Null` never matches -- exactly SQL's NULL-safe-equality rule
//! (`NULL = NULL` is not true). That conversion step is where NULL-safety
//! naturally belongs, one layer up from this crate, for any caller: it's
//! a property of *how a caller turns its value type into `K`*, not of the
//! hash table or of `should_emit`'s post-probe emit decision. Baking a
//! NULL-safety rule into `sql-join` itself would mean either constraining
//! `K` to a specific value enum (defeating the point of being generic) or
//! duplicating a check every sane `K` conversion already needs to get
//! right on its own.

/// Which rows an equi-join keeps, given whether a probe-side row matched
/// anything on the build side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// Only rows with a match on both sides.
    Inner,
    /// Every left row; unmatched ones paired with a NULL-filled right side.
    Left,
    /// Every right row; unmatched ones paired with a NULL-filled left side.
    Right,
    /// Every row from both sides; unmatched ones NULL-filled on the other side.
    Full,
    /// Left rows that have at least one match in right -- right's columns
    /// are never emitted (no NULL-fill case, unlike `Left`).
    Semi,
    /// Left rows that have **no** match in right.
    Anti,
}

/// Whether a probe attempt should emit a row, given `kind` and whether the
/// left/right side of that attempt had a match. Called once per row for
/// `Inner`/`Left`/`Right`/`Full`/`Semi` (probing from the side that drives
/// iteration), and is also the right predicate for `Anti`'s "no match at
/// all" case.
///
/// `left_matched`/`right_matched` describe the *same* probe outcome from
/// each side's perspective -- for a single-match equi-join probe they're
/// equal (a match found is a match on both sides at once); the two
/// parameters exist so one function serves both a left-driven and a
/// right-driven probe loop without the caller needing to swap kind
/// semantics depending on which side it iterates.
pub fn should_emit(kind: JoinKind, left_matched: bool, right_matched: bool) -> bool {
    match kind {
        JoinKind::Inner => left_matched && right_matched,
        JoinKind::Left => left_matched || !right_matched,
        JoinKind::Right => right_matched || !left_matched,
        JoinKind::Full => true,
        JoinKind::Semi => left_matched,
        JoinKind::Anti => !left_matched,
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
    fn inner_only_emits_on_match() {
        assert!(should_emit(JoinKind::Inner, true, true));
        assert!(!should_emit(JoinKind::Inner, true, false));
        assert!(!should_emit(JoinKind::Inner, false, false));
    }

    #[test]
    fn left_emits_matched_and_unmatched_left_rows() {
        assert!(should_emit(JoinKind::Left, true, true));
        assert!(should_emit(JoinKind::Left, false, false));
    }

    #[test]
    fn right_emits_matched_and_unmatched_right_rows() {
        assert!(should_emit(JoinKind::Right, true, true));
        assert!(should_emit(JoinKind::Right, false, false));
    }

    #[test]
    fn full_always_emits() {
        for (l, r) in [(true, true), (true, false), (false, true), (false, false)] {
            assert!(should_emit(JoinKind::Full, l, r));
        }
    }

    #[test]
    fn semi_emits_only_when_left_has_a_match() {
        assert!(should_emit(JoinKind::Semi, true, true));
        assert!(!should_emit(JoinKind::Semi, false, false));
    }

    #[test]
    fn anti_emits_only_when_left_has_no_match() {
        assert!(!should_emit(JoinKind::Anti, true, true));
        assert!(should_emit(JoinKind::Anti, false, false));
    }

    #[test]
    fn join_kind_equality() {
        assert_eq!(JoinKind::Inner, JoinKind::Inner);
        assert_ne!(JoinKind::Inner, JoinKind::Left);
    }
}
