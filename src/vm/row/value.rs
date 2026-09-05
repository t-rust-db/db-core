//! `vm::row`'s value model is the crate-wide one (ADR 0010) — re-exported
//! here so `super::value::*` paths inside `vm::row` stay put.

pub use crate::value::{compare_text, format_real, Collation, TextEncoding, Value};
