//! `vm::row`'s coercion/checked-arithmetic logic is the crate-wide one
//! (ADR 0010/#122) — re-exported here so `super::coerce::*` paths inside
//! `vm::row` stay put.

pub use crate::coerce::*;
