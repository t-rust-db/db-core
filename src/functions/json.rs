// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! sqlite3's JSON functions: `json`, `json_array`, `json_object`, `json_set`, ... (#651), with sqlite3 3.53.4's semantics.
//!
//! A port of the relevant parts of sqlite3's `json.c`. sqlite3 parses JSON
//! text into its binary JSONB form and edits that; this module parses into
//! a small tree ([`Node`]) whose leaves keep JSONB's node types and source
//! text, so rendering back to text follows `jsonTranslateBlobToText` byte
//! for byte: a string keeps the escapes it was written with, a JSON5
//! literal (`0x1F`, `.5`, `'single'`, `\x41`, ...) is canonicalised the way
//! sqlite3 does it, and integers/reals print as written.
//!
//! The parser accepts JSON5 exactly as sqlite3 does (`jsonTranslateTextToBlob`,
//! including the error offsets `json_error_position` reports), and paths
//! follow `jsonLookupStep`: `$`, `.key`, `."quoted key"`, `[N]`, `[#]`,
//! `[#-N]`, checked lazily as the walk reaches each step.
//!
//! Two approximations of sqlite3 remain, by necessity:
//!
//! * **Subtypes.** sqlite3 tags the result of a JSON function with a
//!   `JSON` subtype, so `json_array(json('[1]'))` nests `[[1]]` while
//!   `json_array('[1]')` quotes `["[1]"]`. db-core's [`Value`] carries no
//!   subtype, so each JSON-valued result is recorded here by the identity
//!   of its `Rc<str>` allocation (address, length, content hash, thread)
//!   in a small ring of the most recent results ([`mark_json`]). An
//!   argument is "JSON" when it is that same allocation -- which is what
//!   the row VM passes when one call's result feeds another within an
//!   expression, and what a `||` or a column read never is. A freed
//!   result's address can be reused by an unrelated string; that string
//!   is only mistaken for JSON if it also has byte-identical content to
//!   one of the last [`MARK_CAPACITY`] JSON results. `json_extract` (in
//!   `functions.rs`, not this module) does not mark its results.
//! * **JSONB.** A BLOB argument that passes sqlite3's `jsonArgIsJsonb`
//!   check is decoded as JSONB, as sqlite3 does (`json_valid` flags 4/8
//!   and `json_error_position` run the ported `jsonbValidityCheck`). Any
//!   other BLOB is read as JSON text (the "tag-20240123-a" behaviour), and
//!   embedding one as a value raises `JSON cannot hold BLOB values`. A
//!   BLOB whose header passes but whose body is corrupt in a way sqlite3
//!   would only notice mid-operation (a non-text object label, an element
//!   overrunning its container, a truncated JSON5 escape) raises
//!   `malformed JSON` up front instead of sqlite3's partial or
//!   operation-specific result. The `jsonb_*` functions are not provided.

use std::hash::{DefaultHasher, Hasher};
use std::rc::Rc;
use std::sync::Mutex;

use super::{FunctionError, ScalarFn};
use crate::value::Value;

/// The function named `name` (lowercase) at `arity`, if this family has
/// it.
pub(super) fn lookup(name: &str, arity: usize) -> Option<ScalarFn> {
    match (name, arity) {
        ("json", 1) => Some(json_fn),
        ("json_array", _) => Some(json_array),
        ("json_array_length", 1 | 2) => Some(json_array_length),
        ("json_error_position", 1) => Some(json_error_position),
        ("json_insert", _) => Some(json_insert),
        ("json_object", _) => Some(json_object),
        ("json_patch", 2) => Some(json_patch),
        ("json_quote", 1) => Some(json_quote),
        ("json_remove", _) => Some(json_remove),
        ("json_replace", _) => Some(json_replace),
        ("json_set", _) => Some(json_set),
        ("json_type", 1 | 2) => Some(json_type),
        ("json_valid", 1 | 2) => Some(json_valid),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// JSONB node types and the value tree
// ---------------------------------------------------------------------------

/// JSONB `null`.
const J_NULL: u8 = 0;
/// JSONB `true`.
const J_TRUE: u8 = 1;
/// JSONB `false`.
const J_FALSE: u8 = 2;
/// JSONB canonical integer.
const J_INT: u8 = 3;
/// JSONB JSON5 integer (hexadecimal).
const J_INT5: u8 = 4;
/// JSONB canonical real.
const J_FLOAT: u8 = 5;
/// JSONB JSON5 real (missing digits around `.`).
const J_FLOAT5: u8 = 6;
/// JSONB string needing no escapes.
const J_TEXT: u8 = 7;
/// JSONB string with canonical JSON escapes.
const J_TEXTJ: u8 = 8;
/// JSONB string with JSON5 escapes or raw control characters.
const J_TEXT5: u8 = 9;
/// JSONB string holding raw (unescaped) SQL text.
const J_TEXTRAW: u8 = 10;
/// JSONB array.
const J_ARRAY: u8 = 11;
/// JSONB object.
const J_OBJECT: u8 = 12;

/// sqlite3's `JSON_MAX_DEPTH`.
const MAX_DEPTH: u32 = 1000;

/// A parsed JSON value. Leaves keep their JSONB type and source bytes so
/// that rendering reproduces sqlite3's text exactly.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// `null`.
    Null,
    /// `true`.
    True,
    /// `false`.
    False,
    /// A number: its JSONB type (`J_INT`, `J_INT5`, `J_FLOAT`, `J_FLOAT5`)
    /// and its text (a leading `+` already dropped).
    Num(u8, Vec<u8>),
    /// A string.
    Str(JStr),
    /// An array.
    Array(Vec<Node>),
    /// An object: label/value pairs in order, duplicates kept.
    Object(Vec<(JStr, Node)>),
}

/// A JSON string: its JSONB type (`J_TEXT` ... `J_TEXTRAW`) and the bytes
/// between the quotes, escapes as written.
#[derive(Debug, Clone, PartialEq)]
struct JStr {
    /// One of `J_TEXT`, `J_TEXTJ`, `J_TEXT5`, `J_TEXTRAW`.
    kind: u8,
    /// The string's payload.
    raw: Vec<u8>,
}

impl JStr {
    /// Whether the payload contains no escapes (`jsonLabelCompare`'s
    /// `raw` flag).
    fn is_raw(&self) -> bool {
        self.kind == J_TEXT || self.kind == J_TEXTRAW
    }
}

impl Node {
    /// The node's JSONB type code.
    fn jtype(&self) -> u8 {
        match self {
            Node::Null => J_NULL,
            Node::True => J_TRUE,
            Node::False => J_FALSE,
            Node::Num(t, _) => *t,
            Node::Str(s) => s.kind,
            Node::Array(_) => J_ARRAY,
            Node::Object(_) => J_OBJECT,
        }
    }

    /// The number of elements if this is an array, else 0
    /// (`jsonbArrayCount` as `json_array_length` uses it).
    fn array_len(&self) -> usize {
        match self {
            Node::Array(items) => items.len(),
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Byte helpers (sqlite3 reads NUL-terminated buffers: past the end is 0)
// ---------------------------------------------------------------------------

/// `z[i]`, or 0 past the end -- sqlite3's NUL terminator.
fn at(z: &[u8], i: usize) -> u8 {
    z.get(i).copied().unwrap_or(0)
}

/// `i + n` for buffer offsets (saturating; offsets never get near
/// `usize::MAX`).
fn add(i: usize, n: usize) -> usize {
    i.saturating_add(n)
}

/// `z[a..b]`, empty if out of range.
fn span(z: &[u8], a: usize, b: usize) -> &[u8] {
    z.get(a..b).unwrap_or(&[])
}

/// `sqlite3Isdigit`.
fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// `sqlite3Isxdigit`.
fn is_xdigit(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// `sqlite3Isalnum`.
fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// `jsonIsspace`: the four RFC 8259 whitespace bytes.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// `jsonIsOk`: bytes a string can hold verbatim.
fn is_ok(c: u8) -> bool {
    c >= 0x20 && c != b'"' && c != b'\'' && c != b'\\'
}

/// `sqlite3JsonId1`: a letter, `$`, `_` or a non-ASCII byte.
fn is_id1(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'$' || c == b'_' || c >= 0x80
}

/// `sqlite3JsonId2`: [`is_id1`] or a digit.
fn is_id2(c: u8) -> bool {
    is_id1(c) || c.is_ascii_digit()
}

/// `sqlite3StrNICmp(&z[i], pat, pat.len())==0` for a lowercase `pat`.
fn starts_nocase(z: &[u8], i: usize, pat: &[u8]) -> bool {
    pat.iter()
        .enumerate()
        .all(|(k, p)| at(z, add(i, k)).to_ascii_lowercase() == *p)
}

/// `strncmp(&z[i], pat, pat.len())==0`.
fn starts_with_at(z: &[u8], i: usize, pat: &[u8]) -> bool {
    pat.iter().enumerate().all(|(k, p)| at(z, add(i, k)) == *p)
}

/// `strspn(&z[i], jsonSpaces)`.
fn spaces(z: &[u8], i: usize) -> usize {
    let mut n = 0;
    while is_space(at(z, add(i, n))) {
        n = add(n, 1);
    }
    n
}

/// `jsonIs2Hex(&z[i])`.
fn is_2hex(z: &[u8], i: usize) -> bool {
    is_xdigit(at(z, i)) && is_xdigit(at(z, add(i, 1)))
}

/// `jsonIs4Hex(&z[i])`.
fn is_4hex(z: &[u8], i: usize) -> bool {
    is_2hex(z, i) && is_2hex(z, add(i, 2))
}

/// `jsonHexToInt`: the value of a hex digit (garbage in, garbage out).
fn hex_val(h: u8) -> u32 {
    let h = u32::from(h);
    (h.wrapping_add(9u32.wrapping_mul(1 & (h >> 6)))) & 0xf
}

/// `jsonHexToInt4(&z[i])`.
fn hex4(z: &[u8], i: usize) -> u32 {
    (hex_val(at(z, i)) << 12)
        | (hex_val(at(z, add(i, 1))) << 8)
        | (hex_val(at(z, add(i, 2))) << 4)
        | hex_val(at(z, add(i, 3)))
}

/// `json5Whitespace(&z[i])`: the length of the JSON5 whitespace (and
/// comments) starting at `i`.
fn json5_ws(z: &[u8], i: usize) -> usize {
    let mut n = i;
    loop {
        match at(z, n) {
            0x09 | 0x0a | 0x0b | 0x0c | 0x0d | 0x20 => n = add(n, 1),
            b'/' => {
                if at(z, add(n, 1)) == b'*' && at(z, add(n, 2)) != 0 {
                    let mut j = add(n, 3);
                    loop {
                        if at(z, j) == b'/' && at(z, j.saturating_sub(1)) == b'*' {
                            break;
                        }
                        if at(z, j) == 0 {
                            return n.saturating_sub(i);
                        }
                        j = add(j, 1);
                    }
                    n = add(j, 1);
                } else if at(z, add(n, 1)) == b'/' {
                    let mut j = add(n, 2);
                    loop {
                        let c = at(z, j);
                        if c == 0 || c == b'\n' || c == b'\r' {
                            break;
                        }
                        if c == 0xe2
                            && at(z, add(j, 1)) == 0x80
                            && matches!(at(z, add(j, 2)), 0xa8 | 0xa9)
                        {
                            j = add(j, 2);
                            break;
                        }
                        j = add(j, 1);
                    }
                    n = j;
                    if at(z, n) != 0 {
                        n = add(n, 1);
                    }
                } else {
                    break;
                }
            }
            0xc2 if at(z, add(n, 1)) == 0xa0 => n = add(n, 2),
            0xe1 if at(z, add(n, 1)) == 0x9a && at(z, add(n, 2)) == 0x80 => n = add(n, 3),
            0xe2 => {
                let c1 = at(z, add(n, 1));
                let c2 = at(z, add(n, 2));
                let space =
                    c1 == 0x80 && c2 >= 0x80 && (c2 <= 0x8a || matches!(c2, 0xa8 | 0xa9 | 0xaf));
                if space || (c1 == 0x81 && c2 == 0x9f) {
                    n = add(n, 3);
                } else {
                    break;
                }
            }
            0xe3 if at(z, add(n, 1)) == 0x80 && at(z, add(n, 2)) == 0x80 => n = add(n, 3),
            0xef if at(z, add(n, 1)) == 0xbb && at(z, add(n, 2)) == 0xbf => n = add(n, 3),
            _ => break,
        }
    }
    n.saturating_sub(i)
}

// ---------------------------------------------------------------------------
// Text -> tree (jsonTranslateTextToBlob)
// ---------------------------------------------------------------------------

/// Parser state: `iErr`, `iDepth` and `hasNonstd` of sqlite3's
/// `JsonParse`.
struct Parse {
    /// Byte offset of the error (or of the last delimiter seen).
    err: usize,
    /// Current nesting depth.
    depth: u32,
    /// Whether any JSON5 extension was used.
    nonstd: bool,
}

/// One step of [`translate`]: a value and the offset after it, or one of
/// sqlite3's negative result codes.
enum Tr {
    /// A value and the offset just past it.
    Val(Node, usize),
    /// End of input (0).
    End,
    /// `}` at `err` (-2).
    CloseBrace,
    /// `]` at `err` (-3).
    CloseBracket,
    /// `,` at `err` (-4).
    Comma,
    /// `:` at `err` (-5).
    Colon,
    /// Syntax error at `err` (-1).
    Error,
}

/// `jsonTranslateTextToBlob(pParse, i)`.
fn translate(z: &[u8], p: &mut Parse, start: usize) -> Tr {
    let mut i = start;
    loop {
        let c = at(z, i);
        return match c {
            b'{' => parse_object(z, p, i),
            b'[' => parse_array(z, p, i),
            b'\'' => {
                p.nonstd = true;
                parse_string(z, p, i)
            }
            b'"' => parse_string(z, p, i),
            b't' => {
                if starts_with_at(z, i, b"true") && !is_alnum(at(z, add(i, 4))) {
                    Tr::Val(Node::True, add(i, 4))
                } else {
                    p.err = i;
                    Tr::Error
                }
            }
            b'f' => {
                if starts_with_at(z, i, b"false") && !is_alnum(at(z, add(i, 5))) {
                    Tr::Val(Node::False, add(i, 5))
                } else {
                    p.err = i;
                    Tr::Error
                }
            }
            b'+' => {
                p.nonstd = true;
                parse_number(z, p, i, 0)
            }
            b'.' => {
                if is_digit(at(z, add(i, 1))) {
                    p.nonstd = true;
                    parse_number_tail(z, p, i, 3)
                } else {
                    p.err = i;
                    Tr::Error
                }
            }
            b'-' | b'0'..=b'9' => parse_number(z, p, i, 0),
            b'}' => {
                p.err = i;
                Tr::CloseBrace
            }
            b']' => {
                p.err = i;
                Tr::CloseBracket
            }
            b',' => {
                p.err = i;
                Tr::Comma
            }
            b':' => {
                p.err = i;
                Tr::Colon
            }
            0 => Tr::End,
            0x09 | 0x0a | 0x0d | 0x20 => {
                i = add(i, add(1, spaces(z, add(i, 1))));
                continue;
            }
            0x0b | 0x0c | b'/' | 0xc2 | 0xe1 | 0xe2 | 0xe3 | 0xef => {
                let j = json5_ws(z, i);
                if j > 0 {
                    i = add(i, j);
                    p.nonstd = true;
                    continue;
                }
                p.err = i;
                Tr::Error
            }
            b'n' if starts_with_at(z, i, b"null") && !is_alnum(at(z, add(i, 4))) => {
                Tr::Val(Node::Null, add(i, 4))
            }
            _ => nan_inf(z, p, i),
        };
    }
}

/// The `aNanInfName[]` fallback: `inf`, `infinity`, `NaN`, `QNaN`,
/// `SNaN` in any case.
fn nan_inf(z: &[u8], p: &mut Parse, i: usize) -> Tr {
    const NAMES: [(&[u8], bool); 5] = [
        (b"inf", true),
        (b"infinity", true),
        (b"nan", false),
        (b"qnan", false),
        (b"snan", false),
    ];
    for (name, is_float) in NAMES {
        if !starts_nocase(z, i, name) || is_alnum(at(z, add(i, name.len()))) {
            continue;
        }
        p.nonstd = true;
        let node = if is_float {
            Node::Num(J_FLOAT, b"9e999".to_vec())
        } else {
            Node::Null
        };
        return Tr::Val(node, add(i, name.len()));
    }
    p.err = i;
    Tr::Error
}

/// The JSONB number type for sqlite3's `t` bits (0x01 JSON5, 0x02 real).
fn num_type(t: u8) -> u8 {
    match t {
        0 => J_INT,
        1 => J_INT5,
        2 => J_FLOAT,
        _ => J_FLOAT5,
    }
}

/// `parse_number_finish`: the number node for `z[i..j]`, dropping a
/// leading `+`.
fn number_node(z: &[u8], i: usize, j: usize, t: u8) -> Tr {
    let start = if at(z, i) == b'+' { add(i, 1) } else { i };
    Tr::Val(Node::Num(num_type(t), span(z, start, j).to_vec()), j)
}

/// The `parse_number:` label of `jsonTranslateTextToBlob`: sign, leading
/// zero, hexadecimal and `Infinity` handling.
fn parse_number(z: &[u8], p: &mut Parse, i: usize, mut t: u8) -> Tr {
    let c = at(z, i);
    if c <= b'0' {
        if c == b'0' {
            if matches!(at(z, add(i, 1)), b'x' | b'X') && is_xdigit(at(z, add(i, 2))) {
                p.nonstd = true;
                let mut j = add(i, 3);
                while is_xdigit(at(z, j)) {
                    j = add(j, 1);
                }
                return number_node(z, i, j, 1);
            } else if is_digit(at(z, add(i, 1))) {
                p.err = add(i, 1);
                return Tr::Error;
            }
        } else {
            if !is_digit(at(z, add(i, 1))) {
                if matches!(at(z, add(i, 1)), b'I' | b'i') && starts_nocase(z, add(i, 1), b"inf") {
                    p.nonstd = true;
                    let text: &[u8] = if c == b'-' { b"-9e999" } else { b"9e999" };
                    let len = if starts_nocase(z, add(i, 4), b"inity") {
                        9
                    } else {
                        4
                    };
                    return Tr::Val(Node::Num(J_FLOAT, text.to_vec()), add(i, len));
                }
                if at(z, add(i, 1)) == b'.' {
                    p.nonstd = true;
                    t |= 0x01;
                    return parse_number_tail(z, p, i, t);
                }
                p.err = i;
                return Tr::Error;
            }
            if at(z, add(i, 1)) == b'0' {
                if is_digit(at(z, add(i, 2))) {
                    p.err = add(i, 1);
                    return Tr::Error;
                } else if matches!(at(z, add(i, 2)), b'x' | b'X') && is_xdigit(at(z, add(i, 3))) {
                    p.nonstd = true;
                    t |= 0x01;
                    let mut j = add(i, 4);
                    while is_xdigit(at(z, j)) {
                        j = add(j, 1);
                    }
                    return number_node(z, i, j, t);
                }
            }
        }
    }
    parse_number_tail(z, p, i, t)
}

/// Whether `z[j-1]` is a `.` preceded by a digit at or after `i` -- the
/// JSON5 "trailing dot" case of the number parser.
fn trailing_dot(z: &[u8], i: usize, j: usize) -> bool {
    at(z, j.saturating_sub(1)) == b'.' && j >= add(i, 2) && is_digit(at(z, j.saturating_sub(2)))
}

/// The `parse_number_2:` label: digits, one `.`, one exponent.
fn parse_number_tail(z: &[u8], p: &mut Parse, i: usize, mut t: u8) -> Tr {
    let mut seen_e = false;
    let mut j = add(i, 1);
    loop {
        let c = at(z, j);
        if is_digit(c) {
            j = add(j, 1);
            continue;
        }
        if c == b'.' {
            if t & 0x02 != 0 {
                p.err = j;
                return Tr::Error;
            }
            t |= 0x02;
            j = add(j, 1);
            continue;
        }
        if c == b'e' || c == b'E' {
            if at(z, j.saturating_sub(1)) < b'0' {
                if trailing_dot(z, i, j) {
                    p.nonstd = true;
                    t |= 0x01;
                } else {
                    p.err = j;
                    return Tr::Error;
                }
            }
            if seen_e {
                p.err = j;
                return Tr::Error;
            }
            t |= 0x02;
            seen_e = true;
            let mut c = at(z, add(j, 1));
            if c == b'+' || c == b'-' {
                j = add(j, 1);
                c = at(z, add(j, 1));
            }
            if !is_digit(c) {
                p.err = j;
                return Tr::Error;
            }
            j = add(j, 1);
            continue;
        }
        break;
    }
    if at(z, j.saturating_sub(1)) < b'0' {
        if trailing_dot(z, i, j) {
            p.nonstd = true;
            t |= 0x01;
        } else {
            p.err = j;
            return Tr::Error;
        }
    }
    number_node(z, i, j, t)
}

/// A `"..."` or `'...'` string literal starting at `i`.
fn parse_string(z: &[u8], p: &mut Parse, i: usize) -> Tr {
    let delim = at(z, i);
    let mut kind = J_TEXT;
    let mut j = add(i, 1);
    loop {
        let mut c = at(z, j);
        if is_ok(c) {
            j = add(j, 1);
            continue;
        }
        if c == delim {
            break;
        } else if c == b'\\' {
            j = add(j, 1);
            c = at(z, j);
            if matches!(c, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't')
                || (c == b'u' && is_4hex(z, add(j, 1)))
            {
                if kind == J_TEXT {
                    kind = J_TEXTJ;
                }
            } else if c == b'\''
                || c == b'v'
                || c == b'\n'
                || (c == b'0' && !is_digit(at(z, add(j, 1))))
                || (c == 0xe2
                    && at(z, add(j, 1)) == 0x80
                    && matches!(at(z, add(j, 2)), 0xa8 | 0xa9))
                || (c == b'x' && is_2hex(z, add(j, 1)))
            {
                kind = J_TEXT5;
                p.nonstd = true;
            } else if c == b'\r' {
                if at(z, add(j, 1)) == b'\n' {
                    j = add(j, 1);
                }
                kind = J_TEXT5;
                p.nonstd = true;
            } else {
                p.err = j;
                return Tr::Error;
            }
        } else if c <= 0x1f {
            if c == 0 {
                p.err = j;
                return Tr::Error;
            }
            kind = J_TEXT5;
            p.nonstd = true;
        } else if c == b'"' {
            kind = J_TEXT5;
        }
        j = add(j, 1);
    }
    let raw = span(z, add(i, 1), j).to_vec();
    Tr::Val(Node::Str(JStr { kind, raw }), add(j, 1))
}

/// An array starting at `i` (the `case '['` arm).
fn parse_array(z: &[u8], p: &mut Parse, i: usize) -> Tr {
    p.depth = p.depth.saturating_add(1);
    if p.depth > MAX_DEPTH {
        p.err = i;
        return Tr::Error;
    }
    let mut items = Vec::new();
    let mut j = add(i, 1);
    loop {
        match translate(z, p, j) {
            Tr::Val(node, x) => {
                items.push(node);
                j = x;
            }
            Tr::CloseBracket => {
                j = p.err;
                if !items.is_empty() {
                    p.nonstd = true;
                }
                break;
            }
            Tr::Error => return Tr::Error,
            _ => {
                p.err = j;
                return Tr::Error;
            }
        }
        let c = at(z, j);
        if c == b',' {
            j = add(j, 1);
            continue;
        } else if c == b']' {
            break;
        }
        if is_space(c) {
            j = add(j, add(1, spaces(z, add(j, 1))));
            if at(z, j) == b',' {
                j = add(j, 1);
                continue;
            } else if at(z, j) == b']' {
                break;
            }
        }
        match translate(z, p, j) {
            Tr::Comma => {
                j = add(p.err, 1);
                continue;
            }
            Tr::CloseBracket => {
                j = p.err;
                break;
            }
            _ => {}
        }
        p.err = j;
        return Tr::Error;
    }
    p.depth = p.depth.saturating_sub(1);
    Tr::Val(Node::Array(items), add(j, 1))
}

/// A JSON5 unquoted object label starting at `j` (after whitespace):
/// `Some((label, end))` if one is there.
fn parse_ident_label(z: &[u8], j: usize) -> Option<(JStr, usize)> {
    let mut kind = J_TEXT;
    let is_esc = |k: usize| at(z, k) == b'\\' && at(z, add(k, 1)) == b'u' && is_4hex(z, add(k, 2));
    if !is_id1(at(z, j)) && !is_esc(j) {
        return None;
    }
    if is_esc(j) {
        kind = J_TEXTJ;
    }
    let mut k = add(j, 1);
    loop {
        if is_id2(at(z, k)) && json5_ws(z, k) == 0 {
            k = add(k, 1);
        } else if is_esc(k) {
            kind = J_TEXTJ;
            k = add(k, 1);
        } else {
            break;
        }
    }
    Some((
        JStr {
            kind,
            raw: span(z, j, k).to_vec(),
        },
        k,
    ))
}

/// An object starting at `i` (the `case '{'` arm).
fn parse_object(z: &[u8], p: &mut Parse, i: usize) -> Tr {
    p.depth = p.depth.saturating_add(1);
    if p.depth > MAX_DEPTH {
        p.err = i;
        return Tr::Error;
    }
    let mut fields: Vec<(JStr, Node)> = Vec::new();
    let mut j = add(i, 1);
    loop {
        let (label, x) = match translate(z, p, j) {
            Tr::Val(Node::Str(s), x) => (s, x),
            Tr::Val(_, _) => {
                p.err = j;
                return Tr::Error;
            }
            Tr::CloseBrace => {
                j = p.err;
                if !fields.is_empty() {
                    p.nonstd = true;
                }
                break;
            }
            other => {
                let was_error = matches!(other, Tr::Error);
                j = add(j, json5_ws(z, j));
                match parse_ident_label(z, j) {
                    Some((label, k)) => {
                        p.nonstd = true;
                        (label, k)
                    }
                    None => {
                        if !was_error {
                            p.err = j;
                        }
                        return Tr::Error;
                    }
                }
            }
        };
        j = x;
        if at(z, j) == b':' {
            j = add(j, 1);
        } else {
            let mut colon = false;
            if is_space(at(z, j)) {
                while is_space(at(z, j)) {
                    j = add(j, 1);
                }
                if at(z, j) == b':' {
                    j = add(j, 1);
                    colon = true;
                }
            }
            if !colon {
                match translate(z, p, j) {
                    Tr::Colon => j = add(p.err, 1),
                    Tr::Error => return Tr::Error,
                    _ => {
                        p.err = j;
                        return Tr::Error;
                    }
                }
            }
        }
        match translate(z, p, j) {
            Tr::Val(value, x) => {
                fields.push((label, value));
                j = x;
            }
            Tr::Error => return Tr::Error,
            _ => {
                p.err = j;
                return Tr::Error;
            }
        }
        let c = at(z, j);
        if c == b',' {
            j = add(j, 1);
            continue;
        } else if c == b'}' {
            break;
        }
        if is_space(c) {
            j = add(j, add(1, spaces(z, add(j, 1))));
            if at(z, j) == b',' {
                j = add(j, 1);
                continue;
            } else if at(z, j) == b'}' {
                break;
            }
        }
        match translate(z, p, j) {
            Tr::Comma => {
                j = add(p.err, 1);
                continue;
            }
            Tr::CloseBrace => {
                j = p.err;
                break;
            }
            _ => {}
        }
        p.err = j;
        return Tr::Error;
    }
    p.depth = p.depth.saturating_sub(1);
    Tr::Val(Node::Object(fields), add(j, 1))
}

/// `jsonConvertTextToBlob`: the whole of `z` as one JSON value plus
/// whether it used JSON5, or the error offset.
fn parse_text(z: &[u8]) -> Result<(Node, bool), usize> {
    let mut p = Parse {
        err: 0,
        depth: 0,
        nonstd: false,
    };
    match translate(z, &mut p, 0) {
        Tr::Val(node, end) => {
            let mut i = end;
            while is_space(at(z, i)) {
                i = add(i, 1);
            }
            if at(z, i) != 0 {
                i = add(i, json5_ws(z, i));
                if at(z, i) != 0 {
                    return Err(p.err);
                }
                p.nonstd = true;
            }
            Ok((node, p.nonstd))
        }
        _ => Err(p.err),
    }
}

// ---------------------------------------------------------------------------
// JSONB (binary) input
// ---------------------------------------------------------------------------

/// `jsonbPayloadSize(z, i)`: `(header bytes, payload bytes)`, or `None`
/// if the header is truncated or the payload overruns the buffer.
fn payload_size(z: &[u8], i: usize) -> Option<(usize, usize)> {
    let x = at(z, i) >> 4;
    if i >= z.len() {
        return None;
    }
    let byte = |k: usize| u64::from(at(z, add(i, k)));
    let (n, sz): (usize, u64) = match x {
        0..=11 => (1, u64::from(x)),
        12 => {
            if add(i, 1) >= z.len() {
                return None;
            }
            (2, byte(1))
        }
        13 => {
            if add(i, 2) >= z.len() {
                return None;
            }
            (3, (byte(1) << 8) | byte(2))
        }
        14 => {
            if add(i, 4) >= z.len() {
                return None;
            }
            (
                5,
                (byte(1) << 24) | (byte(2) << 16) | (byte(3) << 8) | byte(4),
            )
        }
        _ => {
            if add(i, 8) >= z.len() || byte(1) != 0 || byte(2) != 0 || byte(3) != 0 || byte(4) != 0
            {
                return None;
            }
            (
                9,
                (byte(5) << 24) | (byte(6) << 16) | (byte(7) << 8) | byte(8),
            )
        }
    };
    let sz = usize::try_from(sz).ok()?;
    if add(add(i, sz), n) > z.len() {
        return None;
    }
    Some((n, sz))
}

/// `jsonbValidityCheck(z, i, end, depth)`: 0 if the element at `i` (which
/// must end at `end`) is well-formed JSONB, else a 1-based byte offset of
/// the problem.
fn validity_check(z: &[u8], i: usize, end: usize, depth: u32) -> usize {
    let fail = add(i, 1);
    if depth > MAX_DEPTH {
        return fail;
    }
    let Some((n, sz)) = payload_size(z, i) else {
        return fail;
    };
    if add(add(i, n), sz) != end {
        return fail;
    }
    let x = at(z, i) & 0x0f;
    let start = add(i, n);
    let k = add(start, sz);
    match x {
        J_NULL | J_TRUE | J_FALSE => {
            if add(n, sz) == 1 {
                0
            } else {
                fail
            }
        }
        J_INT => {
            if sz < 1 {
                return fail;
            }
            let mut j = start;
            if at(z, j) == b'-' {
                j = add(j, 1);
                if sz < 2 {
                    return fail;
                }
            }
            while j < k {
                if !is_digit(at(z, j)) {
                    return add(j, 1);
                }
                j = add(j, 1);
            }
            0
        }
        J_INT5 => {
            if sz < 3 {
                return fail;
            }
            let mut j = start;
            if at(z, j) == b'-' {
                if sz < 4 {
                    return fail;
                }
                j = add(j, 1);
            }
            if at(z, j) != b'0' {
                return fail;
            }
            if !matches!(at(z, add(j, 1)), b'x' | b'X') {
                return add(j, 2);
            }
            j = add(j, 2);
            while j < k {
                if !is_xdigit(at(z, j)) {
                    return add(j, 1);
                }
                j = add(j, 1);
            }
            0
        }
        J_FLOAT | J_FLOAT5 => validity_check_float(z, i, x, start, k, sz),
        J_TEXT => {
            let mut j = start;
            while j < k {
                if !is_ok(at(z, j)) && at(z, j) != b'\'' {
                    return add(j, 1);
                }
                j = add(j, 1);
            }
            0
        }
        J_TEXTJ | J_TEXT5 => validity_check_text(z, x, start, k),
        J_TEXTRAW => 0,
        J_ARRAY | J_OBJECT => {
            let mut j = start;
            let mut cnt: usize = 0;
            while j < k {
                let Some((n2, sz2)) = payload_size(z, j) else {
                    return add(j, 1);
                };
                let next = add(add(j, n2), sz2);
                if next > k {
                    return add(j, 1);
                }
                if x == J_OBJECT && cnt & 1 == 0 {
                    let t = at(z, j) & 0x0f;
                    if !(J_TEXT..=J_TEXTRAW).contains(&t) {
                        return add(j, 1);
                    }
                }
                let sub = validity_check(z, j, next, depth.saturating_add(1));
                if sub != 0 {
                    return sub;
                }
                cnt = add(cnt, 1);
                j = next;
            }
            if x == J_OBJECT && cnt & 1 != 0 {
                return add(j, 1);
            }
            0
        }
        _ => fail,
    }
}

/// The `JSONB_FLOAT`/`JSONB_FLOAT5` arm of [`validity_check`].
fn validity_check_float(z: &[u8], i: usize, x: u8, start: usize, k: usize, sz: usize) -> usize {
    let fail = add(i, 1);
    // 0: initial, 1: '.' seen, 2: 'e' seen
    let mut seen = 0u8;
    if sz < 2 {
        return fail;
    }
    let mut j = start;
    if at(z, j) == b'-' {
        j = add(j, 1);
        if sz < 3 {
            return fail;
        }
    }
    if at(z, j) == b'.' {
        if x == J_FLOAT || !is_digit(at(z, add(j, 1))) {
            return add(j, 1);
        }
        j = add(j, 2);
        seen = 1;
    } else if at(z, j) == b'0' && x == J_FLOAT {
        if add(j, 3) > k {
            return add(j, 1);
        }
        if !matches!(at(z, add(j, 1)), b'.' | b'e' | b'E') {
            return add(j, 1);
        }
        j = add(j, 1);
    }
    while j < k {
        let c = at(z, j);
        if is_digit(c) {
            j = add(j, 1);
            continue;
        }
        if c == b'.' {
            if seen > 0 {
                return add(j, 1);
            }
            if x == J_FLOAT && (add(j, 1) == k || !is_digit(at(z, add(j, 1)))) {
                return add(j, 1);
            }
            seen = 1;
            j = add(j, 1);
            continue;
        }
        if c == b'e' || c == b'E' {
            if seen == 2 || add(j, 1) == k {
                return add(j, 1);
            }
            if matches!(at(z, add(j, 1)), b'+' | b'-') {
                j = add(j, 1);
                if add(j, 1) == k {
                    return add(j, 1);
                }
            }
            seen = 2;
            j = add(j, 1);
            continue;
        }
        return add(j, 1);
    }
    if seen == 0 {
        return fail;
    }
    0
}

/// The `JSONB_TEXTJ`/`JSONB_TEXT5` arm of [`validity_check`].
fn validity_check_text(z: &[u8], x: u8, start: usize, k: usize) -> usize {
    let mut j = start;
    while j < k {
        let c = at(z, j);
        if !is_ok(c) && c != b'\'' {
            if c == b'"' || c <= 0x1f {
                // Raw `"` and control characters are JSON5-only.
                if x == J_TEXTJ {
                    return add(j, 1);
                }
            } else if add(j, 1) >= k {
                return add(j, 1);
            } else if b"\"\\/bfnrt".contains(&at(z, add(j, 1))) {
                j = add(j, 1);
            } else if at(z, add(j, 1)) == b'u' {
                if add(j, 5) >= k || !is_4hex(z, add(j, 2)) {
                    return add(j, 1);
                }
                j = add(j, 1);
            } else if x != J_TEXT5 {
                return add(j, 1);
            } else {
                let (ch, len) = unescape_one(span(z, j, k));
                if ch == INVALID_CHAR {
                    return add(j, 1);
                }
                j = add(j, len.saturating_sub(1));
            }
        }
        j = add(j, 1);
    }
    0
}

/// `jsonArgIsJsonb`: whether a BLOB argument is taken as JSONB rather than
/// as JSON text.
fn is_jsonb(z: &[u8]) -> bool {
    let Some(&c) = z.first() else {
        return false;
    };
    if c & 0x0f > J_OBJECT {
        return false;
    }
    let Some((n, sz)) = payload_size(z, 0) else {
        return false;
    };
    add(n, sz) == z.len()
        && (c & 0x0f > J_FALSE || sz == 0)
        && (sz > 7
            || (c != 0x7b && c != 0x5b && !is_digit(c))
            || validity_check(z, 0, z.len(), 1) == 0)
}

/// Decode the (validity-checked) JSONB element at `i` into a [`Node`],
/// with the offset after it.
fn decode_jsonb(z: &[u8], i: usize, depth: u32) -> Option<(Node, usize)> {
    if depth > MAX_DEPTH {
        return None;
    }
    let (n, sz) = payload_size(z, i)?;
    let start = add(i, n);
    let end = add(start, sz);
    let payload = span(z, start, end);
    let node = match at(z, i) & 0x0f {
        J_NULL => Node::Null,
        J_TRUE => Node::True,
        J_FALSE => Node::False,
        t @ J_INT..=J_FLOAT5 => {
            if payload.is_empty() || (t == J_INT5 && !int5_renders(payload)) {
                return None;
            }
            Node::Num(t, payload.to_vec())
        }
        J_TEXT5 if !text5_renders(payload) => return None,
        t @ J_TEXT..=J_TEXTRAW => Node::Str(JStr {
            kind: t,
            raw: payload.to_vec(),
        }),
        J_ARRAY => {
            let mut items = Vec::new();
            let mut j = start;
            while j < end {
                let (item, next) = decode_jsonb(z, j, depth.saturating_add(1))?;
                items.push(item);
                j = next;
            }
            if j != end {
                return None;
            }
            Node::Array(items)
        }
        J_OBJECT => {
            let mut fields = Vec::new();
            let mut j = start;
            while j < end {
                let (label, next) = decode_jsonb(z, j, depth.saturating_add(1))?;
                let (value, after) = decode_jsonb(z, next, depth.saturating_add(1))?;
                let Node::Str(label) = label else {
                    return None;
                };
                fields.push((label, value));
                j = after;
            }
            if j != end {
                return None;
            }
            Node::Object(fields)
        }
        _ => return None,
    };
    Some((node, end))
}

/// Whether `jsonTranslateBlobToText` renders a `JSONB_INT5` payload
/// without flagging it malformed: hex digits after the `0x`.
fn int5_renders(z: &[u8]) -> bool {
    let k = if matches!(at(z, 0), b'-' | b'+') {
        3
    } else {
        2
    };
    z.get(k..).unwrap_or(&[]).iter().all(|c| is_xdigit(*c))
}

/// Whether `jsonTranslateBlobToText` renders a `JSONB_TEXT5` payload
/// without flagging it malformed: every `\` starts a complete escape.
fn text5_renders(z: &[u8]) -> bool {
    let mut k = 0usize;
    while k < z.len() {
        if at(z, k) != b'\\' {
            k = add(k, 1);
            continue;
        }
        let rest = z.len().saturating_sub(k);
        let complete = match at(z, add(k, 1)) {
            _ if rest < 2 => false,
            b'x' => rest >= 4,
            0xe2 => {
                rest >= 4 && at(z, add(k, 2)) == 0x80 && matches!(at(z, add(k, 3)), 0xa8 | 0xa9)
            }
            _ => true,
        };
        if !complete {
            return false;
        }
        k = add(k, 2);
    }
    true
}

/// A JSONB BLOB (one [`is_jsonb`] accepted) as a [`Node`]; `malformed
/// JSON` where sqlite3's renderer would flag it (a truncated element, an
/// empty number, an object with an odd element count or a non-text
/// label, ...). Contents are otherwise taken as they are, as sqlite3
/// does for a BLOB whose header passes `jsonArgIsJsonb`.
fn jsonb_to_node(z: &[u8]) -> Result<Node, FunctionError> {
    decode_jsonb(z, 0, 0)
        .map(|(node, _)| node)
        .ok_or_else(malformed)
}

// ---------------------------------------------------------------------------
// Tree -> text (jsonTranslateBlobToText)
// ---------------------------------------------------------------------------

/// `jsonAppendControlChar`.
fn push_control(out: &mut Vec<u8>, c: u8) {
    let special = match c {
        0x08 => Some(b'b'),
        0x09 => Some(b't'),
        0x0a => Some(b'n'),
        0x0c => Some(b'f'),
        0x0d => Some(b'r'),
        _ => None,
    };
    match special {
        Some(s) => out.extend_from_slice(&[b'\\', s]),
        None => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            out.extend_from_slice(b"\\u00");
            out.push(at(HEX, usize::from(c >> 4)));
            out.push(at(HEX, usize::from(c & 0xf)));
        }
    }
}

/// `jsonAppendString`: `z` as a quoted JSON string, escaping `"`, `\`
/// and control characters.
fn push_string(out: &mut Vec<u8>, z: &[u8]) {
    out.push(b'"');
    for &c in z {
        if is_ok(c) || c == b'\'' {
            out.push(c);
        } else if c == b'"' || c == b'\\' {
            out.extend_from_slice(&[b'\\', c]);
        } else {
            push_control(out, c);
        }
    }
    out.push(b'"');
}

/// The `JSONB_INT5` arm: a hexadecimal literal in decimal.
fn render_int5(out: &mut Vec<u8>, z: &[u8]) {
    let mut k = 2usize;
    if at(z, 0) == b'-' {
        out.push(b'-');
        k = 3;
    } else if at(z, 0) == b'+' {
        k = 3;
    }
    let mut u: u64 = 0;
    let mut overflow = false;
    for &c in z.get(k..).unwrap_or(&[]) {
        if !is_xdigit(c) {
            break;
        } else if u >> 60 != 0 {
            overflow = true;
        } else {
            u = (u << 4) | u64::from(hex_val(c));
        }
    }
    if overflow {
        out.extend_from_slice(b"9.0e999");
    } else {
        out.extend_from_slice(u.to_string().as_bytes());
    }
}

/// The `JSONB_FLOAT5` arm: add the digits missing beside a `.`.
fn render_float5(out: &mut Vec<u8>, z: &[u8]) {
    let mut k = 0usize;
    if at(z, 0) == b'-' {
        out.push(b'-');
        k = 1;
    }
    if at(z, k) == b'.' {
        out.push(b'0');
    }
    while k < z.len() {
        let c = at(z, k);
        out.push(c);
        if c == b'.' && (add(k, 1) == z.len() || !is_digit(at(z, add(k, 1)))) {
            out.push(b'0');
        }
        k = add(k, 1);
    }
}

/// The `JSONB_TEXT5` arm: rewrite JSON5 escapes and raw control
/// characters as canonical JSON.
fn render_text5(out: &mut Vec<u8>, z: &[u8]) {
    out.push(b'"');
    let mut k = 0usize;
    while k < z.len() {
        let c = at(z, k);
        if is_ok(c) || c == b'\'' {
            out.push(c);
            k = add(k, 1);
            continue;
        }
        if c == b'"' {
            out.extend_from_slice(b"\\\"");
            k = add(k, 1);
            continue;
        }
        if c <= 0x1f {
            push_control(out, c);
            k = add(k, 1);
            continue;
        }
        // c == '\\'
        let rest = z.len().saturating_sub(k);
        if rest < 2 {
            break;
        }
        let e = at(z, add(k, 1));
        match e {
            b'\'' => out.push(b'\''),
            b'v' => out.extend_from_slice(b"\\u000b"),
            b'x' => {
                if rest < 4 {
                    break;
                }
                out.extend_from_slice(b"\\u00");
                out.extend_from_slice(span(z, add(k, 2), add(k, 4)));
                k = add(k, 2);
            }
            b'0' => out.extend_from_slice(b"\\u0000"),
            b'\r' => {
                if rest > 2 && at(z, add(k, 2)) == b'\n' {
                    k = add(k, 1);
                }
            }
            b'\n' => {}
            0xe2 => {
                if rest < 4 || at(z, add(k, 2)) != 0x80 || !matches!(at(z, add(k, 3)), 0xa8 | 0xa9)
                {
                    break;
                }
                k = add(k, 2);
            }
            _ => out.extend_from_slice(span(z, k, add(k, 2))),
        }
        k = add(k, 2);
    }
    out.push(b'"');
}

/// Render a string node.
fn render_str(out: &mut Vec<u8>, s: &JStr) {
    match s.kind {
        J_TEXT | J_TEXTJ => {
            out.push(b'"');
            out.extend_from_slice(&s.raw);
            out.push(b'"');
        }
        J_TEXT5 => render_text5(out, &s.raw),
        _ => push_string(out, &s.raw),
    }
}

/// `jsonTranslateBlobToText`: append `node` as minified JSON text.
fn render(out: &mut Vec<u8>, node: &Node) {
    match node {
        Node::Null => out.extend_from_slice(b"null"),
        Node::True => out.extend_from_slice(b"true"),
        Node::False => out.extend_from_slice(b"false"),
        Node::Num(J_INT5, z) => render_int5(out, z),
        Node::Num(J_FLOAT5, z) => render_float5(out, z),
        Node::Num(_, z) => out.extend_from_slice(z),
        Node::Str(s) => render_str(out, s),
        Node::Array(items) => {
            out.push(b'[');
            for (k, item) in items.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                render(out, item);
            }
            out.push(b']');
        }
        Node::Object(fields) => {
            out.push(b'{');
            for (k, (label, value)) in fields.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                render_str(out, label);
                out.push(b':');
                render(out, value);
            }
            out.push(b'}');
        }
    }
}

// ---------------------------------------------------------------------------
// Label comparison (jsonLabelCompare)
// ---------------------------------------------------------------------------

/// sqlite3's `JSON_INVALID_CHAR`.
const INVALID_CHAR: u32 = 0x99999;

/// `sqlite3Utf8Trans1`: payload bits of a UTF-8 lead byte (`0xc0..`).
const UTF8_TRANS1: [u8; 64] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00, 0x01, 0x02, 0x03, 0x00, 0x01, 0x00, 0x00,
];

/// `sqlite3Utf8ReadLimited`: one character from non-empty `z`, with its
/// byte length.
fn utf8_read(z: &[u8]) -> (u32, usize) {
    let c0 = at(z, 0);
    if c0 < 0xc0 {
        return (u32::from(c0), 1);
    }
    let mut c = u32::from(at(&UTF8_TRANS1, usize::from(c0.wrapping_sub(0xc0))));
    let n = z.len().min(4);
    let mut i = 1usize;
    while i < n && at(z, i) & 0xc0 == 0x80 {
        c = (c << 6).wrapping_add(u32::from(0x3f & at(z, i)));
        i = add(i, 1);
    }
    (c, i)
}

/// `jsonBytesToBypass`: the length of escaped newlines at the start of
/// `z`.
fn bytes_to_bypass(z: &[u8]) -> usize {
    let n = z.len();
    let mut i = 0usize;
    while add(i, 1) < n {
        if at(z, i) != b'\\' {
            return i;
        }
        let c = at(z, add(i, 1));
        if c == b'\n' {
            i = add(i, 2);
            continue;
        }
        if c == b'\r' {
            i = if add(i, 2) < n && at(z, add(i, 2)) == b'\n' {
                add(i, 3)
            } else {
                add(i, 2)
            };
            continue;
        }
        if c == 0xe2
            && add(i, 3) < n
            && at(z, add(i, 2)) == 0x80
            && matches!(at(z, add(i, 3)), 0xa8 | 0xa9)
        {
            i = add(i, 4);
            continue;
        }
        break;
    }
    i
}

/// `jsonUnescapeOneChar`: decode the escape at the start of `z` (which
/// begins with `\`): the character and the bytes consumed.
fn unescape_one(z: &[u8]) -> (u32, usize) {
    let n = z.len();
    if n < 2 {
        return (INVALID_CHAR, n);
    }
    match at(z, 1) {
        b'u' => {
            if n < 6 {
                return (INVALID_CHAR, n);
            }
            let v = hex4(z, 2);
            if v & 0xfc00 == 0xd800 && n >= 12 && at(z, 6) == b'\\' && at(z, 7) == b'u' {
                let lo = hex4(z, 8);
                if lo & 0xfc00 == 0xdc00 {
                    return (
                        (((v & 0x3ff) << 10) | (lo & 0x3ff)).wrapping_add(0x10000),
                        12,
                    );
                }
            }
            (v, 6)
        }
        b'b' => (0x08, 2),
        b'f' => (0x0c, 2),
        b'n' => (0x0a, 2),
        b'r' => (0x0d, 2),
        b't' => (0x09, 2),
        b'v' => (0x0b, 2),
        b'0' => (
            if n > 2 && is_digit(at(z, 2)) {
                INVALID_CHAR
            } else {
                0
            },
            2,
        ),
        c @ (b'\'' | b'"' | b'/' | b'\\') => (u32::from(c), 2),
        b'x' => {
            if n < 4 {
                return (INVALID_CHAR, n);
            }
            ((hex_val(at(z, 2)) << 4) | hex_val(at(z, 3)), 4)
        }
        0xe2 | b'\r' | b'\n' => {
            let skip = bytes_to_bypass(z);
            if skip == 0 {
                (INVALID_CHAR, n)
            } else if skip == n {
                (0, n)
            } else {
                let rest = z.get(skip..).unwrap_or(&[]);
                let (c, len) = if at(rest, 0) == b'\\' {
                    unescape_one(rest)
                } else {
                    utf8_read(rest)
                };
                (c, add(skip, len))
            }
        }
        _ => (INVALID_CHAR, 2),
    }
}

/// The next character of a label at `*pos`, advancing `*pos`; 0 at the
/// end. `raw` labels have no escapes.
fn label_char(z: &[u8], pos: &mut usize, raw: bool) -> u32 {
    let rest = z.get(*pos..).unwrap_or(&[]);
    if rest.is_empty() {
        return 0;
    }
    let (c, len) = if raw || at(rest, 0) != b'\\' {
        utf8_read(rest)
    } else {
        unescape_one(rest)
    };
    *pos = add(*pos, len.max(1));
    c
}

/// `jsonLabelCompare`: whether two object labels are equal once escapes
/// are decoded.
fn label_eq(left: &[u8], left_raw: bool, right: &[u8], right_raw: bool) -> bool {
    if left_raw && right_raw {
        return left == right;
    }
    let (mut l, mut r) = (0usize, 0usize);
    loop {
        let cl = label_char(left, &mut l, left_raw);
        let cr = label_char(right, &mut r, right_raw);
        if cl != cr {
            return false;
        }
        if cl == 0 {
            return true;
        }
    }
}

// ---------------------------------------------------------------------------
// Paths (jsonLookupStep)
// ---------------------------------------------------------------------------

/// Why a path step failed (`JSON_LOOKUP_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lk {
    /// `JSON_LOOKUP_NOTFOUND`.
    NotFound,
    /// `JSON_LOOKUP_PATHERROR`.
    PathError,
    /// `JSON_LOOKUP_TOODEEP`.
    TooDeep,
}

/// The edit a lookup performs (`pParse->eEdit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    /// Read only.
    Lookup,
    /// `JEDIT_DEL` (`json_remove`).
    Delete,
    /// `JEDIT_REPL` (`json_replace`).
    Replace,
    /// `JEDIT_INS` (`json_insert`).
    Insert,
    /// `JEDIT_SET` (`json_set`).
    Set,
}

impl Edit {
    /// Whether the edit creates missing elements (`eEdit>=JEDIT_INS`).
    fn creates(self) -> bool {
        matches!(self, Edit::Insert | Edit::Set)
    }
}

/// What a successful lookup found: its type and, for an array, its
/// length.
#[derive(Debug, Clone, Copy)]
struct Found {
    /// JSONB type of the element.
    jtype: u8,
    /// Element count if it is an array, else 0.
    len: usize,
}

/// The [`Found`] for `node`.
fn found(node: &Node) -> Found {
    Found {
        jtype: node.jtype(),
        len: node.array_len(),
    }
}

/// A lookup/edit: the path, where in it, the edit and the value to
/// insert.
struct Walk {
    /// The path text (after the leading `$`).
    path: Vec<u8>,
    /// The edit to perform.
    edit: Edit,
    /// The value `json_set`/`json_insert`/`json_replace` writes.
    value: Node,
}

/// `jsonLookupStep(node, path[pos..])`, editing in place per `walk.edit`.
fn step(node: &mut Node, walk: &Walk, pos: usize, depth: u32) -> Result<Found, Lk> {
    let path = &walk.path;
    match at(path, pos) {
        0 => {
            if matches!(walk.edit, Edit::Replace | Edit::Set) {
                *node = walk.value.clone();
            }
            Ok(found(node))
        }
        b'.' => step_key(node, walk, pos, depth),
        b'[' => step_index(node, walk, pos, depth),
        _ => Err(Lk::PathError),
    }
}

/// The `.key` / `."key"` arm of [`step`].
fn step_key(node: &mut Node, walk: &Walk, pos: usize, depth: u32) -> Result<Found, Lk> {
    let path = &walk.path;
    let i = add(pos, 1);
    let (key, raw_key, next) = if at(path, i) == b'"' {
        let mut k = add(i, 1);
        while at(path, k) != 0 && at(path, k) != b'"' {
            if at(path, k) == b'\\' && at(path, add(k, 1)) != 0 {
                k = add(k, 1);
            }
            k = add(k, 1);
        }
        if at(path, k) == 0 {
            return Err(Lk::PathError);
        }
        let key = span(path, add(i, 1), k);
        (key, !key.contains(&b'\\'), add(k, 1))
    } else {
        let mut k = i;
        while !matches!(at(path, k), 0 | b'.' | b'[') {
            k = add(k, 1);
        }
        if k == i {
            return Err(Lk::PathError);
        }
        (span(path, i, k), true, k)
    };
    let Node::Object(fields) = node else {
        return Err(Lk::NotFound);
    };
    let hit = fields
        .iter()
        .position(|(label, _)| label_eq(key, raw_key, &label.raw, label.is_raw()));
    if let Some(idx) = hit {
        let depth = depth.saturating_add(1);
        if depth >= MAX_DEPTH {
            return Err(Lk::TooDeep);
        }
        if at(path, next) == 0 && walk.edit == Edit::Delete {
            let removed = fields.remove(idx);
            return Ok(found(&removed.1));
        }
        let Some((_, child)) = fields.get_mut(idx) else {
            return Err(Lk::NotFound);
        };
        return step(child, walk, next, depth);
    }
    if !walk.edit.creates() {
        return Err(Lk::NotFound);
    }
    let sub = substructure(walk, next, depth)?;
    let label = JStr {
        kind: if raw_key { J_TEXTRAW } else { J_TEXT5 },
        raw: key.to_vec(),
    };
    let result = found(&sub);
    fields.push((label, sub));
    Ok(result)
}

/// Parse decimal digits at `*i` as sqlite3 does for array indexes:
/// saturating once past `u32::MAX` so huge indexes are NOTFOUND.
fn index_digits(path: &[u8], i: &mut usize) -> u64 {
    let mut v: u64 = 0;
    while is_digit(at(path, *i)) {
        if v < 0xffff_ffff {
            v = v
                .wrapping_mul(10)
                .wrapping_add(u64::from(at(path, *i).wrapping_sub(b'0')));
        }
        *i = add(*i, 1);
    }
    v
}

/// The `[N]` / `[#]` / `[#-N]` arm of [`step`].
fn step_index(node: &mut Node, walk: &Walk, pos: usize, depth: u32) -> Result<Found, Lk> {
    let path = &walk.path;
    let Node::Array(items) = node else {
        return Err(Lk::NotFound);
    };
    let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
    let mut i = add(pos, 1);
    let mut kk = index_digits(path, &mut i);
    if i < add(pos, 2) || at(path, i) != b']' {
        if at(path, add(pos, 1)) != b'#' {
            return Err(Lk::PathError);
        }
        kk = count;
        i = add(pos, 2);
        if at(path, add(pos, 2)) == b'-' && is_digit(at(path, add(pos, 3))) {
            i = add(pos, 3);
            let nn = index_digits(path, &mut i);
            if nn > kk {
                return Err(Lk::NotFound);
            }
            kk = kk.saturating_sub(nn);
        }
        if at(path, i) != b']' {
            return Err(Lk::PathError);
        }
    }
    let next = add(i, 1);
    if kk < count {
        let depth = depth.saturating_add(1);
        if depth >= MAX_DEPTH {
            return Err(Lk::TooDeep);
        }
        let idx = usize::try_from(kk).map_err(|_| Lk::NotFound)?;
        if at(path, next) == 0 && walk.edit == Edit::Delete {
            let removed = items.remove(idx);
            return Ok(found(&removed));
        }
        let Some(child) = items.get_mut(idx) else {
            return Err(Lk::NotFound);
        };
        return step(child, walk, next, depth);
    }
    if kk > count || !walk.edit.creates() {
        return Err(Lk::NotFound);
    }
    let sub = substructure(walk, next, depth)?;
    let result = found(&sub);
    items.push(sub);
    Ok(result)
}

/// `jsonCreateEditSubstructure`: what to insert for the rest of the path
/// at `pos` -- the value itself, or empty objects/arrays leading to it.
fn substructure(walk: &Walk, pos: usize, depth: u32) -> Result<Node, Lk> {
    match at(&walk.path, pos) {
        0 => Ok(walk.value.clone()),
        c => {
            let depth = depth.saturating_add(1);
            if depth >= MAX_DEPTH {
                return Err(Lk::TooDeep);
            }
            let mut sub = if c == b'.' {
                Node::Object(Vec::new())
            } else {
                Node::Array(Vec::new())
            };
            step(&mut sub, walk, pos, depth)?;
            Ok(sub)
        }
    }
}

// ---------------------------------------------------------------------------
// RFC 7396 merge patch (jsonMergePatch)
// ---------------------------------------------------------------------------

/// `jsonMergePatch`: apply `patch` to `target`; `Err` if nested too
/// deep.
fn merge_patch(target: &mut Node, patch: &Node, depth: u32) -> Result<(), ()> {
    let Node::Object(patch_fields) = patch else {
        *target = patch.clone();
        return Ok(());
    };
    if !matches!(target, Node::Object(_)) {
        *target = Node::Object(Vec::new());
    }
    let Node::Object(fields) = target else {
        return Ok(());
    };
    for (plabel, pvalue) in patch_fields {
        let hit = fields.iter().position(|(label, _)| {
            label_eq(&plabel.raw, plabel.is_raw(), &label.raw, label.is_raw())
        });
        match hit {
            Some(idx) => {
                if *pvalue == Node::Null {
                    fields.remove(idx);
                } else {
                    if depth >= MAX_DEPTH {
                        return Err(());
                    }
                    if let Some((_, tvalue)) = fields.get_mut(idx) {
                        merge_patch(tvalue, pvalue, depth.saturating_add(1))?;
                    }
                }
            }
            None if *pvalue == Node::Null => {}
            None => {
                if matches!(pvalue, Node::Object(_)) {
                    if depth >= MAX_DEPTH {
                        return Err(());
                    }
                    let mut fresh = Node::Null;
                    merge_patch(&mut fresh, pvalue, depth.saturating_add(1))?;
                    fields.push((plabel.clone(), fresh));
                } else {
                    fields.push((plabel.clone(), pvalue.clone()));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Real formatting: sqlite3's "%!.17g" (sqlite3FpDecode + etGENERIC)
// ---------------------------------------------------------------------------

/// `aBase[]` of sqlite3's `powerOfTen`: 10^p, p = 0..26, normalised.
const POW10_BASE: [u64; 27] = [
    0x8000000000000000,
    0xa000000000000000,
    0xc800000000000000,
    0xfa00000000000000,
    0x9c40000000000000,
    0xc350000000000000,
    0xf424000000000000,
    0x9896800000000000,
    0xbebc200000000000,
    0xee6b280000000000,
    0x9502f90000000000,
    0xba43b74000000000,
    0xe8d4a51000000000,
    0x9184e72a00000000,
    0xb5e620f480000000,
    0xe35fa931a0000000,
    0x8e1bc9bf04000000,
    0xb1a2bc2ec5000000,
    0xde0b6b3a76400000,
    0x8ac7230489e80000,
    0xad78ebc5ac620000,
    0xd8d726b7177a8000,
    0x878678326eac9000,
    0xa968163f0a57b400,
    0xd3c21bcecceda100,
    0x84595161401484a0,
    0xa56fa5b99019a5c8,
];

/// `aScale[]` of sqlite3's `powerOfTen`: 10^(27k), high 64 bits.
const POW10_SCALE: [u64; 26] = [
    0x8049a4ac0c5811ae,
    0xcf42894a5dce35ea,
    0xa76c582338ed2621,
    0x873e4f75e2224e68,
    0xda7f5bf590966848,
    0xb080392cc4349dec,
    0x8e938662882af53e,
    0xe65829b3046b0afa,
    0xba121a4650e4ddeb,
    0x964e858c91ba2655,
    0xf2d56790ab41c2a2,
    0xc428d05aa4751e4c,
    0x9e74d1b791e07e48,
    0xcccccccccccccccc,
    0xcecb8f27f4200f3a,
    0xa70c3c40a64e6c51,
    0x86f0ac99b4e8dafd,
    0xda01ee641a708de9,
    0xb01ae745b101e9e4,
    0x8e41ade9fbebc27d,
    0xe5d3ef282a242e81,
    0xb9a74a0637ce2ee1,
    0x95f83d0a1fb69cd9,
    0xf24a01a73cf2dccf,
    0xc3b8358109e84f07,
    0x9e19db92b4e31ba9,
];

/// `aScaleLo[]` of sqlite3's `powerOfTen`: the next 32 bits of `aScale[]`.
const POW10_SCALE_LO: [u32; 26] = [
    0x205b896d, 0x52064cad, 0xaf2af2b8, 0x5a7744a7, 0xaf39a475, 0xbd8d794e, 0x547eb47b, 0x0cb4a5a3,
    0x92f34d62, 0x3a6a07f9, 0xfae27299, 0xaa97e14c, 0x775ea265, 0xcccccccc, 0x00000000, 0x999090b6,
    0x69a028bb, 0xe80e6f48, 0x5ec05dd0, 0x14588f14, 0x8f1668c9, 0x6d953e2c, 0x4abdaf10, 0xbc633b39,
    0x0a862f81, 0x6c07a2c2,
];

/// The high 64 bits of a 128-bit product (`sqlite3Multiply128`).
#[allow(
    clippy::cast_possible_truncation,
    reason = "the shifted 128-bit product's high half fits in 64 bits"
)]
fn mul_hi(a: u64, b: u64) -> u64 {
    ((u128::from(a).wrapping_mul(u128::from(b))) >> 64) as u64
}

/// `sqlite3Multiply160`: upper 64 bits and the next 32 bits of
/// `((a<<32)+a_lo) * b`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "each half is masked or shifted to fit its width"
)]
fn mul160(a: u64, a_lo: u32, b: u64) -> (u64, u32) {
    let mut r = u128::from(a).wrapping_mul(u128::from(b));
    r = r.wrapping_add(u128::from(a_lo).wrapping_mul(u128::from(b)) >> 32);
    ((r >> 64) as u64, ((r >> 32) & 0xffff_ffff) as u32)
}

/// `powerOfTen(p)`: the top 64 (and next 32) bits of 10^p.
fn power_of_ten(p: i32) -> (u64, u32) {
    let at64 = |t: &[u64], k: i32| {
        usize::try_from(k)
            .ok()
            .and_then(|k| t.get(k).copied())
            .unwrap_or(0)
    };
    let at32 = |k: i32| {
        usize::try_from(k)
            .ok()
            .and_then(|k| POW10_SCALE_LO.get(k).copied())
            .unwrap_or(0)
    };
    let (g, n);
    if p < 0 {
        if p == -1 {
            return (at64(&POW10_SCALE, 13), at32(13));
        }
        let (mut g0, mut n0) = (p.wrapping_div(27), p.wrapping_rem(27));
        if n0 != 0 {
            g0 = g0.wrapping_sub(1);
            n0 = n0.wrapping_add(27);
        }
        (g, n) = (g0, n0);
    } else if p < 27 {
        return (at64(&POW10_BASE, p), 0);
    } else {
        (g, n) = (p.wrapping_div(27), p.wrapping_rem(27));
    }
    let gi = g.wrapping_add(13);
    let s = at64(&POW10_SCALE, gi);
    if n == 0 {
        return (s, at32(gi));
    }
    let (mut x, mut lo) = mul160(s, at32(gi), at64(&POW10_BASE, n));
    if x & (1u64 << 63) == 0 {
        x = (x << 1) | u64::from((lo >> 31) & 1);
        lo = (lo << 1) | 1;
    }
    (x, lo)
}

/// `pwr10to2(p)`: floor(log2(10^p)).
fn pwr10to2(p: i32) -> i32 {
    p.wrapping_mul(108_853) >> 15
}

/// `pwr2to10(p)`: floor(log10(2^p)).
fn pwr2to10(p: i32) -> i32 {
    p.wrapping_mul(78_913) >> 18
}

/// `sqlite3Fp2Convert10(m, e, 18)`: `m * 2^e` as `d * 10^p`, about 18
/// significant digits.
fn fp2convert10(m: u64, e: i32) -> (u64, i32) {
    let p = 17i32.wrapping_sub(pwr2to10(e.wrapping_add(63)));
    let (pw, _) = power_of_ten(p);
    let h = mul_hi(m, pw);
    let shift = e.wrapping_add(pwr10to2(p)).wrapping_add(2).wrapping_neg();
    let h = u32::try_from(shift)
        .ok()
        .and_then(|s| h.checked_shr(s))
        .unwrap_or(0);
    (h.wrapping_add((h << 1) & 2) >> 1, p.wrapping_neg())
}

/// `sqlite3Fp10Convert2(d, p)`: the double nearest `d * 10^p`.
fn fp10convert2(d: u64, p: i32) -> f64 {
    if p < -348 {
        return 0.0;
    }
    if p > 347 {
        return f64::INFINITY;
    }
    format!("{d}e{p}").parse().unwrap_or(f64::NAN)
}

/// The decimal digits of `digits` as an integer.
fn digits_value(digits: &[u8]) -> u64 {
    digits.iter().fold(0u64, |v, d| {
        v.wrapping_mul(10)
            .wrapping_add(u64::from(d.wrapping_sub(b'0')))
    })
}

/// `sqlite3FpDecode(&s, r, 17, 20)`: sign, significant digits and
/// decimal-point position of a finite non-zero `r`, or which special it
/// is.
enum Decoded {
    /// Infinity (1) or NaN (2) -- `isSpecial`.
    Special(u8),
    /// A finite value: `digits` with the point after `idp` of them.
    Finite {
        /// Significant digits, no trailing zeros.
        digits: Vec<u8>,
        /// `iDP`.
        idp: i32,
    },
}

/// `sqlite3FpDecode` at `%!.17g`'s precision, for `r >= 0`.
fn fp_decode17(r: f64) -> Decoded {
    if r == 0.0 {
        return Decoded::Finite {
            digits: b"0".to_vec(),
            idp: 1,
        };
    }
    let bits = r.to_bits();
    let e0 = (bits >> 52) & 0x7ff;
    if e0 == 0x7ff {
        return Decoded::Special(if bits & 0x7fff_ffff_ffff_ffff == 0x7ff0_0000_0000_0000 {
            1
        } else {
            2
        });
    }
    let mut v = bits & 0x000f_ffff_ffff_ffff;
    let e: i32 = if e0 == 0 {
        let nn = v.leading_zeros();
        v <<= nn;
        (-1074i32).wrapping_sub(i32::try_from(nn).unwrap_or(0))
    } else {
        v = (v << 11) | (1u64 << 63);
        i32::try_from(e0).unwrap_or(0).wrapping_sub(1086)
    };
    let (d, exp) = fp2convert10(v, e);
    let mut z: Vec<u8> = d.to_string().into_bytes();
    let n0 = i32::try_from(z.len()).unwrap_or(0);
    let mut idp = n0.wrapping_add(exp);
    let mut n = z.len();
    if 17 < n {
        let mut round = 17usize;
        if at(&z, 15) == b'9' && at(&z, 14) == b'9' {
            let mut jj = 14usize;
            while jj > 0 && at(&z, jj.saturating_sub(1)) == b'9' {
                jj = jj.saturating_sub(1);
            }
            let v2 = if jj == 0 {
                1
            } else {
                digits_value(span(&z, 0, jj)).wrapping_add(1)
            };
            let p2 = exp
                .wrapping_add(n0)
                .wrapping_sub(i32::try_from(jj).unwrap_or(0));
            if r == fp10convert2(v2, p2) {
                round = add(jj, 1);
            }
        } else if idp >= n0 || (at(&z, 15) == b'0' && at(&z, 14) == b'0' && at(&z, 13) == b'0') {
            let mut jj = 13usize;
            while jj > 1 && at(&z, jj.saturating_sub(1)) == b'0' {
                jj = jj.saturating_sub(1);
            }
            let v2 = digits_value(span(&z, 0, jj));
            let p2 = exp
                .wrapping_add(n0)
                .wrapping_sub(i32::try_from(jj).unwrap_or(0));
            if r == fp10convert2(v2, p2) {
                round = add(jj, 1);
            }
        }
        n = round;
        if at(&z, round) >= b'5' {
            let mut j = round.saturating_sub(1);
            while let Some(dig) = z.get_mut(j) {
                *dig = dig.wrapping_add(1);
                if *dig <= b'9' {
                    break;
                }
                *dig = b'0';
                if j == 0 {
                    z.insert(0, b'1');
                    n = add(n, 1);
                    idp = idp.wrapping_add(1);
                    break;
                }
                j = j.saturating_sub(1);
            }
        }
    }
    z.truncate(n);
    while z.len() > 1 && z.last() == Some(&b'0') {
        z.pop();
    }
    Decoded::Finite { digits: z, idp }
}

/// sqlite3's `printf("%!.17g", r)` (`zeropad` false: how a REAL becomes
/// TEXT) or `"%!0.17g"` (`zeropad` true: how JSON functions print a
/// REAL; infinities become `9.0e+999`, NaN `null`).
fn format_real17(r: f64, zeropad: bool) -> Vec<u8> {
    let neg = r < 0.0;
    let (digits, idp) = match fp_decode17(r.abs()) {
        Decoded::Special(2) => {
            return if zeropad {
                b"null".to_vec()
            } else {
                b"NaN".to_vec()
            }
        }
        Decoded::Special(_) => {
            if zeropad {
                (b"9".to_vec(), 1000)
            } else {
                return if neg {
                    b"-Inf".to_vec()
                } else {
                    b"Inf".to_vec()
                };
            }
        }
        Decoded::Finite { digits, idp } => (digits, idp),
    };
    let mut out = Vec::new();
    if neg {
        out.push(b'-');
    }
    let n = i32::try_from(digits.len()).unwrap_or(0);
    let exp = idp.wrapping_sub(1);
    let exp_form = !(-4..=16).contains(&exp);
    let mut precision: i32 = if exp_form {
        16
    } else {
        16i32.wrapping_sub(exp)
    };
    let mut e2: i32 = if exp_form { 0 } else { exp };
    let mut j: i32 = 0;
    if e2 < 0 {
        out.push(b'0');
    } else {
        j = e2.wrapping_add(1).min(n);
        out.extend_from_slice(span(&digits, 0, usize::try_from(j).unwrap_or(0)));
        e2 = e2.wrapping_sub(j);
        if e2 >= 0 {
            out.resize(
                add(out.len(), usize::try_from(e2.wrapping_add(1)).unwrap_or(0)),
                b'0',
            );
            e2 = -1;
        }
    }
    out.push(b'.');
    if e2 < -1 && precision > 0 {
        let nn = (-1i32).wrapping_sub(e2).min(precision);
        out.resize(add(out.len(), usize::try_from(nn).unwrap_or(0)), b'0');
        precision = precision.wrapping_sub(nn);
    }
    if precision > 0 {
        let nn = n.wrapping_sub(j).min(precision);
        if nn > 0 {
            let from = usize::try_from(j).unwrap_or(0);
            out.extend_from_slice(span(
                &digits,
                from,
                add(from, usize::try_from(nn).unwrap_or(0)),
            ));
        }
    }
    while out.last() == Some(&b'0') {
        out.pop();
    }
    if out.last() == Some(&b'.') {
        out.push(b'0');
    }
    if exp_form {
        out.push(b'e');
        out.push(if exp < 0 { b'-' } else { b'+' });
        let e = exp.unsigned_abs();
        if e >= 100 {
            out.extend_from_slice((e / 100).to_string().as_bytes());
        }
        out.extend_from_slice(format!("{:02}", e % 100).as_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// SQL value plumbing
// ---------------------------------------------------------------------------

/// Most recent JSON results remembered for the subtype approximation.
const MARK_CAPACITY: usize = 64;

/// One remembered JSON result: the identity of its `Rc<str>` allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    /// The thread that produced it.
    thread: std::thread::ThreadId,
    /// The allocation's address.
    addr: usize,
    /// Its length in bytes.
    len: usize,
    /// A hash of its content.
    hash: u64,
}

/// The ring of recent JSON results (see the module docs on subtypes).
static MARKS: Mutex<Vec<Mark>> = Mutex::new(Vec::new());

/// The content hash recorded in a [`Mark`].
fn content_hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(s.as_bytes());
    h.finish()
}

/// The [`Mark`] identifying `rc`, with content hash `hash`.
fn mark_of(rc: &Rc<str>, hash: u64) -> Mark {
    Mark {
        thread: std::thread::current().id(),
        addr: Rc::as_ptr(rc).cast::<u8>().addr(),
        len: rc.len(),
        hash,
    }
}

/// Record `rc` as a JSON-subtyped result.
fn mark_json(rc: &Rc<str>) {
    let mark = mark_of(rc, content_hash(rc));
    let mut marks = MARKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    marks.retain(|m| m.addr != mark.addr || m.thread != mark.thread);
    if marks.len() >= MARK_CAPACITY {
        marks.remove(0);
    }
    marks.push(mark);
}

/// Whether `v` is a JSON function's result (sqlite3's JSON subtype).
fn is_json(v: &Value) -> bool {
    let Value::Text(rc) = v else {
        return false;
    };
    let probe = mark_of(rc, 0);
    let marks = MARKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut candidates = marks
        .iter()
        .filter(|m| m.addr == probe.addr && m.len == probe.len && m.thread == probe.thread)
        .peekable();
    if candidates.peek().is_none() {
        return false;
    }
    let hash = content_hash(rc);
    candidates.any(|m| m.hash == hash)
}

/// A JSON function's text result, marked as JSON.
fn json_result(bytes: Vec<u8>) -> Value {
    let text = String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    let rc: Rc<str> = Rc::from(text);
    mark_json(&rc);
    Value::Text(rc)
}

/// A plain (non-JSON) text result.
fn text_result(s: &str) -> Value {
    Value::Text(Rc::from(s))
}

/// `FunctionError::Sql("malformed JSON")`.
fn malformed() -> FunctionError {
    FunctionError::Sql("malformed JSON".to_string())
}

/// `sqlite3_value_text(v)` as bytes; `None` for NULL.
fn value_text(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Integer(i) => Some(i.to_string().into_bytes()),
        Value::Real(r) => Some(format_real17(*r, false)),
        Value::Text(s) => Some(s.as_bytes().to_vec()),
        Value::Blob(b) => Some(b.to_vec()),
    }
}

/// `sqlite3_value_int64(v)`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "saturating float-to-int, as sqlite3's doubleToInt64"
)]
fn value_i64(v: &Value) -> i64 {
    match v {
        Value::Null => 0,
        Value::Integer(i) => *i,
        Value::Real(r) => *r as i64,
        Value::Text(s) => atoi64(s.as_bytes()),
        Value::Blob(b) => atoi64(b),
    }
}

/// `sqlite3Atoi64`'s value for the integer prefix of `z` (saturating).
fn atoi64(z: &[u8]) -> i64 {
    let mut i = 0usize;
    while at(z, i).is_ascii_whitespace() {
        i = add(i, 1);
    }
    let neg = at(z, i) == b'-';
    if matches!(at(z, i), b'-' | b'+') {
        i = add(i, 1);
    }
    let mut v: i64 = 0;
    while is_digit(at(z, i)) {
        let d = i64::from(at(z, i).wrapping_sub(b'0'));
        v = if neg {
            v.saturating_mul(10).saturating_sub(d)
        } else {
            v.saturating_mul(10).saturating_add(d)
        };
        i = add(i, 1);
    }
    v
}

/// `jsonParseFuncArg`: the JSON document an argument holds, `None` for
/// NULL, `malformed JSON` if it does not parse.
fn parse_arg(v: &Value) -> Result<Option<Node>, FunctionError> {
    if let Value::Blob(b) = v {
        if is_jsonb(b) {
            return jsonb_to_node(b).map(Some);
        }
    }
    let Some(text) = value_text(v) else {
        return Ok(None);
    };
    parse_text(&text)
        .map(|(node, _)| Some(node))
        .map_err(|_| malformed())
}

/// `jsonFunctionArgToBlob`: an SQL value as the JSON to insert.
fn value_to_node(v: &Value) -> Result<Node, FunctionError> {
    match v {
        Value::Null => Ok(Node::Null),
        Value::Blob(b) => {
            if is_jsonb(b) {
                jsonb_to_node(b)
            } else {
                Err(blob_error())
            }
        }
        Value::Text(s) => {
            if is_json(v) {
                parse_text(s.as_bytes())
                    .map(|(node, _)| node)
                    .map_err(|_| malformed())
            } else {
                Ok(Node::Str(JStr {
                    kind: J_TEXTRAW,
                    raw: s.as_bytes().to_vec(),
                }))
            }
        }
        Value::Real(r) => {
            if r.is_nan() {
                return Ok(Node::Null);
            }
            let text = format_real17(*r, false);
            Ok(Node::Num(
                J_FLOAT,
                match text.as_slice() {
                    [b'I', ..] => b"9e999".to_vec(),
                    [b'-', b'I', ..] => b"-9e999".to_vec(),
                    _ => text,
                },
            ))
        }
        Value::Integer(i) => Ok(Node::Num(J_INT, i.to_string().into_bytes())),
    }
}

/// `JSON cannot hold BLOB values`.
fn blob_error() -> FunctionError {
    FunctionError::Sql("JSON cannot hold BLOB values".to_string())
}

/// `jsonAppendSqlValue`: append an SQL value as JSON.
fn push_sql_value(out: &mut Vec<u8>, v: &Value) -> Result<(), FunctionError> {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Real(r) => out.extend_from_slice(&format_real17(*r, true)),
        Value::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Text(s) => {
            if is_json(v) {
                out.extend_from_slice(s.as_bytes());
            } else {
                push_string(out, s.as_bytes());
            }
        }
        Value::Blob(b) => {
            if !is_jsonb(b) {
                return Err(blob_error());
            }
            render(out, &jsonb_to_node(b)?);
        }
    }
    Ok(())
}

/// `jsonBadPathError`: the error for a failed path lookup.
fn path_error(path: &[u8], lk: Lk) -> FunctionError {
    if lk == Lk::TooDeep {
        return FunctionError::Sql("JSON path too deep".to_string());
    }
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    let text = String::from_utf8_lossy(span(path, 0, end)).replace('\'', "''");
    FunctionError::Sql(format!("bad JSON path: '{text}'"))
}

/// Render `node` as a marked JSON result.
fn node_result(node: &Node) -> Value {
    let mut out = Vec::new();
    render(&mut out, node);
    json_result(out)
}

/// Look `path` (the text after `$`) up in `root` without editing it.
fn lookup_path(root: &mut Node, path: &[u8]) -> Result<Found, Lk> {
    let walk = Walk {
        path: path.to_vec(),
        edit: Edit::Lookup,
        value: Node::Null,
    };
    step(root, &walk, 0, 0)
}

// ---------------------------------------------------------------------------
// The SQL functions
// ---------------------------------------------------------------------------

/// `json(X)`: X minified, JSON5 rewritten as canonical JSON.
fn json_fn(args: &[Value]) -> Result<Value, FunctionError> {
    json_remove(args)
}

/// `json_array(V...)`: a JSON array of the arguments.
fn json_array(args: &[Value]) -> Result<Value, FunctionError> {
    let mut out = vec![b'['];
    for (k, v) in args.iter().enumerate() {
        if k > 0 {
            out.push(b',');
        }
        push_sql_value(&mut out, v)?;
    }
    out.push(b']');
    Ok(json_result(out))
}

/// `json_object(L, V, ...)`: a JSON object of label/value pairs.
fn json_object(args: &[Value]) -> Result<Value, FunctionError> {
    if !args.len().is_multiple_of(2) {
        return Err(FunctionError::Sql(
            "json_object() requires an even number of arguments".to_string(),
        ));
    }
    let mut out = vec![b'{'];
    let mut pending: Option<FunctionError> = None;
    for (k, pair) in args.chunks(2).enumerate() {
        let (Some(label), Some(value)) = (pair.first(), pair.get(1)) else {
            break;
        };
        let Value::Text(label) = label else {
            return Err(FunctionError::Sql(
                "json_object() labels must be TEXT".to_string(),
            ));
        };
        if k > 0 {
            out.push(b',');
        }
        push_string(&mut out, label.as_bytes());
        out.push(b':');
        if let Err(e) = push_sql_value(&mut out, value) {
            pending.get_or_insert(e);
        }
    }
    if let Some(e) = pending {
        return Err(e);
    }
    out.push(b'}');
    Ok(json_result(out))
}

/// `json_quote(V)`: V as a JSON value.
fn json_quote(args: &[Value]) -> Result<Value, FunctionError> {
    let mut out = Vec::new();
    if let Some(v) = args.first() {
        push_sql_value(&mut out, v)?;
    }
    Ok(json_result(out))
}

/// `json_array_length(X[, P])`: element count of the array at P (0 if
/// not an array, NULL if P is missing).
fn json_array_length(args: &[Value]) -> Result<Value, FunctionError> {
    let Some(mut root) = parse_arg(args.first().unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    let len = match args.get(1) {
        None => root.array_len(),
        Some(p) => {
            let Some(path) = value_text(p) else {
                return Ok(Value::Null);
            };
            let rest: &[u8] = if at(&path, 0) == b'$' {
                path.get(1..).unwrap_or(&[])
            } else {
                b"@"
            };
            match lookup_path(&mut root, rest) {
                Ok(f) => {
                    if f.jtype == J_ARRAY {
                        f.len
                    } else {
                        0
                    }
                }
                Err(Lk::NotFound) => return Ok(Value::Null),
                Err(lk) => return Err(path_error(&path, lk)),
            }
        }
    };
    Ok(Value::Integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

/// `json_error_position(X)`: 0 if X is well-formed (JSON5 allowed), else
/// the 1-based character position of the first error.
fn json_error_position(args: &[Value]) -> Result<Value, FunctionError> {
    let v = args.first().unwrap_or(&Value::Null);
    if let Value::Blob(b) = v {
        if is_jsonb(b) {
            let pos = validity_check(b, 0, b.len(), 1);
            return Ok(Value::Integer(i64::try_from(pos).unwrap_or(i64::MAX)));
        }
    }
    let Some(text) = value_text(v) else {
        return Ok(Value::Null);
    };
    let pos = match parse_text(&text) {
        Ok(_) => 0,
        Err(err) => {
            let chars = text
                .iter()
                .take(err)
                .take_while(|&&b| b != 0)
                .filter(|&&b| b & 0xc0 != 0x80)
                .count();
            add(chars, 1)
        }
    };
    Ok(Value::Integer(i64::try_from(pos).unwrap_or(i64::MAX)))
}

/// `json_insert(X, P, V, ...)`: add V at each P that does not exist.
fn json_insert(args: &[Value]) -> Result<Value, FunctionError> {
    edit_fn(args, Edit::Insert, "insert")
}

/// `json_replace(X, P, V, ...)`: overwrite each P that exists with V.
fn json_replace(args: &[Value]) -> Result<Value, FunctionError> {
    edit_fn(args, Edit::Replace, "replace")
}

/// `json_set(X, P, V, ...)`: write V at each P, creating it if needed.
fn json_set(args: &[Value]) -> Result<Value, FunctionError> {
    edit_fn(args, Edit::Set, "set")
}

/// `jsonSetFunc`/`jsonReplaceFunc` + `jsonInsertIntoBlob`.
fn edit_fn(args: &[Value], edit: Edit, name: &str) -> Result<Value, FunctionError> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Value::Null);
    };
    if args.len().is_multiple_of(2) {
        return Err(FunctionError::Sql(format!(
            "json_{name}() needs an odd number of arguments"
        )));
    }
    let Some(mut root) = parse_arg(first)? else {
        return Ok(Value::Null);
    };
    for pair in rest.chunks(2) {
        let (Some(p), Some(v)) = (pair.first(), pair.get(1)) else {
            break;
        };
        let Some(path) = value_text(p) else {
            continue;
        };
        if at(&path, 0) != b'$' {
            return Err(path_error(&path, Lk::PathError));
        }
        let value = value_to_node(v)?;
        if at(&path, 1) == 0 {
            if matches!(edit, Edit::Replace | Edit::Set) {
                root = value;
            }
            continue;
        }
        let walk = Walk {
            path: path.get(1..).unwrap_or(&[]).to_vec(),
            edit,
            value,
        };
        match step(&mut root, &walk, 0, 0) {
            Ok(_) | Err(Lk::NotFound) => {}
            Err(lk) => return Err(path_error(&path, lk)),
        }
    }
    Ok(node_result(&root))
}

/// `json_remove(X, P...)`: X without the elements at each P.
fn json_remove(args: &[Value]) -> Result<Value, FunctionError> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Value::Null);
    };
    let Some(mut root) = parse_arg(first)? else {
        return Ok(Value::Null);
    };
    for p in rest {
        let Some(path) = value_text(p) else {
            return Ok(Value::Null);
        };
        if at(&path, 0) != b'$' {
            return Err(path_error(&path, Lk::PathError));
        }
        if at(&path, 1) == 0 {
            return Ok(Value::Null);
        }
        let walk = Walk {
            path: path.get(1..).unwrap_or(&[]).to_vec(),
            edit: Edit::Delete,
            value: Node::Null,
        };
        match step(&mut root, &walk, 0, 0) {
            Ok(_) | Err(Lk::NotFound) => {}
            Err(lk) => return Err(path_error(&path, lk)),
        }
    }
    Ok(node_result(&root))
}

/// `json_patch(T, P)`: RFC 7396 merge patch of P into T.
fn json_patch(args: &[Value]) -> Result<Value, FunctionError> {
    let Some(mut target) = parse_arg(args.first().unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    let Some(patch) = parse_arg(args.get(1).unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    merge_patch(&mut target, &patch, 0)
        .map_err(|()| FunctionError::Sql("JSON nested too deep".to_string()))?;
    Ok(node_result(&target))
}

/// The `json_type` name of a JSONB type (`jsonbType[]`).
fn type_name(t: u8) -> &'static str {
    match t {
        J_NULL => "null",
        J_TRUE => "true",
        J_FALSE => "false",
        J_INT | J_INT5 => "integer",
        J_FLOAT | J_FLOAT5 => "real",
        J_TEXT..=J_TEXTRAW => "text",
        J_ARRAY => "array",
        _ => "object",
    }
}

/// `json_type(X[, P])`: the type of X, or of the element at P.
fn json_type(args: &[Value]) -> Result<Value, FunctionError> {
    let Some(mut root) = parse_arg(args.first().unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    let t = match args.get(1) {
        None => root.jtype(),
        Some(p) => {
            let Some(path) = value_text(p) else {
                return Ok(Value::Null);
            };
            if at(&path, 0) != b'$' {
                return Err(path_error(&path, Lk::PathError));
            }
            match lookup_path(&mut root, path.get(1..).unwrap_or(&[])) {
                Ok(f) => f.jtype,
                Err(Lk::NotFound) => return Ok(Value::Null),
                Err(lk) => return Err(path_error(&path, lk)),
            }
        }
    };
    Ok(text_result(type_name(t)))
}

/// `json_valid(X[, F])`: 1 if X is well-formed per flags F (1: RFC 8259
/// text, 2: JSON5 text, 4: superficially JSONB, 8: strictly JSONB).
fn json_valid(args: &[Value]) -> Result<Value, FunctionError> {
    let mut flags: i64 = 1;
    if let Some(f) = args.get(1) {
        flags = value_i64(f);
        if !(1..=15).contains(&flags) {
            return Err(FunctionError::Sql(
                "FLAGS parameter to json_valid() must be between 1 and 15".to_string(),
            ));
        }
    }
    let v = args.first().unwrap_or(&Value::Null);
    if let Value::Blob(b) = v {
        if is_jsonb(b) {
            let ok = if flags & 0x04 != 0 {
                true
            } else {
                flags & 0x08 != 0 && validity_check(b, 0, b.len(), 1) == 0
            };
            return Ok(Value::Integer(i64::from(ok)));
        }
    }
    let Some(text) = value_text(v) else {
        return Ok(Value::Null);
    };
    if flags & 0x03 == 0 {
        return Ok(Value::Integer(0));
    }
    let ok = match parse_text(&text) {
        Ok((_, nonstd)) => flags & 0x02 != 0 || !nonstd,
        Err(_) => false,
    };
    Ok(Value::Integer(i64::from(ok)))
}

#[cfg(test)]
mod tests {
    //! Expectations taken from sqlite3 3.53.4 via
    //! `tools/row_functions_cases_json.sql`. In string literals `~` stands
    //! for a backslash, to keep JSON escapes readable.

    use std::rc::Rc;

    use crate::functions::{call, FunctionError};
    use crate::value::Value;

    /// A TEXT value, `~` read as a backslash.
    fn t(s: &str) -> Value {
        Value::Text(Rc::from(s.replace('~', "\\")))
    }

    /// A BLOB value.
    fn blob(b: &[u8]) -> Value {
        Value::Blob(Rc::from(b))
    }

    /// `name(args)`, which must succeed.
    fn ok(name: &str, args: &[Value]) -> Value {
        call(name, args).unwrap()
    }

    /// `name(args)`, which must return TEXT.
    fn text(name: &str, args: &[Value]) -> String {
        match ok(name, args) {
            Value::Text(s) => s.to_string(),
            other => panic!("{name}: expected text, got {other:?}"),
        }
    }

    /// `name(args)`'s error message, which must be an SQL error.
    fn err(name: &str, args: &[Value]) -> String {
        match call(name, args) {
            Err(FunctionError::Sql(m)) => m,
            other => panic!("{name}: expected an SQL error, got {other:?}"),
        }
    }

    /// An expected string, `~` read as a backslash.
    fn exp(s: &str) -> String {
        s.replace('~', "\\")
    }

    /// `inner` wrapped in `n` levels of `open` ... `close`.
    fn nested(open: &str, close: &str, n: usize, inner: &str) -> String {
        format!("{}{inner}{}", open.repeat(n), close.repeat(n))
    }

    #[test]
    fn json_minifies() {
        assert_eq!(
            text("json", &[t(r#" { "a" : [ 1 , 2 , 3 ] , "b" : null } "#)]),
            r#"{"a":[1,2,3],"b":null}"#
        );
        assert_eq!(text("json", &[t("  42  ")]), "42");
        assert_eq!(text("json", &[t(r#"{"a":1,"a":2}"#)]), r#"{"a":1,"a":2}"#);
    }

    #[test]
    fn json_keeps_number_and_escape_text() {
        assert_eq!(text("json", &[t("1E5")]), "1E5");
        assert_eq!(text("json", &[t("-0.0e-0")]), "-0.0e-0");
        assert_eq!(text("json", &[t(r#""~u00e9~/x""#)]), exp(r#""~u00e9~/x""#));
    }

    #[test]
    fn json_canonicalises_json5_numbers() {
        assert_eq!(
            text("json", &[t("[.5, 5., +1, 0x1F, -0x10, 1.e3, -.5, +.5]")]),
            "[0.5,5.0,1,31,-16,1.0e3,-0.5,0.5]"
        );
        assert_eq!(text("json", &[t("0xFFFFFFFFFFFFFFFFFF")]), "9.0e999");
        assert_eq!(
            text("json", &[t("[Infinity, NaN, QNaN, SNaN, inf, -inf]")]),
            "[9e999,null,null,null,9e999,-9e999]"
        );
    }

    #[test]
    fn json_canonicalises_json5_strings_and_labels() {
        assert_eq!(
            text("json", &[t("{a:1, $b:2, _c:3, d4:4}")]),
            r#"{"a":1,"$b":2,"_c":3,"d4":4}"#
        );
        assert_eq!(text("json", &[t(r#"'it"s'"#)]), exp(r#""it~"s""#));
        assert_eq!(
            text("json", &[t(r#""~x41~v~0""#)]),
            exp(r#""~u0041~u000b~u0000""#)
        );
        assert_eq!(
            text("json", &[t("\"ctl\u{1}\u{1f}\"")]),
            exp(r#""ctl~u0001~u001f""#)
        );
        assert_eq!(text("json", &[t("\"line~\nnext\"")]), r#""linenext""#);
        assert_eq!(text("json", &[t("{a~u0062:1}")]), exp(r#"{"a~u0062":1}"#));
    }

    #[test]
    fn json_skips_json5_whitespace_and_comments() {
        assert_eq!(
            text("json", &[t("/* c */ [1, /* inner */ 2] // tail")]),
            "[1,2]"
        );
        assert_eq!(text("json", &[t("\u{feff}[1,2,]")]), "[1,2]");
        assert_eq!(text("json", &[t("\u{a0}1\u{2007}")]), "1");
    }

    #[test]
    fn json_of_sql_values() {
        assert_eq!(text("json", &[Value::Integer(-5)]), "-5");
        assert_eq!(text("json", &[Value::Real(1e300)]), "1.0e+300");
        assert_eq!(ok("json", &[Value::Null]), Value::Null);
    }

    #[test]
    fn json_rejects_malformed_input() {
        for bad in [
            "", "[1,2", "01", "{1:2}", "[,1]", "1 2", "\"~q\"", "/* open", "-nan",
        ] {
            assert_eq!(err("json", &[t(bad)]), "malformed JSON", "{bad}");
        }
    }

    #[test]
    fn json_error_position_matches_sqlite() {
        let pos = |s: &str| ok("json_error_position", &[t(s)]);
        assert_eq!(pos("[1,2]"), Value::Integer(0));
        assert_eq!(pos("{a:1}"), Value::Integer(0));
        assert_eq!(pos("1 2"), Value::Integer(1));
        assert_eq!(pos("{}x"), Value::Integer(2));
        assert_eq!(pos("[1,[]] x"), Value::Integer(5));
        assert_eq!(pos("[\"é\", x]"), Value::Integer(7));
        assert_eq!(pos("{\"a\":1,,}"), Value::Integer(8));
        assert_eq!(pos("{ 1:2}"), Value::Integer(2));
        assert_eq!(ok("json_error_position", &[Value::Null]), Value::Null);
        assert_eq!(
            ok("json_error_position", &[blob(&[0x1c, 0x17])]),
            Value::Integer(2)
        );
    }

    #[test]
    fn json_valid_flags() {
        let valid = |args: &[Value]| ok("json_valid", args);
        assert_eq!(valid(&[t("{a:1}")]), Value::Integer(0));
        assert_eq!(valid(&[t("{a:1}"), Value::Integer(2)]), Value::Integer(1));
        assert_eq!(valid(&[t("[1]"), Value::Integer(4)]), Value::Integer(0));
        assert_eq!(
            valid(&[blob(&[0x00]), Value::Integer(8)]),
            Value::Integer(1)
        );
        assert_eq!(valid(&[blob(b"[1]")]), Value::Integer(1));
        assert_eq!(valid(&[Value::Null]), Value::Null);
        assert_eq!(
            err("json_valid", &[t("[1]"), Value::Integer(0)]),
            "FLAGS parameter to json_valid() must be between 1 and 15"
        );
    }

    #[test]
    fn json_valid_depth_limit() {
        let ok_depth = nested("[", "]", 1000, "");
        let too_deep = nested("[", "]", 1001, "");
        assert_eq!(ok("json_valid", &[t(&ok_depth)]), Value::Integer(1));
        assert_eq!(ok("json_valid", &[t(&too_deep)]), Value::Integer(0));
        assert_eq!(
            ok("json_error_position", &[t(&too_deep)]),
            Value::Integer(1001)
        );
    }

    #[test]
    fn json_quote_formats_reals_like_sqlite() {
        let q = |r: f64| text("json_quote", &[Value::Real(r)]);
        assert_eq!(q(1.0 / 3.0), "0.33333333333333332");
        assert_eq!(q(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(q(0.1), "0.1");
        assert_eq!(q(49.47), "49.47");
        assert_eq!(q(100.0), "100.0");
        assert_eq!(q(1e15), "1000000000000000.0");
        assert_eq!(q(1e17), "1.0e+17");
        assert_eq!(q(1e-7), "1.0e-07");
        assert_eq!(q(-2.5e-5), "-2.5e-05");
        assert_eq!(q(5e-324), "4.9406564584124654e-324");
        assert_eq!(q(1e-310), "9.9999999999999695e-311");
        assert_eq!(q(1e21 + 1e5), "1.0000000000000001e+21");
        assert_eq!(q(f64::INFINITY), "9.0e+999");
        assert_eq!(q(-0.0), "0.0");
    }

    #[test]
    fn json_quote_strings_and_nulls() {
        assert_eq!(text("json_quote", &[t("a\"b~c")]), exp(r#""a~"b~~c""#));
        assert_eq!(
            text("json_quote", &[t("tab\t\u{1}\u{7f}")]),
            exp("\"tab~t~u0001\u{7f}\"")
        );
        assert_eq!(text("json_quote", &[Value::Null]), "null");
        assert_eq!(text("json_quote", &[Value::Integer(-7)]), "-7");
        assert_eq!(
            err("json_quote", &[blob(&[0xff])]),
            "JSON cannot hold BLOB values"
        );
    }

    #[test]
    fn json_array_nests_json_results_but_quotes_text() {
        let inner = ok("json", &[t("[1]")]);
        assert_eq!(text("json_array", std::slice::from_ref(&inner)), "[[1]]");
        assert_eq!(text("json_array", &[t("[1]")]), r#"["[1]"]"#);
        // A copy of the same text is a different allocation: plain text.
        let Value::Text(s) = &inner else { panic!() };
        let copy = Value::Text(Rc::from(s.to_string()));
        assert_eq!(text("json_array", &[copy]), r#"["[1]"]"#);
        let quoted = ok("json_quote", &[t("x")]);
        assert_eq!(text("json_array", &[quoted]), r#"["x"]"#);
    }

    #[test]
    fn json_array_values() {
        assert_eq!(text("json_array", &[]), "[]");
        assert_eq!(
            text(
                "json_array",
                &[Value::Integer(1), t("two"), Value::Real(3.5), Value::Null]
            ),
            r#"[1,"two",3.5,null]"#
        );
        assert_eq!(
            text("json_array", &[blob(&[0x01]), blob(&[0x0b])]),
            "[true,[]]"
        );
        assert_eq!(
            err("json_array", &[Value::Integer(1), blob(&[0xab, 0xcd])]),
            "JSON cannot hold BLOB values"
        );
    }

    #[test]
    fn json_object_pairs_and_errors() {
        assert_eq!(
            text(
                "json_object",
                &[t("a"), Value::Integer(1), t("a"), Value::Integer(2)]
            ),
            r#"{"a":1,"a":2}"#
        );
        let arr = ok("json", &[t("[1,2]")]);
        assert_eq!(text("json_object", &[t("a"), arr]), r#"{"a":[1,2]}"#);
        assert_eq!(
            text("json_object", &[t("k\"ey"), t("[1,2]")]),
            exp(r#"{"k~"ey":"[1,2]"}"#)
        );
        assert_eq!(
            err("json_object", &[t("a")]),
            "json_object() requires an even number of arguments"
        );
        assert_eq!(
            err("json_object", &[Value::Integer(1), Value::Integer(2)]),
            "json_object() labels must be TEXT"
        );
    }

    #[test]
    fn json_array_length_paths() {
        let len = |args: &[Value]| ok("json_array_length", args);
        assert_eq!(len(&[t("[1,2,3]")]), Value::Integer(3));
        assert_eq!(len(&[t("{}")]), Value::Integer(0));
        assert_eq!(len(&[t(r#"{"a":[1,2]}"#), t("$.a")]), Value::Integer(2));
        assert_eq!(len(&[t(r#"{"a":[1,2]}"#), t("$.b")]), Value::Null);
        assert_eq!(len(&[t("[1,[2,3,4]]"), t("$[#-1]")]), Value::Integer(3));
        assert_eq!(
            err("json_array_length", &[t("[1]"), t("x")]),
            "bad JSON path: 'x'"
        );
    }

    #[test]
    fn json_type_of_every_kind() {
        let ty = |s: &str| text("json_type", &[t(s)]);
        assert_eq!(ty("null"), "null");
        assert_eq!(ty("true"), "true");
        assert_eq!(ty("0x10"), "integer");
        assert_eq!(ty(".5"), "real");
        assert_eq!(ty("Infinity"), "real");
        assert_eq!(ty("'s'"), "text");
        assert_eq!(ty("[]"), "array");
        assert_eq!(ty("{}"), "object");
        assert_eq!(
            text("json_type", &[blob(&[0x2b, 0x01, 0x02]), t("$[1]")]),
            "false"
        );
    }

    #[test]
    fn json_type_path_forms() {
        let ty = |doc: &str, path: &str| ok("json_type", &[t(doc), t(path)]);
        let doc = r#"{"a":[1,2.5,"x",null,true]}"#;
        assert_eq!(ty(doc, "$.a[1]"), t("real"));
        assert_eq!(ty(doc, "$.a[#-1]"), t("true"));
        assert_eq!(ty(doc, "$.a[5]"), Value::Null);
        assert_eq!(ty(r#"{"a.b":1}"#, r#"$."a.b""#), t("integer"));
        assert_eq!(ty(r#"{"a.b":1}"#, "$.a.b"), Value::Null);
        assert_eq!(ty(r#"{"é":1}"#, r#"$."~u00e9""#), t("integer"));
        assert_eq!(ty(r#"{"a x":1}"#, "$.a x"), t("integer"));
        assert_eq!(ty("[1]", "$[99999999999999999999]"), Value::Null);
        // `[` on a non-array is NOTFOUND before the index is parsed.
        assert_eq!(ty(r#"{"a":1}"#, "$[x]"), Value::Null);
    }

    #[test]
    fn json_type_bad_paths() {
        assert_eq!(
            err("json_type", &[t("[1]"), t("$[x]")]),
            "bad JSON path: '$[x]'"
        );
        assert_eq!(
            err("json_type", &[t(r#"{"a":1}"#), t("$.")]),
            "bad JSON path: '$.'"
        );
        assert_eq!(
            err("json_type", &[t("[1]"), t("$[-1]")]),
            "bad JSON path: '$[-1]'"
        );
        assert_eq!(
            err("json_type", &[t("[1]"), t("it's")]),
            "bad JSON path: 'it''s'"
        );
        let deep = nested("[", "]", 999, "[1]");
        let path = format!("${}", "[0]".repeat(1000));
        assert_eq!(
            err("json_type", &[t(&deep), t(&path)]),
            "JSON path too deep"
        );
    }

    #[test]
    fn json_set_insert_replace() {
        let doc = t(r#"{"a":1}"#);
        let two = Value::Integer(2);
        assert_eq!(
            text("json_set", &[doc.clone(), t("$.a"), two.clone()]),
            r#"{"a":2}"#
        );
        assert_eq!(
            text("json_set", &[doc.clone(), t("$.b"), two.clone()]),
            r#"{"a":1,"b":2}"#
        );
        assert_eq!(
            text("json_insert", &[doc.clone(), t("$.a"), two.clone()]),
            r#"{"a":1}"#
        );
        assert_eq!(
            text("json_insert", &[doc.clone(), t("$.b"), two.clone()]),
            r#"{"a":1,"b":2}"#
        );
        assert_eq!(
            text("json_replace", &[doc.clone(), t("$.b"), two.clone()]),
            r#"{"a":1}"#
        );
        assert_eq!(
            text("json_replace", &[doc.clone(), t("$"), two.clone()]),
            "2"
        );
        assert_eq!(text("json_insert", &[doc, t("$"), two]), r#"{"a":1}"#);
    }

    #[test]
    fn json_set_arrays_and_substructure() {
        let set = |doc: &str, path: &str| text("json_set", &[t(doc), t(path), Value::Integer(9)]);
        assert_eq!(set("[1,2]", "$[#]"), "[1,2,9]");
        assert_eq!(set("[1,2]", "$[5]"), "[1,2]");
        assert_eq!(set("[1,2]", "$[#-1]"), "[1,9]");
        assert_eq!(set("{}", "$.a.b.c"), r#"{"a":{"b":{"c":9}}}"#);
        assert_eq!(set("{}", "$.a[0].b"), r#"{"a":[{"b":9}]}"#);
        assert_eq!(set("{}", "$.a[1]"), "{}");
        assert_eq!(set(r#"{"a":1}"#, r#"$."q~"t""#), exp(r#"{"a":1,"q~"t":9}"#));
        assert_eq!(set(r#"{"~u0061":1}"#, "$.a"), exp(r#"{"~u0061":9}"#));
    }

    #[test]
    fn json_set_values() {
        let set = |v: Value| text("json_set", &[t(r#"{"a":1}"#), t("$.a"), v]);
        assert_eq!(set(t("[1]")), r#"{"a":"[1]"}"#);
        assert_eq!(set(ok("json", &[t("[1]")])), r#"{"a":[1]}"#);
        assert_eq!(set(Value::Real(0.1 + 0.2)), r#"{"a":0.30000000000000004}"#);
        assert_eq!(set(Value::Real(f64::INFINITY)), r#"{"a":9e999}"#);
        assert_eq!(set(Value::Null), r#"{"a":null}"#);
        assert_eq!(set(blob(&[0x2b, 0x13, 0x31])), r#"{"a":[1]}"#);
    }

    #[test]
    fn json_set_errors_and_nulls() {
        let doc = t(r#"{"a":1}"#);
        assert_eq!(
            err("json_set", &[doc.clone(), t("$.a")]),
            "json_set() needs an odd number of arguments"
        );
        assert_eq!(
            err("json_replace", &[doc.clone(), t("$.a")]),
            "json_replace() needs an odd number of arguments"
        );
        assert_eq!(
            err("json_set", &[doc.clone(), t("$x"), Value::Integer(1)]),
            "bad JSON path: '$x'"
        );
        assert_eq!(
            err("json_set", &[doc.clone(), t("$.a"), blob(&[0xff])]),
            "JSON cannot hold BLOB values"
        );
        assert_eq!(
            err("json_set", &[t("[1"), t("$[0]"), Value::Integer(1)]),
            "malformed JSON"
        );
        assert_eq!(
            ok("json_set", &[Value::Null, t("$.a"), Value::Integer(1)]),
            Value::Null
        );
        assert_eq!(
            text(
                "json_set",
                &[
                    doc,
                    Value::Null,
                    Value::Integer(2),
                    t("$.c"),
                    Value::Integer(3)
                ]
            ),
            r#"{"a":1,"c":3}"#
        );
    }

    #[test]
    fn json_remove_paths() {
        let rm = |doc: &str, paths: &[&str]| {
            let mut args = vec![t(doc)];
            args.extend(paths.iter().map(|p| t(p)));
            ok("json_remove", &args)
        };
        assert_eq!(rm("[1,2,3]", &["$[#-1]"]), t("[1,2]"));
        assert_eq!(rm("[1,2,3]", &["$[0]", "$[0]"]), t("[3]"));
        assert_eq!(rm(r#"{"a":1,"a":2}"#, &["$.a"]), t(r#"{"a":2}"#));
        assert_eq!(rm(r#"{"a":1}"#, &["$.b"]), t(r#"{"a":1}"#));
        assert_eq!(rm("[1,2]", &["$"]), Value::Null);
        assert_eq!(rm(" [ 1 , 2 ] ", &[]), t("[1,2]"));
        assert_eq!(
            err("json_remove", &[t(r#"{"a":1}"#), t("$.b"), t("$x")]),
            "bad JSON path: '$x'"
        );
    }

    #[test]
    fn json_patch_rfc7396() {
        let patch = |a: &str, b: &str| text("json_patch", &[t(a), t(b)]);
        assert_eq!(
            patch(r#"{"a":1,"b":2}"#, r#"{"b":3,"c":4}"#),
            r#"{"a":1,"b":3,"c":4}"#
        );
        assert_eq!(
            patch(r#"{"a":{"x":1}}"#, r#"{"a":{"x":null}}"#),
            r#"{"a":{}}"#
        );
        assert_eq!(
            patch("{}", r#"{"a":{"b":{"c":null,"d":1}}}"#),
            r#"{"a":{"b":{"d":1}}}"#
        );
        assert_eq!(patch(r#"{"a":1}"#, "[1,2]"), "[1,2]");
        assert_eq!(patch(r#"{"a":1,"a":2}"#, r#"{"a":3}"#), r#"{"a":3,"a":2}"#);
        assert_eq!(
            patch(r#"{"~u0061":1}"#, r#"{"a":2}"#),
            exp(r#"{"~u0061":2}"#)
        );
        assert_eq!(ok("json_patch", &[t("{}"), Value::Null]), Value::Null);
        assert_eq!(ok("json_patch", &[Value::Null, t("{")]), Value::Null);
        assert_eq!(err("json_patch", &[t("{}"), t("{")]), "malformed JSON");
    }

    #[test]
    fn jsonb_blob_inputs() {
        assert_eq!(
            text("json", &[blob(&[0xcb, 0x02, 0x01, 0x02])]),
            "[true,false]"
        );
        assert_eq!(
            text("json", &[blob(&[0x4c, 0x17, 0x61, 0x13, 0x31])]),
            r#"{"a":1}"#
        );
        // Not JSONB: read as JSON text.
        assert_eq!(text("json", &[blob(b"[1,2]")]), "[1,2]");
        assert_eq!(text("json", &[blob(b"{}")]), "{}");
        assert_eq!(err("json", &[blob(&[0x3b, 0x13])]), "malformed JSON");
    }
}
