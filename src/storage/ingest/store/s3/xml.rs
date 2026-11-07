//! The little XML S3 answers in: pull a tag's text out, unescape it.

/// The text of every `<tag>...</tag>` in `xml`, unescaped, in order.
pub fn all(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some((_, after)) = rest.split_once(open.as_str()) {
        let Some((inner, tail)) = after.split_once(close.as_str()) else {
            break;
        };
        out.push(unescape(inner));
        rest = tail;
    }
    out
}

/// The text of the first `<tag>...</tag>`.
pub fn first(xml: &str, tag: &str) -> Option<String> {
    all(xml, tag).into_iter().next()
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        let Some((entity, tail)) = after.split_once(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => out.push(c),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_found_in_order_and_unescaped() {
        let xml =
            "<R><Key>a&amp;b</Key><Key>c&lt;d&#13;e&#x41;</Key><IsTruncated>true</IsTruncated></R>";
        assert_eq!(all(xml, "Key"), ["a&b", "c<d\reA"]);
        assert_eq!(first(xml, "IsTruncated").as_deref(), Some("true"));
        assert_eq!(first(xml, "Missing"), None);
    }

    #[test]
    fn an_unknown_entity_and_a_stray_ampersand_are_kept() {
        assert_eq!(all("<K>a&nope;b &amp</K>", "K"), ["a&nope;b &amp"]);
    }
}
