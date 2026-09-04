//! Shared join infrastructure for t-rust-db engines. Starts with
//! [`JoinHashTable`] -- the hash table representation, not join semantics
//! or a cost model, is the fix column-rs's join benchmark needs (see
//! `hash_table` module docs). [`JoinKind`]/[`should_emit`] (see
//! `semantics` module docs for why NULL-safety isn't included) round out
//! join-kind emit logic; a build-side cost model is still a follow-up
//! addition once an engine actually needs one, not built speculatively
//! here.

#![forbid(unsafe_code)]

mod hash_table;
mod semantics;

pub use hash_table::JoinHashTable;
pub use semantics::{should_emit, JoinKind};
