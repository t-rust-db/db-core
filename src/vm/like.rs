//! `LIKE`, `ILIKE` and `GLOB` patterns compiled once per opcode (#691).
//!
//! A pattern whose only wildcard is "any run" (`%` for `LIKE`, `*` for
//! `GLOB`) is a list of literal segments: the text starts with the first
//! unless the pattern starts with the wildcard, ends with the last unless
//! it ends with one, and holds the others in order in between. Each
//! segment is found leftmost (`starts_with`, `find`, `ends_with` on the
//! text's bytes), which is exact when the wildcard matches any run:
//! taking a later match for a segment never leaves more room for the
//! rest. Any other pattern (`_`, `?`, `[...]`) goes to the general
//! recursive matchers in [`crate::functions`], so every pattern means what
//! it did before.

use crate::functions::{glob_match, like_match_exact};

/// Which pattern language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// `LIKE`/`ILIKE`: `%` any run, `_` any one character (DuckDB's
    /// case-sensitive `LIKE`, #679).
    Like,
    /// `GLOB`: `*` any run, `?` any one character, `[...]` a class.
    Glob,
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub(crate) struct Matcher {
    /// `ILIKE`: the pattern is lowercased once, each text as it is matched.
    nocase: bool,
    shape: Shape,
}

#[derive(Debug, Clone)]
enum Shape {
    /// The literal segments between wildcards, none empty.
    Segments {
        parts: Vec<Finder>,
        anchored_start: bool,
        anchored_end: bool,
    },
    /// Anything else, matched by the general matcher.
    General { pattern: String, dialect: Dialect },
}

impl Matcher {
    /// `pattern` in `dialect`; `nocase` only for `ILIKE`.
    pub(crate) fn new(pattern: &str, dialect: Dialect, nocase: bool) -> Matcher {
        let pattern = if nocase {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };
        let (any_run, general): (char, &[char]) = match dialect {
            Dialect::Like => ('%', &['_']),
            Dialect::Glob => ('*', &['?', '[']),
        };
        let shape = if pattern.chars().any(|c| general.contains(&c)) {
            Shape::General { pattern, dialect }
        } else {
            Shape::Segments {
                anchored_start: !pattern.starts_with(any_run),
                anchored_end: !pattern.ends_with(any_run),
                parts: pattern
                    .split(any_run)
                    .filter(|s| !s.is_empty())
                    .map(Finder::new)
                    .collect(),
            }
        };
        Matcher { nocase, shape }
    }

    /// Whether `text` matches. `buf` is scratch for `ILIKE`'s lowercased
    /// text, reused across rows so a match allocates nothing.
    pub(crate) fn matches(&self, text: &str, buf: &mut String) -> bool {
        if !self.nocase {
            return self.matches_as_is(text);
        }
        if text.is_ascii() {
            buf.clear();
            buf.push_str(text);
            buf.make_ascii_lowercase();
        } else {
            *buf = text.to_lowercase();
        }
        self.matches_as_is(buf)
    }

    fn matches_as_is(&self, text: &str) -> bool {
        match &self.shape {
            Shape::Segments {
                parts,
                anchored_start,
                anchored_end,
            } => segments_match(text, parts, *anchored_start, *anchored_end),
            Shape::General {
                pattern,
                dialect: Dialect::Like,
            } => like_match_exact(text, pattern),
            Shape::General {
                pattern,
                dialect: Dialect::Glob,
            } => glob_match(text, pattern),
        }
    }
}

/// A literal segment with its failure table built once, so each row's
/// search is linear in the text (`str::find` rebuilds a searcher per call,
/// #732). Found byte-wise: a match of valid UTF-8 in valid UTF-8 always
/// starts on a character boundary.
#[derive(Debug, Clone)]
struct Finder {
    needle: String,
    /// KMP failure function: `fail[i]` is the length of the longest proper
    /// border of `needle[..=i]`.
    fail: Vec<usize>,
}

impl Finder {
    fn new(needle: &str) -> Finder {
        let n = needle.as_bytes();
        let mut fail: Vec<usize> = Vec::with_capacity(n.len());
        let mut k = 0usize;
        for (i, b) in n.iter().enumerate() {
            if i > 0 {
                while k > 0 && n.get(k) != Some(b) {
                    k = fail.get(k.saturating_sub(1)).copied().unwrap_or(0);
                }
                if n.get(k) == Some(b) {
                    k = k.saturating_add(1);
                }
            }
            fail.push(k);
        }
        Finder {
            needle: needle.to_string(),
            fail,
        }
    }

    /// Byte offset of the leftmost occurrence in `text`.
    fn find(&self, text: &[u8]) -> Option<usize> {
        let n = self.needle.as_bytes();
        let first = *n.first()?;
        let mut k = 0usize;
        let mut i = 0usize;
        loop {
            if k == 0 {
                // Nothing matched yet: jump to the next candidate start.
                let skip = text.get(i..)?.iter().position(|&b| b == first)?;
                i = i.saturating_add(skip);
            }
            let b = *text.get(i)?;
            if n.get(k) == Some(&b) {
                k = k.saturating_add(1);
                i = i.saturating_add(1);
                if k == n.len() {
                    return Some(i.saturating_sub(k));
                }
            } else if k > 0 {
                k = self.fail.get(k.saturating_sub(1)).copied().unwrap_or(0);
            } else {
                i = i.saturating_add(1);
            }
        }
    }
}

/// Whether `text` holds `parts` in order: the first at its start when
/// `anchored_start`, the last at its end when `anchored_end`, none
/// overlapping.
fn segments_match(text: &str, parts: &[Finder], anchored_start: bool, anchored_end: bool) -> bool {
    let Some((first, rest)) = parts.split_first() else {
        // Only wildcards ("%"), or the empty pattern, which matches only
        // the empty text.
        return !(anchored_start && anchored_end) || text.is_empty();
    };
    if anchored_start && anchored_end && rest.is_empty() {
        return text == first.needle;
    }
    let (mut pos, middle) = if anchored_start {
        if !text.starts_with(first.needle.as_str()) {
            return false;
        }
        (first.needle.len(), rest)
    } else {
        (0, parts)
    };
    let (end, middle) = match (anchored_end, middle.split_last()) {
        (true, Some((last, middle))) => {
            if !text.ends_with(last.needle.as_str()) {
                return false;
            }
            let end = text.len().saturating_sub(last.needle.len());
            if end < pos {
                return false;
            }
            (end, middle)
        }
        _ => (text.len(), middle),
    };
    for part in middle {
        let Some(found) = text.get(pos..end).and_then(|t| part.find(t.as_bytes())) else {
            return false;
        };
        pos = pos.saturating_add(found).saturating_add(part.needle.len());
    }
    true
}

#[cfg(test)]
mod tests;
