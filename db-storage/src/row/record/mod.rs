//! SQLite record format decoding: varints, serial types, and the record
//! header walk. Pure computation, no I/O -- the b-tree layer hands this
//! module raw payload bytes; this module never reads a page itself.
//!
//! Extracted verbatim from sqlite-rs's private `src/record` (db-core#13,
//! Phase 1 of the sqlite-rs -> db-core integration plan) -- moved as-is,
//! not restyled, per ADR 0001's convention of converging toward
//! sqlite-rs's existing shape rather than the other way around.
//! `RecordError` stays a hand-rolled enum (no `thiserror`), matching every
//! other db-core error type's independence -- see ADR 0001.
//!
//! `Value`/`TextEncoding` here are deliberately their own type, not
//! `sql_types::Value`: record values need a `Blob` variant `sql_types`
//! doesn't have, and use `Rc<str>`/`Rc<[u8]>` for cheap cloning during
//! decode where `sql_types::Value::Str` uses an owned `String` -- these
//! aren't the same concept wearing different names, they're value
//! representations for two different layers (on-disk record bytes vs.
//! the parser/executor's runtime values).

#![forbid(unsafe_code)]

mod decode;
mod encode;
mod error;
mod varint;

// The value model is db-core's (ADR 0010, t-rust-db/db-core#83): the b-tree
// decodes straight into the type `db_core::vm::row` executes over.
pub use db_core::value::{compare_text, Collation, TextEncoding, Value};
pub use decode::{
    decode_column, decode_record, decode_record_only_into, decode_record_upto, decode_serial_value,
    parse_header_into, record_column_count,
};
pub use encode::{encode_record, encode_record_into, encode_varint};
pub use error::RecordError;
pub use varint::decode_varint;
