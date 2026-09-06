//! `vm::row`'s comparison logic is the crate-wide one (ADR 0010/#122) —
//! re-exported here so `super::compare::*` paths inside `vm::row` stay put.

pub use crate::compare::compare;
