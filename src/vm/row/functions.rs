//! `vm::row`'s scalar-function registry is the crate-wide one (ADR
//! 0010/#122) — re-exported here so `super::functions::*` paths inside
//! `vm::row` stay put.

pub use crate::functions::{call, glob_match, like_match, FunctionError};
