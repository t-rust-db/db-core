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
//! #828, transforms (each with its `array_` alias where DuckDB has one):
//!
//! | call | result |
//! |---|---|
//! | `array_to_string(l, sep)` | the non-NULL elements joined by `sep` (NULL `sep`: `,`) |
//! | `list_aggr(l, agg[, sep])` | `string_agg`, `count`, `min`, `max`, `first`, `last` of the elements |
//! | `list_distinct(l)` / `list_unique(l)` | the distinct non-NULL elements / their count |
//! | `list_sort(l[, 'ASC'\|'DESC'[, 'NULLS FIRST'\|'NULLS LAST']])` | sorted, NULLs last by default |
//! | `list_reverse_sort(l[, nulls])` / `list_reverse(l)` | sorted descending / reversed |
//! | `list_slice(l, begin, end)` | elements `begin..=end`, 1-based, negative from the end |
//! | `list_concat(a, b)` (`list_cat`, `array_concat`, `array_cat`) | `a` then `b`, a NULL list as empty |
//!
//! `list_aggr`'s aggregate and `list_sort`'s order are string literals,
//! checked at plan time ([`literal_arg_error`]).
//!
//! A NULL map gives NULL. A value that is not a map (or a key that is not a
//! string) gives NULL too: column types are only known per row here.

use std::sync::Arc;

use super::batch::{ListItems, MapEntries, SlotState, Value};

/// The functions handled here, with their arities.
pub(crate) const MAP_FUNCTIONS: &[(&str, usize)] = &[
    ("array_to_string", 2),
    ("list_aggr", 2),
    ("list_aggr", 3),
    ("list_aggregate", 2),
    ("list_aggregate", 3),
    ("list_distinct", 1),
    ("array_distinct", 1),
    ("list_unique", 1),
    ("array_unique", 1),
    ("list_sort", 1),
    ("list_sort", 2),
    ("list_sort", 3),
    ("array_sort", 1),
    ("array_sort", 2),
    ("array_sort", 3),
    ("list_reverse_sort", 1),
    ("list_reverse_sort", 2),
    ("array_reverse_sort", 1),
    ("array_reverse_sort", 2),
    ("list_reverse", 1),
    ("array_reverse", 1),
    ("list_slice", 3),
    ("array_slice", 3),
    ("list_concat", 2),
    ("list_cat", 2),
    ("array_concat", 2),
    ("array_cat", 2),
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
    ("json_keys", 1),
    ("json_keys", 2),
    ("json_array_elements_text", 1),
    ("json_array_elements_text", 2),
];

/// The arity of a function that takes any number of arguments.
const VARIADIC: usize = usize::MAX;

/// Whether `name(..arity args)` is a map function call -- the parser's
/// and the planner's check of a call by name and arity.
#[cfg(any(feature = "parser-column", feature = "codegen-batch"))]
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
    if is_list_concat(name) {
        return match args {
            [a, b] => list_concat(a, b),
            _ => Value::Null,
        };
    }
    if is_list_contains(name) {
        return match args {
            [list, value] => list_contains(list, value),
            _ => Value::Null,
        };
    }
    if name.eq_ignore_ascii_case("json_keys") {
        return json_keys(args);
    }
    if name.eq_ignore_ascii_case("json_array_elements_text") {
        return json_array_elements_text(args);
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

/// `json_keys(json[, path])` (#839): the keys of the object at `path` (the
/// root by default), in document order. NULL for a non-string document,
/// malformed JSON, a missing path or a value that is not an object. The
/// object is found by the batch `json_extract`, so a path indexes arrays.
fn json_keys(args: &[&Value]) -> Value {
    let (Some(Value::Str(doc)), path) = (args.first(), args.get(1)) else {
        return Value::Null;
    };
    let path = match path {
        None => "$",
        Some(Value::Str(p)) => p.as_ref(),
        Some(_) => return Value::Null,
    };
    let text = |s: &str| crate::value::Value::Text(s.to_string().into());
    let Ok(crate::value::Value::Text(object)) =
        crate::functions::late_json_extract(&[text(doc), text(path)])
    else {
        return Value::Null;
    };
    let Some((crate::json_path::JsonValue::Object(fields), _)) =
        crate::json_path::parse_object(&object)
    else {
        return Value::Null;
    };
    Value::List(Arc::new(
        fields
            .iter()
            .map(|(k, _)| Some(Arc::from(crate::json_path::unescape(k).as_ref())))
            .collect(),
    ))
}

/// `json_array_elements_text(json[, path])` (#839): the elements of the
/// array at `path` as a list of text -- what `UNNEST` expands into rows. A
/// string is its text, a number its digits, an object or array its JSON
/// text and a JSON `null` a NULL element. NULL for anything but an array.
/// The row engine's `json_array_length` and `json_extract` do the work.
fn json_array_elements_text(args: &[&Value]) -> Value {
    use crate::value::Value as Scalar;
    let (Some(Value::Str(doc)), path) = (args.first(), args.get(1)) else {
        return Value::Null;
    };
    let path = match path {
        None => "$",
        Some(Value::Str(p)) => p.as_ref(),
        Some(_) => return Value::Null,
    };
    let text = |s: &str| Scalar::Text(s.to_string().into());
    let is_array = matches!(
        crate::functions::call("json_type", &[text(doc), text(path)]),
        Ok(Scalar::Text(t)) if &*t == "array"
    );
    let Ok(Scalar::Integer(len)) =
        crate::functions::call("json_array_length", &[text(doc), text(path)])
    else {
        return Value::Null;
    };
    if !is_array {
        return Value::Null;
    }
    let items: ListItems = (0..len)
        .map(|i| {
            let at = text(&format!("{path}[{i}]"));
            match crate::functions::call("json_extract", &[text(doc), at]) {
                Ok(Scalar::Text(t)) => Some(Arc::from(&*t)),
                Ok(Scalar::Integer(n)) => Some(Arc::from(n.to_string())),
                Ok(Scalar::Real(r)) => Some(Arc::from(r.to_string())),
                _ => None,
            }
        })
        .collect();
    Value::List(Arc::new(items))
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
        ("array_to_string", [sep]) => join(items, sep),
        ("list_aggr" | "list_aggregate", [Value::Str(agg), sep @ ..]) => {
            aggregate(items, agg, sep.first().copied().unwrap_or(&Value::Null))
        }
        ("list_distinct" | "array_distinct", []) => Value::List(std::sync::Arc::new(
            distinct(items)
                .into_iter()
                .map(|s| Some(s.clone()))
                .collect(),
        )),
        ("list_unique" | "array_unique", []) => {
            Value::Int(i64::try_from(distinct(items).len()).unwrap_or(i64::MAX))
        }
        ("list_sort" | "array_sort", order) => match sort_order(order) {
            Some((desc, nulls_first)) => sorted(items, desc, nulls_first),
            None => Value::Null,
        },
        ("list_reverse_sort" | "array_reverse_sort", nulls) => {
            match sort_order(&[
                &Value::Str("DESC".into()),
                nulls.first().copied().unwrap_or(&Value::Null),
            ]) {
                Some((desc, nulls_first)) => sorted(items, desc, nulls_first),
                None => Value::Null,
            }
        }
        ("list_reverse" | "array_reverse", []) => {
            Value::List(std::sync::Arc::new(items.iter().rev().cloned().collect()))
        }
        ("list_slice" | "array_slice", [Value::Int(begin), Value::Int(end)]) => {
            slice(items, *begin, *end)
        }
        _ => Value::Null,
    }
}

/// Whether `name` (any case) is `list_concat` or one of its aliases.
fn is_list_concat(name: &str) -> bool {
    ["list_concat", "list_cat", "array_concat", "array_cat"]
        .iter()
        .any(|f| f.eq_ignore_ascii_case(name))
}

/// `a` then `b`; a NULL list counts as empty, as in DuckDB.
fn list_concat(a: &Value, b: &Value) -> Value {
    match (list_part(a), list_part(b)) {
        (Some(a), Some(b)) => {
            Value::List(std::sync::Arc::new(a.iter().chain(b).cloned().collect()))
        }
        _ => Value::Null,
    }
}

/// A list's elements, a NULL list as none; `None` for a non-list.
fn list_part(v: &Value) -> Option<&[Option<std::sync::Arc<str>>]> {
    match v {
        Value::List(items) => Some(items.as_slice()),
        Value::Null => Some(&[]),
        _ => None,
    }
}

/// A separator argument: NULL is DuckDB's default, `,`.
fn separator(sep: &Value) -> String {
    match sep {
        Value::Null => ",".to_string(),
        sep => sep.to_string(),
    }
}

/// The non-NULL elements joined by `sep`.
fn join(items: &ListItems, sep: &Value) -> Value {
    let parts: Vec<&str> = items.iter().flatten().map(|s| &**s).collect();
    Value::Str(parts.join(&separator(sep)).into())
}

fn text(item: Option<&std::sync::Arc<str>>) -> Value {
    item.map_or(Value::Null, |s| Value::Str(s.to_string().into()))
}

/// `agg` over the elements: the aggregates that make sense of strings.
fn aggregate(items: &ListItems, agg: &str, sep: &Value) -> Value {
    let values = items.iter().flatten();
    match agg.to_ascii_lowercase().as_str() {
        // Like the `string_agg` aggregate: nothing to join is NULL, where
        // `array_to_string` gives ''.
        "string_agg" if items.iter().all(Option::is_none) => Value::Null,
        "string_agg" => join(items, sep),
        "count" => Value::Int(i64::try_from(values.count()).unwrap_or(i64::MAX)),
        "min" => text(values.min()),
        "max" => text(values.max()),
        "first" => text(items.first().and_then(Option::as_ref)),
        "last" => text(items.last().and_then(Option::as_ref)),
        _ => Value::Null,
    }
}

/// The distinct non-NULL elements, in first-seen order.
fn distinct(items: &ListItems) -> Vec<&std::sync::Arc<str>> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .flatten()
        .filter(|s| seen.insert(&***s))
        .collect()
}

/// `list_sort`'s order arguments: (descending, NULLs first). NULLs come
/// last unless asked; an unknown word is `None` (the parser refuses it).
fn sort_order(args: &[&Value]) -> Option<(bool, bool)> {
    let word = |i: usize| match args.get(i) {
        None | Some(Value::Null) => Some(None),
        Some(Value::Str(s)) => Some(Some(s.trim().to_ascii_uppercase())),
        Some(_) => None,
    };
    let desc = match word(0)?.as_deref() {
        None | Some("ASC") => false,
        Some("DESC") => true,
        Some(_) => return None,
    };
    let nulls_first = match word(1)?.as_deref() {
        None | Some("NULLS LAST") => false,
        Some("NULLS FIRST") => true,
        Some(_) => return None,
    };
    Some((desc, nulls_first))
}

fn sorted(items: &ListItems, desc: bool, nulls_first: bool) -> Value {
    let mut values: Vec<&std::sync::Arc<str>> = items.iter().flatten().collect();
    values.sort();
    if desc {
        values.reverse();
    }
    let nulls = std::iter::repeat_n(None, items.len().saturating_sub(values.len()));
    let values = values.into_iter().map(|s| Some(s.clone()));
    Value::List(std::sync::Arc::new(if nulls_first {
        nulls.chain(values).collect()
    } else {
        values.chain(nulls).collect()
    }))
}

/// Elements `begin..=end`, 1-based; a negative bound counts from the end,
/// and the range is clipped to the list.
fn slice(items: &ListItems, begin: i64, end: i64) -> Value {
    let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let from_end = |i: i64| {
        if i < 0 {
            len.saturating_add(i).saturating_add(1)
        } else {
            i
        }
    };
    let begin = from_end(begin).max(1);
    let end = from_end(end).min(len);
    let part = usize::try_from(begin.saturating_sub(1))
        .ok()
        .zip(usize::try_from(end).ok())
        .and_then(|(b, e)| items.get(b..e))
        .unwrap_or(&[]);
    Value::List(std::sync::Arc::new(part.to_vec()))
}

/// Why literal argument `position` of `name` is not usable, for the
/// plan-time check (#828): `list_aggr`'s aggregate and `list_sort`'s
/// order must be string literals DuckDB accepts here. `value` is the
/// argument when it is a string literal. The path of a JSON function
/// (#839) is checked by the row engine's own path parser, and only when it
/// is a literal. Like sqlite3's it reads the path as far as the document
/// goes, so against `{}` it refuses a path that does not start with `$`
/// and leaves a malformed tail (`$.a[`) to give NULL.
#[cfg(feature = "parser-column")]
pub(crate) fn literal_arg_error(
    name: &str,
    position: usize,
    value: Option<&str>,
) -> Option<String> {
    if position == 1
        && matches!(
            name.to_ascii_lowercase().as_str(),
            "json_extract"
                | "json_extract_string"
                | "json_array_length"
                | "json_type"
                | "json_keys"
        )
    {
        let doc = crate::value::Value::Text("{}".into());
        let path = crate::value::Value::Text(value?.into());
        return match crate::functions::call("json_extract", &[doc, path]) {
            Err(crate::functions::FunctionError::Sql(why)) => Some(format!("{name}: {why}")),
            _ => None,
        };
    }
    let allowed: &[&str] = match (name.to_ascii_lowercase().as_str(), position) {
        ("list_aggr" | "list_aggregate", 1) => {
            &["string_agg", "count", "min", "max", "first", "last"]
        }
        ("list_sort" | "array_sort", 1) => &["asc", "desc"],
        ("list_sort" | "array_sort", 2) | ("list_reverse_sort" | "array_reverse_sort", 1) => {
            &["nulls first", "nulls last"]
        }
        _ => return None,
    };
    let ok = value.is_some_and(|v| allowed.contains(&v.trim().to_ascii_lowercase().as_str()));
    (!ok).then(|| {
        format!(
            "{name}: argument {} must be one of {}",
            position.saturating_add(1),
            allowed.join(", ")
        )
    })
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
    fn list_transforms_follow_duckdb() {
        let l = Value::List(Arc::new(vec![
            Some("b".into()),
            None,
            Some("a".into()),
            Some("b".into()),
        ]));
        assert_eq!(call("array_to_string", &[&l, &s("-")]), s("b-a-b"));
        assert_eq!(call("array_to_string", &[&l, &Value::Null]), s("b,a,b"));
        assert_eq!(call("array_to_string", &[&list(&[]), &s(",")]), s(""));
        let agg = |a: &str| call("list_aggr", &[&l, &s(a)]);
        assert_eq!(agg("string_agg"), s("b,a,b"));
        assert_eq!(
            call("list_aggr", &[&l, &s("string_agg"), &s("|")]),
            s("b|a|b")
        );
        assert_eq!(
            call("list_aggr", &[&list(&[]), &s("string_agg")]),
            Value::Null
        );
        assert_eq!(agg("COUNT"), Value::Int(3));
        assert_eq!(agg("min"), s("a"));
        assert_eq!(agg("max"), s("b"));
        assert_eq!(agg("first"), s("b"));
        assert_eq!(agg("last"), s("b"));
        assert_eq!(agg("avg"), Value::Null);
        assert_eq!(call("list_aggr", &[&l, &Value::Null]), Value::Null);
        assert_eq!(call("list_distinct", &[&l]), list(&["b", "a"]));
        assert_eq!(call("list_unique", &[&l]), Value::Int(2));
        let with_null = |v: &[Option<&str>]| {
            Value::List(Arc::new(v.iter().map(|x| x.map(Arc::from)).collect()))
        };
        assert_eq!(
            call("list_sort", &[&l]),
            with_null(&[Some("a"), Some("b"), Some("b"), None])
        );
        assert_eq!(
            call("list_sort", &[&l, &s("desc"), &s(" nulls first ")]),
            with_null(&[None, Some("b"), Some("b"), Some("a")])
        );
        assert_eq!(
            call("list_reverse_sort", &[&l]),
            with_null(&[Some("b"), Some("b"), Some("a"), None])
        );
        assert_eq!(call("list_sort", &[&l, &s("sideways")]), Value::Null);
        assert_eq!(
            call("list_sort", &[&l, &s("ASC"), &s("middle")]),
            Value::Null
        );
        assert_eq!(call("list_sort", &[&l, &Value::Int(1)]), Value::Null);
        assert_eq!(
            call("list_reverse", &[&l]),
            with_null(&[Some("b"), Some("a"), None, Some("b")])
        );
        let abcd = list(&["a", "b", "c", "d"]);
        let sl = |b: i64, e: i64| call("list_slice", &[&abcd, &Value::Int(b), &Value::Int(e)]);
        assert_eq!(sl(2, 3), list(&["b", "c"]));
        assert_eq!(sl(-2, -1), list(&["c", "d"]));
        assert_eq!(sl(0, 2), list(&["a", "b"]));
        assert_eq!(sl(3, 10), list(&["c", "d"]));
        assert_eq!(sl(3, 2), list(&[]));
        assert_eq!(sl(1, -10), list(&[]));
        assert_eq!(sl(i64::MIN, i64::MAX), abcd);
        assert_eq!(
            call("list_slice", &[&abcd, &Value::Null, &Value::Int(2)]),
            Value::Null
        );
        assert_eq!(
            call("list_concat", &[&list(&["a"]), &list(&["b"])]),
            list(&["a", "b"])
        );
        assert_eq!(call("array_cat", &[&Value::Null, &Value::Null]), list(&[]));
        assert_eq!(call("list_cat", &[&s("a"), &list(&["b"])]), Value::Null);
    }

    #[test]
    #[cfg(feature = "parser-column")]
    fn literal_args_are_checked() {
        assert_eq!(literal_arg_error("list_aggr", 1, Some("String_Agg")), None);
        assert!(literal_arg_error("list_aggr", 1, Some("avg")).is_some());
        assert!(literal_arg_error("list_aggregate", 1, None).is_some());
        assert_eq!(literal_arg_error("list_sort", 1, Some("desc")), None);
        assert!(literal_arg_error("array_sort", 2, Some("desc")).is_some());
        assert_eq!(
            literal_arg_error("list_reverse_sort", 1, Some("NULLS FIRST")),
            None
        );
        assert_eq!(literal_arg_error("list_sort", 0, None), None);
        // #839: a JSON path literal is checked, a computed one is not.
        assert_eq!(literal_arg_error("json_extract", 1, Some("$.a[0]")), None);
        assert_eq!(literal_arg_error("json_keys", 1, Some("$.a[#-1]")), None);
        assert!(literal_arg_error("json_type", 1, Some("a.b")).is_some());
        assert!(literal_arg_error("json_extract", 1, Some("")).is_some());
        assert_eq!(literal_arg_error("json_extract", 1, None), None);
        assert_eq!(literal_arg_error("string_split", 1, None), None);
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
