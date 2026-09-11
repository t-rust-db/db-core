//! Thin re-export of [`crate::json_path`] for `jsonl` (#319) and
//! `detect` (#321) callers -- the parser itself is feature-free
//! (ADR 0011) so `functions::json_extract` (#307) can share it too;
//! moving it to the crate root means one JSON parser, not two.

pub use crate::json_path::{parse_object, parse_value, JsonValue};
