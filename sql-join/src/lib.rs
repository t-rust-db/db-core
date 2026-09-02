//! Shared join infrastructure for t-rust-db engines. Starts with
//! [`JoinHashTable`] -- the hash table representation, not join semantics
//! or a cost model, is the fix column-rs's join benchmark needs (see
//! `hash_table` module docs). `JoinKind`/NULL-safe semantics and a
//! build-side cost model are follow-up additions once an engine actually
//! needs them, not built speculatively here.

#![forbid(unsafe_code)]

mod hash_table;

pub use hash_table::JoinHashTable;
