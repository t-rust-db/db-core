// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
use std::rc::Rc;

/// A single decoded column value, per SQLite's dynamic type system.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// A signed integer, stored as 1/2/3/4/6/8 bytes on disk per the serial type.
    Integer(i64),
    /// An 8-byte IEEE 754 floating-point value.
    Real(f64),
    /// A text value, decoded according to the database's `TextEncoding`.
    Text(Rc<str>),
    /// An uninterpreted byte sequence.
    Blob(Rc<[u8]>),
}

/// The database's text encoding, from database header byte 56.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// UTF-8.
    Utf8,
    /// UTF-16 little-endian.
    Utf16Le,
    /// UTF-16 big-endian.
    Utf16Be,
}
