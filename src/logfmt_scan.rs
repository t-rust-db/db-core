//! `key=value` scanning core, shared between `storage::stream::logfmt`
//! (#320, ingest-time parsing into `LogBatch`) and
//! `functions::logfmt_extract` (#307, late parse of one line). Moved to
//! the crate root because `functions` must stay feature-free (ADR
//! 0011) while `storage::stream` lives behind the `storage-stream`
//! feature -- one scanner, not two.

/// Tokenize `key=value` pairs, quoted values (with `\"`/`\\` escapes
/// left verbatim, same as `jsonl`), and bare keys (`debug` -> `(key,
/// None)`). Duplicate keys: last one wins by construction (the caller
/// decides how to fold repeats; this iterator yields every occurrence in
/// source order). An ingested table decodes the escapes when it stores a
/// quoted value (#616); see [`quoted_values`].
pub fn scan(s: &str) -> impl Iterator<Item = (&str, Option<&str>)> {
    tokens(s).map(|(key, value)| (key, value.map(|(text, _)| text)))
}

/// The values of `s` that were written in quotes, escapes left verbatim.
pub(crate) fn quoted_values(s: &str) -> impl Iterator<Item = &str> {
    tokens(s).filter_map(|(_, value)| match value {
        Some((text, true)) => Some(text),
        _ => None,
    })
}

/// [`scan`], with each value paired with whether it was quoted.
fn tokens(s: &str) -> impl Iterator<Item = (&str, Option<(&str, bool)>)> {
    let mut rest = s;
    std::iter::from_fn(move || loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let key_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        let key = rest.get(..key_end)?;
        if key.is_empty() {
            // Stray '=' with no key; skip one char and keep scanning.
            let skip = rest.chars().next().map_or(0, char::len_utf8);
            rest = rest.get(skip..).unwrap_or("");
            continue;
        }
        let after_key = rest.get(key_end..)?;
        let Some(after_eq) = after_key.strip_prefix('=') else {
            rest = after_key;
            return Some((key, None));
        };
        if let Some(after_quote) = after_eq.strip_prefix('"') {
            let mut escaped = false;
            for (i, b) in after_quote.bytes().enumerate() {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    let value = after_quote.get(..i).unwrap_or("");
                    let after = i.checked_add(1).unwrap_or(after_quote.len());
                    rest = after_quote.get(after..).unwrap_or("");
                    return Some((key, Some((value, true))));
                }
            }
            // Unterminated quote: take the rest of the line.
            rest = "";
            return Some((key, Some((after_quote, true))));
        }
        let value_end = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
        let value = after_eq.get(..value_end).unwrap_or("");
        rest = after_eq.get(value_end..).unwrap_or("");
        return Some((key, Some((value, false))));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_heroku_style_line() {
        let pairs: Vec<_> =
            scan("ts=2026-09-10T08:00:05Z level=warn msg=\"request slow\"").collect();
        assert_eq!(pairs[0], ("ts", Some("2026-09-10T08:00:05Z")));
        assert_eq!(pairs[1], ("level", Some("warn")));
        assert_eq!(pairs[2], ("msg", Some("request slow")));
    }

    #[test]
    fn bare_key_has_no_value() {
        let pairs: Vec<_> = scan("msg=hi debug").collect();
        assert_eq!(pairs[1], ("debug", None));
    }

    #[test]
    fn quoted_values_skips_bare_values_and_keys() {
        let quoted: Vec<_> =
            quoted_values(r#"a=1 msg="x \"y\"" debug path=C:\new u="open"#).collect();
        assert_eq!(quoted, vec![r#"x \"y\""#, "open"]);
    }
}
