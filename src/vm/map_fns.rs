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
//! nothing), NULL when `l` or `v` is NULL. A value that is not a list is
//! a conversion error, as a non-map is below.
//!
//! #826: on a list, `l[i]` (`map_extract_value`), `list_extract(l, i)` and
//! `list_element(l, i)` read element `i`: 1-based, a negative `i` counts
//! from the end, and 0 or out of range is NULL. `len(l)` and
//! `array_length(l)` count its elements, NULL ones included; on a string,
//! `len` is its length in characters. DuckDB's `cardinality` takes only a
//! map, so on a list it is an error.
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
//! A NULL map, list or argument gives NULL. A value of the wrong type -- a
//! string where a list belongs, a list where a map does, an index that is
//! not an integer -- is a [`VmError::Conversion`] in DuckDB's words (#892):
//! column types are only known per row here, so the error comes with the
//! first row that has one, where DuckDB refuses the query when binding.
//! Index and slice arguments are cast as DuckDB casts them: integer text
//! parses, and a slice bound may be a float, rounded half away from zero.

use std::sync::Arc;

use super::batch::{ListItems, MapEntries, Result, SlotState, Value, VmError};

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
pub(crate) fn call_column(name: &str, columns: &[&[Value]], num_rows: usize) -> Result<Vec<Value>> {
    if name.eq_ignore_ascii_case("string_split") || name.eq_ignore_ascii_case("str_split") {
        if let [strings, seps] = columns {
            let mut memo: std::collections::HashMap<(&str, &str), Value, SlotState> =
                std::collections::HashMap::default();
            return Ok(strings
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
                .collect());
        }
    }
    (0..num_rows)
        .map(|row| {
            let args: Vec<&Value> = columns.iter().filter_map(|c| c.get(row)).collect();
            call(name, &args)
        })
        .collect::<Result<_>>()
}

/// `name(args)` for one row.
pub(crate) fn call(name: &str, args: &[&Value]) -> Result<Value> {
    if name.eq_ignore_ascii_case("string_split") || name.eq_ignore_ascii_case("str_split") {
        return match args {
            [s, sep] => Ok(string_split(s, sep)),
            _ => Err(arity(name, args)),
        };
    }
    if name.eq_ignore_ascii_case("list_value") {
        return Ok(list_value(args));
    }
    if is_list_concat(name) {
        return match args {
            [a, b] => list_concat(a, b),
            _ => Err(arity(name, args)),
        };
    }
    if is_list_contains(name) {
        return match args {
            [list, value] => list_contains(list, value),
            _ => Err(arity(name, args)),
        };
    }
    if name.eq_ignore_ascii_case("json_keys") {
        return json_keys(args);
    }
    if name.eq_ignore_ascii_case("json_array_elements_text") {
        return json_array_elements_text(args);
    }
    let Some((first, rest)) = args.split_first() else {
        return Err(arity(name, args));
    };
    let entries = match first {
        Value::Null => return Ok(Value::Null),
        Value::List(items) => return list_call(name, first, items, rest),
        Value::Str(s) if name.eq_ignore_ascii_case("len") && rest.is_empty() => {
            return Ok(Value::Int(
                i64::try_from(s.chars().count()).unwrap_or(i64::MAX),
            ));
        }
        Value::Map(entries) => entries,
        other => return Err(not_a(other, wanted_type(name))),
    };
    let key = match rest {
        [] => None,
        [Value::Str(k)] => Some(k.as_ref()),
        [Value::Null] => return Ok(Value::Null),
        [other] => return Err(not_a(other, "VARCHAR")),
        _ => return Err(arity(name, args)),
    };
    Ok(match (name.to_ascii_lowercase().as_str(), key) {
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
        // A map function with the wrong argument count; any other
        // function here reads a list, not a map.
        _ if wanted_type(name) == "MAP" || name.eq_ignore_ascii_case("map_extract_value") => {
            return Err(arity(name, args))
        }
        _ => return Err(not_a(first, "VARCHAR[]")),
    })
}

/// The type `name`'s first argument must have: a map for the map
/// functions, a list for the rest.
fn wanted_type(name: &str) -> &'static str {
    let map_only = ["map_get_ci", "element_at", "map_keys", "cardinality"];
    if map_only.iter().any(|f| f.eq_ignore_ascii_case(name)) {
        "MAP"
    } else {
        "VARCHAR[]"
    }
}

/// `value` cannot be read as `to` (#892): DuckDB's conversion error.
fn not_a(value: &Value, to: &'static str) -> VmError {
    // A float keeps its point: `1.0`, not the `1` it displays as, which
    // would read as a convertible integer.
    let value = match value {
        Value::Float(f) => format!("{f:?}"),
        other => other.to_string(),
    };
    VmError::Conversion { value, to }
}

/// A call whose argument count the parser should have refused.
fn arity(name: &str, args: &[&Value]) -> VmError {
    VmError::MalformedProgram {
        opcode: "Function",
        reason: format!("{name} called with {} arguments", args.len()),
    }
}

/// A list argument: `None` for NULL, a conversion error for a non-list.
fn list_arg(value: &Value) -> Result<Option<&ListItems>> {
    match value {
        Value::Null => Ok(None),
        Value::List(items) => Ok(Some(items)),
        other => Err(not_a(other, "VARCHAR[]")),
    }
}

/// An index argument: an integer, or text DuckDB casts to one; `None` for
/// NULL.
fn index_arg(value: &Value) -> Result<Option<i64>> {
    match value {
        Value::Null => Ok(None),
        Value::Int(i) => Ok(Some(*i)),
        Value::Str(s) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| not_a(value, "INT64")),
        other => Err(not_a(other, "INT64")),
    }
}

/// A slice bound: an index, or a float rounded half away from zero as
/// DuckDB casts `DOUBLE` to `BIGINT`.
fn bound_arg(value: &Value) -> Result<Option<i64>> {
    // 2^63 as an `f64`: every rounded float below it in magnitude fits.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    match value {
        Value::Float(f) => {
            let r = f.round();
            if r.is_finite() && r > -LIMIT && r < LIMIT {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "`r` is integral and inside `i64`'s range, checked above"
                )]
                Ok(Some(r as i64))
            } else {
                Err(not_a(value, "INT64"))
            }
        }
        other => index_arg(other),
    }
}

/// `json_keys(json[, path])` (#839): the keys of the object at `path` (the
/// root by default), in document order; `[]` for a value that is not an
/// object, as in DuckDB. NULL for a non-string document, malformed JSON or
/// a missing path. The value is found by the batch `json_extract`, so a
/// path indexes arrays.
fn json_keys(args: &[&Value]) -> Result<Value> {
    let Some((doc, path)) = json_args(args)? else {
        return Ok(Value::Null);
    };
    let (doc, path) = (doc.as_str(), path.as_str());
    let text = |s: &str| crate::value::Value::Text(s.to_string().into());
    let Ok(crate::value::Value::Text(object)) =
        crate::functions::late_json_extract(&[text(doc), text(path)])
    else {
        return Ok(Value::Null);
    };
    let Some((crate::json_path::JsonValue::Object(fields), _)) =
        crate::json_path::parse_object(&object)
    else {
        return Ok(Value::List(Arc::new(Vec::new())));
    };
    Ok(Value::List(Arc::new(
        fields
            .iter()
            .map(|(k, _)| Some(Arc::from(crate::json_path::unescape(k).as_ref())))
            .collect(),
    )))
}

/// The `(json, path)` arguments of the JSON list functions: `None` when
/// either is NULL, the root `$` when there is no path, and a conversion
/// error for a document or path that is not text.
fn json_args(args: &[&Value]) -> Result<Option<(String, String)>> {
    let text = |v: &Value| match v {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(s.to_string())),
        other => Err(not_a(other, "VARCHAR")),
    };
    let doc = match args.first() {
        Some(doc) => text(doc)?,
        None => return Err(arity("json", args)),
    };
    let path = match args.get(1) {
        Some(path) => text(path)?,
        None => Some("$".to_string()),
    };
    Ok(doc.zip(path))
}

/// `json_array_elements_text(json[, path])` (#839): the elements of the
/// array at `path` as a list of text -- what `UNNEST` expands into rows. A
/// string is its text, a number its digits, an object or array its JSON
/// text and a JSON `null` a NULL element. NULL for anything but an array.
/// The row engine's `json_array_length` and `json_extract` do the work.
fn json_array_elements_text(args: &[&Value]) -> Result<Value> {
    use crate::value::Value as Scalar;
    let Some((doc, path)) = json_args(args)? else {
        return Ok(Value::Null);
    };
    let (doc, path) = (doc.as_str(), path.as_str());
    let text = |s: &str| Scalar::Text(s.to_string().into());
    let is_array = matches!(
        crate::functions::call("json_type", &[text(doc), text(path)]),
        Ok(Scalar::Text(t)) if &*t == "array"
    );
    let Ok(Scalar::Integer(len)) =
        crate::functions::call("json_array_length", &[text(doc), text(path)])
    else {
        return Ok(Value::Null);
    };
    if !is_array {
        return Ok(Value::Null);
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
    Ok(Value::List(Arc::new(items)))
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

/// `name(list, rest...)` for a function that reads a list (#826); a map
/// function given the list is a conversion error.
fn list_call(name: &str, list: &Value, items: &ListItems, rest: &[&Value]) -> Result<Value> {
    Ok(match (name.to_ascii_lowercase().as_str(), rest) {
        ("map_extract_value" | "list_extract" | "list_element", [index]) => {
            index_arg(index)?.map_or(Value::Null, |i| list_element(items, i))
        }
        ("len" | "array_length", []) => Value::Int(i64::try_from(items.len()).unwrap_or(i64::MAX)),
        ("list_has_any" | "array_has_any", [wanted]) => list_arg(wanted)?
            .map_or(Value::Null, |w| {
                Value::Bool(w.iter().flatten().any(|w| has(items, w)))
            }),
        ("list_has_all" | "array_has_all", [wanted]) => list_arg(wanted)?
            .map_or(Value::Null, |w| {
                Value::Bool(w.iter().flatten().all(|w| has(items, w)))
            }),
        ("list_position" | "list_indexof" | "array_position" | "array_indexof", [value]) => {
            list_position(items, value)
        }
        ("array_to_string", [sep]) => join(items, sep),
        ("list_aggr" | "list_aggregate", [agg, sep @ ..]) => {
            aggregate(items, agg, sep.first().copied().unwrap_or(&Value::Null))?
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
        ("list_sort" | "array_sort", order) => {
            let (desc, nulls_first) = sort_order(order)?;
            sorted(items, desc, nulls_first)
        }
        ("list_reverse_sort" | "array_reverse_sort", nulls) => {
            let (desc, nulls_first) = sort_order(&[
                &Value::Str("DESC".into()),
                nulls.first().copied().unwrap_or(&Value::Null),
            ])?;
            sorted(items, desc, nulls_first)
        }
        ("list_reverse" | "array_reverse", []) => {
            Value::List(std::sync::Arc::new(items.iter().rev().cloned().collect()))
        }
        ("list_slice" | "array_slice", [begin, end]) => {
            match (bound_arg(begin)?, bound_arg(end)?) {
                (Some(begin), Some(end)) => slice(items, begin, end),
                _ => Value::Null,
            }
        }
        _ => return Err(not_a(list, "MAP")),
    })
}

/// Whether `name` (any case) is `list_concat` or one of its aliases.
fn is_list_concat(name: &str) -> bool {
    ["list_concat", "list_cat", "array_concat", "array_cat"]
        .iter()
        .any(|f| f.eq_ignore_ascii_case(name))
}

/// `a` then `b`; a NULL list counts as empty, as in DuckDB.
fn list_concat(a: &Value, b: &Value) -> Result<Value> {
    let (a, b) = (list_part(a)?, list_part(b)?);
    Ok(Value::List(std::sync::Arc::new(
        a.iter().chain(b).cloned().collect(),
    )))
}

/// A list's elements, a NULL list as none; a conversion error for a
/// non-list.
fn list_part(v: &Value) -> Result<&[Option<std::sync::Arc<str>>]> {
    Ok(list_arg(v)?.map_or(&[][..], |items| items.as_slice()))
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
/// Any other name is DuckDB's "no such aggregate" error; the parser
/// already refuses one written as a literal.
fn aggregate(items: &ListItems, agg: &Value, sep: &Value) -> Result<Value> {
    let values = items.iter().flatten();
    let unknown = || VmError::UnsupportedOp {
        opcode: "list_aggr",
        op: format!("aggregate {agg}"),
    };
    let Value::Str(name) = agg else {
        return Err(unknown());
    };
    Ok(match name.to_ascii_lowercase().as_str() {
        // Like the `string_agg` aggregate: nothing to join is NULL, where
        // `array_to_string` gives ''.
        "string_agg" if items.iter().all(Option::is_none) => Value::Null,
        "string_agg" => join(items, sep),
        "count" => Value::Int(i64::try_from(values.count()).unwrap_or(i64::MAX)),
        "min" => text(values.min()),
        "max" => text(values.max()),
        "first" => text(items.first().and_then(Option::as_ref)),
        "last" => text(items.last().and_then(Option::as_ref)),
        _ => return Err(unknown()),
    })
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
/// last unless asked; an unknown word is an error (the parser refuses one
/// written as a literal).
fn sort_order(args: &[&Value]) -> Result<(bool, bool)> {
    let word = |i: usize| match args.get(i) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.trim().to_ascii_uppercase())),
        Some(other) => Err(not_a(other, "sort order")),
    };
    let unknown = |w: &str| VmError::Conversion {
        value: w.to_string(),
        to: "sort order",
    };
    let desc = match word(0)?.as_deref() {
        None | Some("ASC") => false,
        Some("DESC") => true,
        Some(w) => return Err(unknown(w)),
    };
    let nulls_first = match word(1)?.as_deref() {
        None | Some("NULLS LAST") => false,
        Some("NULLS FIRST") => true,
        Some(w) => return Err(unknown(w)),
    };
    Ok((desc, nulls_first))
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
/// argument when it is a string literal.
#[cfg(feature = "parser-column")]
pub(crate) fn literal_arg_error(
    name: &str,
    position: usize,
    value: Option<&str>,
) -> Option<String> {
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

/// Element `i` of `items`: 1-based, negative from the end.
fn list_element(items: &ListItems, i: i64) -> Value {
    let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let at = if i < 0 {
        len.saturating_add(i)
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

fn list_contains(list: &Value, value: &Value) -> Result<Value> {
    let Some(items) = list_arg(list)? else {
        return Ok(Value::Null);
    };
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    // A list's elements are strings, so `value` is compared as text.
    let value = value.to_string();
    Ok(Value::Bool(
        items
            .iter()
            .any(|item| item.as_deref() == Some(value.as_str())),
    ))
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

    /// The row's value; tests that expect an error call [`super::call`].
    fn call(name: &str, args: &[&Value]) -> Value {
        super::call(name, args).unwrap()
    }

    /// Every row's value.
    fn call_column(name: &str, columns: &[&[Value]], num_rows: usize) -> Vec<Value> {
        super::call_column(name, columns, num_rows).unwrap()
    }

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

    fn bound(f: f64) -> Result<Option<i64>> {
        bound_arg(&Value::Float(f))
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_bound_arg_6e257dc1__v1_a_finite_float_in_range_rounds() {
        assert_eq!(bound(2.5).unwrap(), Some(3));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_bound_arg_6e257dc1__v2_nan_is_an_error() {
        assert!(bound(f64::NAN).is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_bound_arg_6e257dc1__v3_below_i64_is_an_error() {
        assert!(bound(-1e19).is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_bound_arg_6e257dc1__v4_above_i64_is_an_error() {
        assert!(bound(1e19).is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_95857d66__v1_len_of_a_string_counts_characters() {
        assert_eq!(call("LEN", &[&s("héllo")]), Value::Int(5));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_95857d66__v2_another_function_on_a_string_is_an_error() {
        assert!(matches!(
            super::call("cardinality", &[&s("x")]),
            Err(VmError::Conversion { to: "MAP", .. })
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_95857d66__v3_len_with_more_arguments_is_an_error() {
        assert!(super::call("len", &[&s("abc"), &s("x")]).is_err());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_b8ba15a0__v1_a_map_function_with_a_wrong_count() {
        assert!(matches!(
            super::call("cardinality", &[&m(), &s("extra")]),
            Err(VmError::MalformedProgram { .. })
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_b8ba15a0__v2_map_extract_value_without_a_key() {
        assert!(matches!(
            super::call("map_extract_value", &[&m()]),
            Err(VmError::MalformedProgram { .. })
        ));
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_map_fns_call_b8ba15a0__v3_a_list_function_given_a_map() {
        assert!(matches!(
            super::call("list_sort", &[&m()]),
            Err(VmError::Conversion {
                to: "VARCHAR[]",
                ..
            })
        ));
    }

    /// #892: slice bounds cast as DuckDB casts them -- integer text
    /// parses, a float rounds half away from zero -- and anything else is
    /// an error rather than NULL.
    #[test]
    fn list_slice_bounds_cast_like_duckdb() {
        let abcd = list(&["a", "b", "c", "d"]);
        let sl = |b: Value, e: Value| super::call("list_slice", &[&abcd, &b, &e]);
        assert_eq!(sl(s("1"), s("2")).unwrap(), list(&["a", "b"]));
        assert_eq!(sl(Value::Float(2.5), Value::Int(3)).unwrap(), list(&["c"]));
        assert_eq!(
            sl(Value::Float(-1.5), Value::Int(4)).unwrap(),
            list(&["c", "d"])
        );
        assert!(sl(s("x"), Value::Int(2)).is_err());
        assert!(sl(Value::Float(f64::NAN), Value::Int(2)).is_err());
        assert!(sl(Value::Float(1e19), Value::Int(2)).is_err());
        assert!(sl(Value::Bool(true), Value::Int(2)).is_err());
        assert!(super::call("list_has_any", &[&abcd, &s("x")]).is_err());
        assert_eq!(call("list_has_any", &[&abcd, &Value::Null]), Value::Null);
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
        // Text casts to an index as DuckDB casts it; other types do not.
        assert_eq!(call("LIST_ELEMENT", &[&l, &s("1")]), s("a"));
        assert!(super::call("list_extract", &[&l, &s("x")]).is_err());
        assert!(super::call("list_extract", &[&l, &Value::Float(1.0)]).is_err());
        assert_eq!(
            call("list_extract", &[&Value::Null, &Value::Int(1)]),
            Value::Null
        );
        assert_eq!(call("len", &[&l]), Value::Int(3));
        assert_eq!(call("array_length", &[&list(&[])]), Value::Int(0));
        assert_eq!(call("len", &[&Value::Null]), Value::Null);
        // On a string, `len` is its length in characters.
        assert_eq!(call("len", &[&s("héllo")]), Value::Int(5));
        assert!(super::call("len", &[&Value::Int(5)]).is_err());
        // DuckDB's `cardinality` takes only a map.
        assert!(super::call("cardinality", &[&l]).is_err());
        assert!(super::call("map_keys", &[&l]).is_err());
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
        // An aggregate the parser could not check is DuckDB's catalog error.
        assert!(super::call("list_aggr", &[&l, &s("avg")]).is_err());
        assert!(super::call("list_aggr", &[&l, &Value::Null]).is_err());
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
        // An order the parser could not check is an error, as in DuckDB.
        assert!(super::call("list_sort", &[&l, &s("sideways")]).is_err());
        assert!(super::call("list_sort", &[&l, &s("ASC"), &s("middle")]).is_err());
        assert!(super::call("list_sort", &[&l, &Value::Int(1)]).is_err());
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
        assert!(super::call("list_cat", &[&s("a"), &list(&["b"])]).is_err());
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
    }

    #[test]
    fn map_get_ci_ignores_case() {
        let m = m();
        assert_eq!(call("map_get_ci", &[&m, &s("USER-AGENT")]), s("curl/8"));
        assert_eq!(call("map_get_ci", &[&m, &s("Ä")]), s("x"));
        assert_eq!(call("map_get_ci", &[&m, &s("nope")]), Value::Null);
    }

    #[test]
    fn null_arguments_give_null_and_wrong_types_are_conversion_errors() {
        let m = m();
        assert_eq!(
            call("map_extract_value", &[&Value::Null, &s("a")]),
            Value::Null
        );
        assert_eq!(call("map_extract_value", &[&m, &Value::Null]), Value::Null);
        // #892: a wrong type is DuckDB's conversion error, not NULL.
        let err = |name: &str, args: &[&Value]| super::call(name, args).unwrap_err();
        assert_eq!(
            err("cardinality", &[&s("{}")]),
            VmError::Conversion {
                value: "{}".into(),
                to: "MAP"
            }
        );
        assert_eq!(
            err("map_extract_value", &[&m, &Value::Int(1)]),
            VmError::Conversion {
                value: "1".into(),
                to: "VARCHAR"
            }
        );
        assert_eq!(
            err("list_contains", &[&s("abc"), &s("a")]),
            VmError::Conversion {
                value: "abc".into(),
                to: "VARCHAR[]"
            }
        );
        assert!(matches!(
            err("cardinality", &[&m, &s("extra")]),
            VmError::MalformedProgram { .. }
        ));
        assert!(matches!(
            err("json_keys", &[&Value::Int(5)]),
            VmError::Conversion { to: "VARCHAR", .. }
        ));
        assert!(matches!(
            err("list_concat", &[&list(&["a"]), &s("b")]),
            VmError::Conversion {
                to: "VARCHAR[]",
                ..
            }
        ));
        assert!(is_map_function("Map_Get_CI"));
        assert!(!is_map_function("json_extract"));
    }
}
