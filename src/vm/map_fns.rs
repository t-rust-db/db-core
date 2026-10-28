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
//! #825: `list_contains(l, v)` (aliases `list_has`, `array_contains`,
//! `array_has`) tests a list for a value, DuckDB's answers -- `true` when
//! an element equals `v`, `false` otherwise (a NULL element matches
//! nothing), NULL when `l` or `v` is NULL. A value that is not a list gives
//! NULL, as a non-map does below.
//!
//! #826: on a list, `l[i]` (`map_extract_value`), `list_extract(l, i)` and
//! `list_element(l, i)` read element `i`: 1-based, a negative `i` counts
//! from the end, and 0 or out of range is NULL. `len(l)` and
//! `array_length(l)` count its elements, NULL ones included. DuckDB's
//! `cardinality` takes only a map, so on a list it is NULL here.
//!
//! #827: `list_value(v, ...)` (a list literal `['a', 'b']` parses as it)
//! makes a list of strings and NULLs. `list_has_any(a, b)` / `list_has_all(a,
//! b)` (and their `array_` aliases) test `b`'s non-NULL elements against `a`;
//! `list_position(l, v)` (`list_indexof`, `array_position`, `array_indexof`)
//! is the 1-based position of the first element equal to `v` -- a NULL `v`
//! finds a NULL element -- or NULL when there is none.
//!
//! A NULL map gives NULL. A value that is not a map (or a key that is not a
//! string) gives NULL too: column types are only known per row here.

use super::batch::{ListItems, MapEntries, SlotState, Value};

/// The functions handled here, with their arities.
pub(crate) const MAP_FUNCTIONS: [(&str, usize); 24] = [
    ("list_value", VARIADIC),
    ("list_has_any", 2),
    ("array_has_any", 2),
    ("list_has_all", 2),
    ("array_has_all", 2),
    ("list_position", 2),
    ("list_indexof", 2),
    ("array_position", 2),
    ("array_indexof", 2),
    ("string_split", 2),
    ("str_split", 2),
    ("list_contains", 2),
    ("list_has", 2),
    ("array_contains", 2),
    ("array_has", 2),
    ("list_extract", 2),
    ("list_element", 2),
    ("len", 1),
    ("array_length", 1),
    ("map_extract_value", 2),
    ("map_get_ci", 2),
    ("element_at", 2),
    ("map_keys", 1),
    ("cardinality", 1),
];

/// The arity of a function that takes any number of arguments.
const VARIADIC: usize = usize::MAX;

/// Whether `name(..arity args)` is a map function call -- the parser's
/// and the planner's check of a call by name and arity.
pub(crate) fn is_map_function_call(name: &str, arity: usize) -> bool {
    MAP_FUNCTIONS
        .iter()
        .any(|(f, n)| f.eq_ignore_ascii_case(name) && (*n == arity || *n == VARIADIC))
}

/// Whether `name` (any case) is a map function.
pub(crate) fn is_map_function(name: &str) -> bool {
    MAP_FUNCTIONS
        .iter()
        .any(|(f, _)| f.eq_ignore_ascii_case(name))
}

/// `name(args)` for every row of `columns` (one slice per argument, all
/// `num_rows` long). A split column holds few distinct strings (labels,
/// tags), so `string_split` of two strings is computed once per distinct
/// pair and shared by `Arc` (#815).
pub(crate) fn call_column(name: &str, columns: &[&[Value]], num_rows: usize) -> Vec<Value> {
    if name.eq_ignore_ascii_case("string_split") || name.eq_ignore_ascii_case("str_split") {
        if let [strings, seps] = columns {
            let mut memo: std::collections::HashMap<(&str, &str), Value, SlotState> =
                std::collections::HashMap::default();
            return strings
                .iter()
                .zip(seps.iter())
                .take(num_rows)
                .map(|pair| match pair {
                    (Value::Str(s), Value::Str(sep)) => memo
                        .entry((s.as_ref(), sep.as_ref()))
                        .or_insert_with(|| string_split(pair.0, pair.1))
                        .clone(),
                    (s, sep) => string_split(s, sep),
                })
                .collect();
        }
    }
    (0..num_rows)
        .map(|row| {
            let args: Vec<&Value> = columns.iter().filter_map(|c| c.get(row)).collect();
            call(name, &args)
        })
        .collect()
}

/// `name(args)` for one row.
pub(crate) fn call(name: &str, args: &[&Value]) -> Value {
    if name.eq_ignore_ascii_case("string_split") || name.eq_ignore_ascii_case("str_split") {
        return match args {
            [s, sep] => string_split(s, sep),
            _ => Value::Null,
        };
    }
    if name.eq_ignore_ascii_case("list_value") {
        return list_value(args);
    }
    if is_list_contains(name) {
        return match args {
            [list, value] => list_contains(list, value),
            _ => Value::Null,
        };
    }
    if let Some((Value::List(items), rest)) = args.split_first() {
        return list_call(name, items, rest);
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

/// `name(list, rest...)` for a function that reads a list (#826).
fn list_call(name: &str, items: &ListItems, rest: &[&Value]) -> Value {
    match (name.to_ascii_lowercase().as_str(), rest) {
        ("map_extract_value" | "list_extract" | "list_element", [index]) => {
            list_element(items, index)
        }
        ("len" | "array_length", []) => Value::Int(i64::try_from(items.len()).unwrap_or(i64::MAX)),
        ("list_has_any" | "array_has_any", [Value::List(wanted)]) => {
            Value::Bool(wanted.iter().flatten().any(|w| has(items, w)))
        }
        ("list_has_all" | "array_has_all", [Value::List(wanted)]) => {
            Value::Bool(wanted.iter().flatten().all(|w| has(items, w)))
        }
        ("list_position" | "list_indexof" | "array_position" | "array_indexof", [value]) => {
            list_position(items, value)
        }
        _ => Value::Null,
    }
}

/// Whether `items` holds the non-NULL element `wanted`.
fn has(items: &ListItems, wanted: &str) -> bool {
    items.iter().flatten().any(|item| &**item == wanted)
}

/// The 1-based position of the first element equal to `value` (a NULL
/// `value` finds a NULL element), NULL when there is none.
fn list_position(items: &ListItems, value: &Value) -> Value {
    let value = match value {
        Value::Null => None,
        v => Some(v.to_string()),
    };
    items
        .iter()
        .position(|item| item.as_deref() == value.as_deref())
        .and_then(|at| i64::try_from(at.saturating_add(1)).ok())
        .map_or(Value::Null, Value::Int)
}

/// A list of `values`: strings, NULLs, and any other value as NULL (the
/// parser refuses a literal that is neither, #827).
fn list_value(values: &[&Value]) -> Value {
    Value::List(std::sync::Arc::new(
        values
            .iter()
            .map(|v| match v {
                Value::Str(s) => Some(s.as_ref().into()),
                _ => None,
            })
            .collect(),
    ))
}

/// Element `index` of `items`: 1-based, negative from the end.
fn list_element(items: &ListItems, index: &Value) -> Value {
    let Value::Int(i) = index else {
        return Value::Null;
    };
    let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let at = if *i < 0 {
        len.saturating_add(*i)
    } else {
        i.saturating_sub(1)
    };
    match usize::try_from(at).ok().and_then(|at| items.get(at)) {
        Some(Some(item)) => Value::Str(item.to_string().into()),
        _ => Value::Null,
    }
}

/// Whether `name` (any case) is `list_contains` or one of its aliases.
fn is_list_contains(name: &str) -> bool {
    ["list_contains", "list_has", "array_contains", "array_has"]
        .iter()
        .any(|f| f.eq_ignore_ascii_case(name))
}

fn list_contains(list: &Value, value: &Value) -> Value {
    let Value::List(items) = list else {
        return Value::Null;
    };
    if matches!(value, Value::Null) {
        return Value::Null;
    }
    // A list's elements are strings, so `value` is compared as text.
    let value = value.to_string();
    Value::Bool(
        items
            .iter()
            .any(|item| item.as_deref() == Some(value.as_str())),
    )
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

    // vm_map_fns_call_column_fe1c886e (`call_column`): `name` is `string_split`
    // or `str_split`; a repeated string shares its list, a NULL stays NULL.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_column_fe1c886e__v1_string_split_is_the_first_name() {
        let strings = [s("a,b"), s("a,b"), Value::Null, s("c")];
        let seps = [s(","), s(","), s(","), Value::Null];
        assert_eq!(
            call_column("string_split", &[&strings, &seps], 4),
            [
                list(&["a", "b"]),
                list(&["a", "b"]),
                Value::Null,
                list(&["c"])
            ]
        );
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_column_fe1c886e__v2_str_split_is_the_alias() {
        assert_eq!(
            call_column("str_split", &[&[s("a,b")], &[s(",")]], 1),
            [list(&["a", "b"])]
        );
    }
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_column_fe1c886e__v3_any_other_name_is_a_map_function() {
        assert_eq!(call_column("cardinality", &[&[m()]], 1), [Value::Int(3)]);
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
    fn list_contains_follows_duckdb() {
        let with_null = Value::List(Arc::new(vec![Some("a".into()), None]));
        assert_eq!(
            call("list_contains", &[&list(&["a", "b"]), &s("b")]),
            Value::Bool(true)
        );
        assert_eq!(
            call("list_contains", &[&with_null, &s("c")]),
            Value::Bool(false)
        );
        assert_eq!(
            call("list_contains", &[&list(&[]), &s("a")]),
            Value::Bool(false)
        );
        assert_eq!(call("list_contains", &[&Value::Null, &s("a")]), Value::Null);
        assert_eq!(
            call("list_contains", &[&list(&["a"]), &Value::Null]),
            Value::Null
        );
        assert_eq!(call("list_contains", &[&s("a"), &s("a")]), Value::Null);
        assert_eq!(
            call("list_contains", &[&list(&["1"]), &Value::Int(1)]),
            Value::Bool(true)
        );
        for alias in ["LIST_HAS", "array_contains", "array_has"] {
            assert_eq!(
                call(alias, &[&list(&["a"]), &s("a")]),
                Value::Bool(true),
                "{alias}"
            );
        }
        assert_eq!(
            call_column(
                "list_contains",
                &[&[list(&["a"]), Value::Null], &[s("a"), s("a")]],
                2
            ),
            [Value::Bool(true), Value::Null]
        );
    }

    #[test]
    fn list_element_and_length_follow_duckdb() {
        let l = Value::List(Arc::new(vec![Some("a".into()), None, Some("c".into())]));
        let at = |i: i64| call("map_extract_value", &[&l, &Value::Int(i)]);
        assert_eq!(at(1), s("a"));
        assert_eq!(at(2), Value::Null);
        assert_eq!(at(-1), s("c"));
        assert_eq!(at(-3), s("a"));
        assert_eq!(at(-4), Value::Null);
        assert_eq!(at(0), Value::Null);
        assert_eq!(at(4), Value::Null);
        assert_eq!(at(i64::MIN), Value::Null);
        assert_eq!(call("list_extract", &[&l, &Value::Null]), Value::Null);
        assert_eq!(call("LIST_ELEMENT", &[&l, &s("1")]), Value::Null);
        assert_eq!(
            call("list_extract", &[&Value::Null, &Value::Int(1)]),
            Value::Null
        );
        assert_eq!(call("len", &[&l]), Value::Int(3));
        assert_eq!(call("array_length", &[&list(&[])]), Value::Int(0));
        assert_eq!(call("len", &[&Value::Null]), Value::Null);
        // DuckDB's `cardinality` takes only a map.
        assert_eq!(call("cardinality", &[&l]), Value::Null);
        assert_eq!(call("map_keys", &[&l]), Value::Null);
    }

    #[test]
    fn list_literal_set_and_position_follow_duckdb() {
        let l = Value::List(Arc::new(vec![
            Some("a".into()),
            None,
            Some("b".into()),
            Some("a".into()),
        ]));
        assert_eq!(
            call("list_value", &[&s("a"), &Value::Null]),
            Value::List(Arc::new(vec![Some("a".into()), None]))
        );
        assert_eq!(call("list_value", &[]), list(&[]));
        assert_eq!(call_column("list_value", &[], 2), [list(&[]), list(&[])]);
        // The parser refuses a non-string literal; a value that slips by is NULL.
        assert_eq!(
            call("list_value", &[&Value::Int(1)]),
            Value::List(Arc::new(vec![None]))
        );
        let any = |b: &Value| call("list_has_any", &[&l, b]);
        assert_eq!(any(&list(&["z", "b"])), Value::Bool(true));
        assert_eq!(any(&list(&["z"])), Value::Bool(false));
        assert_eq!(any(&list(&[])), Value::Bool(false));
        assert_eq!(any(&Value::List(Arc::new(vec![None]))), Value::Bool(false));
        assert_eq!(any(&Value::Null), Value::Null);
        let all = |b: &Value| call("array_has_all", &[&l, b]);
        assert_eq!(all(&list(&["a", "b"])), Value::Bool(true));
        assert_eq!(all(&list(&["a", "z"])), Value::Bool(false));
        assert_eq!(all(&list(&[])), Value::Bool(true));
        assert_eq!(all(&Value::List(Arc::new(vec![None]))), Value::Bool(true));
        assert_eq!(
            call("list_has_all", &[&Value::Null, &list(&["a"])]),
            Value::Null
        );
        assert_eq!(
            call("array_has_any", &[&l, &list(&["a"])]),
            Value::Bool(true)
        );
        let pos = |name: &str, v: &Value| call(name, &[&l, v]);
        assert_eq!(pos("list_position", &s("a")), Value::Int(1));
        assert_eq!(pos("list_indexof", &s("b")), Value::Int(3));
        assert_eq!(pos("array_position", &Value::Null), Value::Int(2));
        assert_eq!(pos("array_indexof", &s("z")), Value::Null);
        assert_eq!(call("list_position", &[&Value::Null, &s("a")]), Value::Null);
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
