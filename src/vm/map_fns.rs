//! Map functions of the batch VM (#621), over [`Value::Map`]:
//!
//! | call | result |
//! |---|---|
//! | `map_extract_value(m, k)` (also `m['k']`) | `m`'s value for key `k`, NULL when absent |
//! | `map_get_ci(m, k)` | the same with keys compared case-insensitively (HTTP header names), first match |
//! | `element_at(m, k)` | DuckDB's one-element list: `[v]`, `[]` when absent, as text |
//! | `map_keys(m)` | the keys as DuckDB prints a list: `[a, B]` |
//! | `cardinality(m)` | the number of entries |
//!
//! #803: `string_split(s, sep)` (alias `str_split`) is here too, as the one
//! function that makes a list: `s` split at each `sep`, DuckDB's answers --
//! `''` gives `['']`, a NULL `s` gives NULL, a NULL `sep` the whole string as
//! one element, an empty `sep` one element per character.
//!
//! A NULL map gives NULL. A value that is not a map (or a key that is not a
//! string) gives NULL too: column types are only known per row here.

use super::batch::{MapEntries, Value};

/// The functions handled here, with their arities.
pub(crate) const MAP_FUNCTIONS: [(&str, usize); 7] = [
    ("string_split", 2),
    ("str_split", 2),
    ("map_extract_value", 2),
    ("map_get_ci", 2),
    ("element_at", 2),
    ("map_keys", 1),
    ("cardinality", 1),
];

/// Whether `name` (any case) is a map function.
pub(crate) fn is_map_function(name: &str) -> bool {
    MAP_FUNCTIONS
        .iter()
        .any(|(f, _)| f.eq_ignore_ascii_case(name))
}

/// `name(args)` for one row.
pub(crate) fn call(name: &str, args: &[&Value]) -> Value {
    if name.eq_ignore_ascii_case("string_split") || name.eq_ignore_ascii_case("str_split") {
        return match args {
            [s, sep] => string_split(s, sep),
            _ => Value::Null,
        };
    }
    let (Some(Value::Map(entries)), key) = (args.first(), args.get(1)) else {
        return Value::Null;
    };
    let key = match key {
        Some(Value::Str(k)) => Some(k.as_ref()),
        Some(_) => return Value::Null,
        None => None,
    };
    match (name.to_ascii_lowercase().as_str(), key) {
        ("map_extract_value", Some(k)) => value_of(lookup(entries, |e| e == k)),
        ("map_get_ci", Some(k)) => {
            let k = k.to_lowercase();
            value_of(lookup(entries, |e| e.to_lowercase() == k))
        }
        ("element_at", Some(k)) => {
            let text = match lookup(entries, |e| e == k) {
                None => "[]".to_string(),
                Some(Some(v)) => format!("[{v}]"),
                Some(None) => "[NULL]".to_string(),
            };
            Value::Str(text.into())
        }
        ("map_keys", None) => {
            let keys: Vec<&str> = entries.iter().map(|(k, _)| &**k).collect();
            Value::Str(format!("[{}]", keys.join(", ")).into())
        }
        ("cardinality", None) => Value::Int(i64::try_from(entries.len()).unwrap_or(i64::MAX)),
        _ => Value::Null,
    }
}

fn string_split(s: &Value, sep: &Value) -> Value {
    if matches!(s, Value::Null) {
        return Value::Null;
    }
    let s = s.to_string();
    let items: Vec<Option<std::sync::Arc<str>>> = match sep {
        Value::Null => vec![Some(s.as_str().into())],
        sep => match sep.to_string() {
            sep if sep.is_empty() && s.is_empty() => vec![Some("".into())],
            sep if sep.is_empty() => s.chars().map(|c| Some(c.to_string().into())).collect(),
            sep => s
                .split(sep.as_str())
                .map(|part| Some(part.into()))
                .collect(),
        },
    };
    Value::List(std::sync::Arc::new(items))
}

/// The value of the first entry whose key matches: `None` when no key
/// does, `Some(None)` for a NULL value.
fn lookup(entries: &MapEntries, matches: impl Fn(&str) -> bool) -> Option<Option<&str>> {
    entries
        .iter()
        .find(|(k, _)| matches(k))
        .map(|(_, v)| v.as_deref())
}

fn value_of(found: Option<Option<&str>>) -> Value {
    match found {
        Some(Some(v)) => Value::Str(v.to_string().into()),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn m() -> Value {
        Value::Map(Arc::new(vec![
            ("User-Agent".into(), Some("curl/8".into())),
            ("B".into(), None),
            ("ä".into(), Some("x".into())),
        ]))
    }

    fn s(v: &str) -> Value {
        Value::Str(v.to_string().into())
    }

    #[test]
    fn lookups_follow_duckdb() {
        let m = m();
        assert_eq!(
            call("map_extract_value", &[&m, &s("User-Agent")]),
            s("curl/8")
        );
        assert_eq!(
            call("map_extract_value", &[&m, &s("user-agent")]),
            Value::Null
        );
        assert_eq!(call("map_extract_value", &[&m, &s("B")]), Value::Null);
        assert_eq!(call("map_extract_value", &[&m, &s("zz")]), Value::Null);
        assert_eq!(call("element_at", &[&m, &s("User-Agent")]), s("[curl/8]"));
        assert_eq!(call("element_at", &[&m, &s("B")]), s("[NULL]"));
        assert_eq!(call("element_at", &[&m, &s("zz")]), s("[]"));
        assert_eq!(call("map_keys", &[&m]), s("[User-Agent, B, ä]"));
        assert_eq!(call("CARDINALITY", &[&m]), Value::Int(3));
    }

    fn list(items: &[&str]) -> Value {
        Value::List(Arc::new(
            items.iter().map(|v| Some(Arc::from(*v))).collect(),
        ))
    }

    #[test]
    fn string_split_follows_duckdb() {
        let split = |s: &Value, sep: &Value| call("string_split", &[s, sep]);
        assert_eq!(split(&s("a,b"), &s(",")), list(&["a", "b"]));
        assert_eq!(split(&s(""), &s(",")), list(&[""]));
        assert_eq!(split(&Value::Null, &s(",")), Value::Null);
        assert_eq!(split(&s("abc"), &s(",")), list(&["abc"]));
        assert_eq!(split(&s(",a,"), &s(",")), list(&["", "a", ""]));
        assert_eq!(split(&s("a,,b"), &s(",")), list(&["a", "", "b"]));
        assert_eq!(split(&s("aXYbXYc"), &s("XY")), list(&["a", "b", "c"]));
        assert_eq!(split(&s("äb"), &s("")), list(&["ä", "b"]));
        assert_eq!(split(&s(""), &s("")), list(&[""]));
        assert_eq!(split(&s("abc"), &Value::Null), list(&["abc"]));
        assert_eq!(split(&s("abc"), &s("abc")), list(&["", ""]));
        assert_eq!(call("STR_SPLIT", &[&s("a b"), &s(" ")]), list(&["a", "b"]));
    }

    // vm_map_fns_call_fe1c886e (`call`): `name` is `string_split` or `str_split`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_fe1c886e__v1_string_split_is_the_first_name() {
        assert_eq!(
            call("string_split", &[&s("a,b"), &s(",")]),
            list(&["a", "b"])
        );
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_fe1c886e__v2_str_split_is_the_alias() {
        assert_eq!(call("str_split", &[&s("a,b"), &s(",")]), list(&["a", "b"]));
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_fe1c886e__v3_any_other_name_is_a_map_function() {
        assert_eq!(call("cardinality", &[&m()]), Value::Int(3));
    }

    // vm_map_fns_string_split_e1a5eb72 (`string_split`): `sep.is_empty() && s.is_empty()`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_string_split_e1a5eb72__v1_empty_by_empty_is_one_empty_element() {
        assert_eq!(string_split(&s(""), &s("")), list(&[""]));
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_string_split_e1a5eb72__v2_empty_separator_splits_into_characters() {
        assert_eq!(string_split(&s("ab"), &s("")), list(&["a", "b"]));
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_string_split_e1a5eb72__v3_empty_string_by_a_separator_is_one_empty_element()
    {
        assert_eq!(string_split(&s(""), &s(",")), list(&[""]));
    }

    #[test]
    fn map_get_ci_ignores_case() {
        let m = m();
        assert_eq!(call("map_get_ci", &[&m, &s("USER-AGENT")]), s("curl/8"));
        assert_eq!(call("map_get_ci", &[&m, &s("Ä")]), s("x"));
        assert_eq!(call("map_get_ci", &[&m, &s("nope")]), Value::Null);
    }

    #[test]
    fn null_and_non_map_arguments_give_null() {
        let m = m();
        assert_eq!(
            call("map_extract_value", &[&Value::Null, &s("a")]),
            Value::Null
        );
        assert_eq!(call("cardinality", &[&s("{}")]), Value::Null);
        assert_eq!(
            call("map_extract_value", &[&m, &Value::Int(1)]),
            Value::Null
        );
        assert_eq!(call("map_extract_value", &[&m, &Value::Null]), Value::Null);
        assert_eq!(call("cardinality", &[&m, &s("extra")]), Value::Null);
        assert!(is_map_function("Map_Get_CI"));
        assert!(!is_map_function("json_extract"));
    }
}
