//! Minimal, zero-copy JSON value parser for `jsonl` (#319). Only what the
//! stream engine needs: object/array/string/number/bool/null, borrowing
//! string spans directly from the source line (no unescaping -- an escaped
//! string is stored verbatim, backslashes and all; late `json_extract`
//! (#307) can unescape on demand).

/// A parsed JSON value, borrowing from the source line.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue<'a> {
    /// `null`.
    Null,
    /// `true`/`false`.
    Bool(bool),
    /// A number that parsed as an integer.
    Int(i64),
    /// A number that did not fit (or wasn't) an integer.
    Float(f64),
    /// A string's contents (quotes stripped, escapes left verbatim).
    Str(&'a str),
    /// An object: key/value pairs in source order.
    Object(Vec<(&'a str, JsonValue<'a>)>),
    /// An array, kept as its raw source text (deeper structures are not
    /// flattened -- ADR-0018: "keep as string for late `json_extract`").
    Array(&'a str),
}

/// Parse one JSON value starting at `s`. Returns `(value, rest)` where
/// `rest` is the unparsed remainder, or `None` on malformed input.
#[must_use]
pub fn parse_value(s: &str) -> Option<(JsonValue<'_>, &str)> {
    let s = s.trim_start();
    match s.as_bytes().first()? {
        b'{' => parse_object(s),
        b'[' => parse_array(s),
        b'"' => parse_string(s).map(|(v, r)| (JsonValue::Str(v), r)),
        b't' if s.starts_with("true") => Some((JsonValue::Bool(true), s.get(4..)?)),
        b'f' if s.starts_with("false") => Some((JsonValue::Bool(false), s.get(5..)?)),
        b'n' if s.starts_with("null") => Some((JsonValue::Null, s.get(4..)?)),
        b'-' | b'0'..=b'9' => parse_number(s),
        _ => None,
    }
}

/// Parse a top-level JSON object. Returns `None` if `s` doesn't start an
/// object or is malformed.
#[must_use]
pub fn parse_object(s: &str) -> Option<(JsonValue<'_>, &str)> {
    let s = s.trim_start();
    let mut rest = s.strip_prefix('{')?;
    rest = rest.trim_start();
    let mut fields = Vec::new();

    if let Some(after) = rest.strip_prefix('}') {
        return Some((JsonValue::Object(fields), after));
    }

    loop {
        let (key, after_key) = parse_string(rest)?;
        let after_key = after_key.trim_start();
        let after_colon = after_key.strip_prefix(':')?.trim_start();
        let (value, after_value) = parse_value(after_colon)?;
        fields.push((key, value));
        let after_value = after_value.trim_start();
        if let Some(after) = after_value.strip_prefix(',') {
            rest = after.trim_start();
        } else if let Some(after) = after_value.strip_prefix('}') {
            return Some((JsonValue::Object(fields), after));
        } else {
            return None;
        }
    }
}

/// Parse a JSON array, returning its raw source text (bracket to bracket)
/// rather than decoded elements -- arrays are stored as-is.
fn parse_array(s: &str) -> Option<(JsonValue<'_>, &str)> {
    debug_assert!(s.starts_with('['));
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, b) in s.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' => depth = depth.saturating_add(1),
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = i.checked_add(1)?;
                    return Some((JsonValue::Array(s.get(0..end)?), s.get(end..)?));
                }
            }
            _ => {}
        }
    }
    None
}

/// Parse a `"quoted string"`, returning its inner content (no unescaping)
/// and the remainder after the closing quote.
fn parse_string(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let rest = s.strip_prefix('"')?;
    let mut escaped = false;
    for (i, b) in rest.bytes().enumerate() {
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == b'"' {
            let after = i.checked_add(1)?;
            return Some((rest.get(..i)?, rest.get(after..)?));
        }
    }
    None
}

/// Parse a JSON number (integer or float).
fn parse_number(s: &str) -> Option<(JsonValue<'_>, &str)> {
    let bytes = s.as_bytes();
    let mut i: usize = 0;
    if bytes.first() == Some(&b'-') {
        i = i.checked_add(1)?;
    }
    let start_digits = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i = i.checked_add(1)?;
    }
    if i == start_digits {
        return None;
    }
    let mut is_float = false;
    if bytes.get(i) == Some(&b'.') {
        is_float = true;
        i = i.checked_add(1)?;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i = i.checked_add(1)?;
        }
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        is_float = true;
        i = i.checked_add(1)?;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i = i.checked_add(1)?;
        }
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i = i.checked_add(1)?;
        }
    }
    let text = s.get(..i)?;
    let rest = s.get(i..)?;
    if is_float {
        text.parse::<f64>()
            .ok()
            .map(|v| (JsonValue::Float(v), rest))
    } else {
        match text.parse::<i64>() {
            Ok(v) => Some((JsonValue::Int(v), rest)),
            Err(_) => text
                .parse::<f64>()
                .ok()
                .map(|v| (JsonValue::Float(v), rest)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_object() {
        let (v, rest) = parse_object(r#"{"a":1,"b":"two","c":true,"d":null}"#).expect("parses");
        assert_eq!(rest, "");
        let JsonValue::Object(fields) = v else {
            panic!("expected object")
        };
        assert_eq!(fields[0], ("a", JsonValue::Int(1)));
        assert_eq!(fields[1], ("b", JsonValue::Str("two")));
        assert_eq!(fields[2], ("c", JsonValue::Bool(true)));
        assert_eq!(fields[3], ("d", JsonValue::Null));
    }

    #[test]
    fn parses_nested_object_and_array() {
        let (v, _) =
            parse_object(r#"{"req":{"method":"GET","tags":[1,2,3]},"x":1.5}"#).expect("parses");
        let JsonValue::Object(fields) = v else {
            panic!("expected object")
        };
        let JsonValue::Object(req) = &fields[0].1 else {
            panic!("expected nested object")
        };
        assert_eq!(req[0], ("method", JsonValue::Str("GET")));
        assert_eq!(req[1].1, JsonValue::Array("[1,2,3]"));
        assert_eq!(fields[1].1, JsonValue::Float(1.5));
    }

    #[test]
    fn empty_object_parses() {
        let (v, rest) = parse_object("{}").expect("parses");
        assert_eq!(rest, "");
        assert_eq!(v, JsonValue::Object(vec![]));
    }

    #[test]
    fn malformed_object_returns_none() {
        assert!(parse_object(r#"{"a":}"#).is_none());
        assert!(parse_object(r#"{"a":1"#).is_none());
        assert!(parse_object("not json").is_none());
    }
}
