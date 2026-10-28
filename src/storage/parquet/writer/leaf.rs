//! A column's leaves (column chunks) as written: a flat column is one
//! leaf, a [`ColumnType::MapUtf8`] two (`key` and `value`) with
//! repetition and definition levels (#621), a [`ColumnType::ListUtf8`]
//! one (`element`) with them (#801).
//!
//! A map is the three-level group DuckDB writes for `MAP(VARCHAR,
//! VARCHAR)`:
//!
//! ```text
//! <optional|required> group h (MAP) {
//!   repeated group key_value {
//!     required binary key (UTF8);
//!     optional binary value (UTF8);
//!   }
//! }
//! ```
//!
//! With `n` = 1 for a nullable map (0 for a required one), each row is one
//! level entry per leaf when it is NULL (definition level 0) or empty
//! (`n`), and one per entry otherwise: repetition level 0 for a row's first
//! entry and 1 for the rest; definition level `n + 1` for a key, and for a
//! value `n + 2` when it is present and `n + 1` when it is NULL. A leaf's
//! values hold one slot per level entry, `None` where the level holds no
//! value, so encoding and statistics treat it like a flat nullable column.
//!
//! A list is the standard three-level LIST group (DuckDB's `VARCHAR[]`,
//! pyarrow's `list<string>`):
//!
//! ```text
//! <optional|required> group labels (LIST) {
//!   repeated group list {
//!     optional binary element (UTF8);
//!   }
//! }
//! ```
//!
//! Its levels are a map's value leaf's: definition level 0 for a NULL
//! list, `n` for an empty one, `n + 1` for a NULL element and `n + 2` for
//! a present one; repetition level 0 for a row's first element and 1 for
//! the rest.

use super::{ColumnSpec, ColumnType, ColumnValues};

/// One leaf of a column, ready to write as a column chunk.
#[derive(Debug)]
pub(crate) struct Leaf {
    /// The path from the top-level field to the leaf.
    pub(crate) path: Vec<String>,
    /// The leaf's own type (`Utf8` for a map's key and value).
    pub(crate) column_type: ColumnType,
    pub(crate) max_def: u32,
    pub(crate) max_rep: u32,
    /// Per level entry; `None` when `max_def` is 0 (every entry defined).
    pub(crate) def_levels: Option<Vec<u32>>,
    /// Per level entry; `None` when `max_rep` is 0.
    pub(crate) rep_levels: Option<Vec<u32>>,
    /// One slot per level entry.
    pub(crate) values: ColumnValues,
}

/// `values` (one per row) of column `spec` as its leaves.
pub(crate) fn leaves(spec: &ColumnSpec, values: ColumnValues) -> Vec<Leaf> {
    let n = u32::from(spec.nullable);
    let values = match values {
        ColumnValues::List(rows) => return vec![list_leaf(spec, rows)],
        other => other,
    };
    let ColumnValues::Map(rows) = values else {
        let def_levels = spec
            .nullable
            .then(|| values.validity().into_iter().map(u32::from).collect());
        return vec![Leaf {
            path: vec![spec.name.clone()],
            column_type: spec.column_type,
            max_def: n,
            max_rep: 0,
            def_levels,
            rep_levels: None,
            values,
        }];
    };
    // A defined key, and a defined (non-NULL) value.
    let (entry, present) = (n.saturating_add(1), n.saturating_add(2));
    let mut rep = Vec::new();
    let mut key_def = Vec::new();
    let mut value_def = Vec::new();
    let mut keys = Vec::new();
    let mut values = Vec::new();
    for row in rows {
        match row {
            None => {
                rep.push(0);
                key_def.push(0);
                value_def.push(0);
                keys.push(None);
                values.push(None);
            }
            Some(entries) if entries.is_empty() => {
                rep.push(0);
                key_def.push(n);
                value_def.push(n);
                keys.push(None);
                values.push(None);
            }
            Some(entries) => {
                for (j, (key, value)) in entries.into_iter().enumerate() {
                    rep.push(u32::from(j > 0));
                    key_def.push(entry);
                    value_def.push(if value.is_some() { present } else { entry });
                    keys.push(Some(key.to_string()));
                    values.push(value.map(|v| v.to_string()));
                }
            }
        }
    }
    let path = |leaf: &str| vec![spec.name.clone(), "key_value".to_string(), leaf.to_string()];
    vec![
        Leaf {
            path: path("key"),
            column_type: ColumnType::Utf8,
            max_def: entry,
            max_rep: 1,
            def_levels: Some(key_def),
            rep_levels: Some(rep.clone()),
            values: ColumnValues::Utf8(keys),
        },
        Leaf {
            path: path("value"),
            column_type: ColumnType::Utf8,
            max_def: present,
            max_rep: 1,
            def_levels: Some(value_def),
            rep_levels: Some(rep),
            values: ColumnValues::Utf8(values),
        },
    ]
}

/// A `LIST<UTF8>` column's one `element` leaf.
fn list_leaf(spec: &ColumnSpec, rows: Vec<Option<super::ListItems>>) -> Leaf {
    let n = u32::from(spec.nullable);
    // A NULL element, and a present one.
    let (null, present) = (n.saturating_add(1), n.saturating_add(2));
    let mut rep = Vec::new();
    let mut def = Vec::new();
    let mut elements = Vec::new();
    for row in rows {
        match row {
            None => {
                rep.push(0);
                def.push(0);
                elements.push(None);
            }
            Some(items) if items.is_empty() => {
                rep.push(0);
                def.push(n);
                elements.push(None);
            }
            Some(items) => {
                for (j, item) in items.into_iter().enumerate() {
                    rep.push(u32::from(j > 0));
                    def.push(if item.is_some() { present } else { null });
                    elements.push(item.map(|v| v.to_string()));
                }
            }
        }
    }
    Leaf {
        path: vec![spec.name.clone(), "list".to_string(), "element".to_string()],
        column_type: ColumnType::Utf8,
        max_def: present,
        max_rep: 1,
        def_levels: Some(def),
        rep_levels: Some(rep),
        values: ColumnValues::Utf8(elements),
    }
}

/// How many leaves a column of type `t` has.
pub(crate) fn leaf_count(t: ColumnType) -> usize {
    match t {
        ColumnType::MapUtf8 => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parquet::schema::MapEntries;

    fn map(entries: &[(&str, Option<&str>)]) -> Option<MapEntries> {
        Some(
            entries
                .iter()
                .map(|(k, v)| ((*k).into(), v.map(Into::into)))
                .collect(),
        )
    }

    #[test]
    fn a_nullable_map_shreds_into_duckdb_levels() {
        let spec = ColumnSpec::new("h", ColumnType::MapUtf8, true);
        let rows = ColumnValues::Map(vec![
            map(&[("a", Some("1")), ("b", None)]),
            None,
            map(&[]),
            map(&[("c", Some("3"))]),
        ]);
        let [key, value] = <[Leaf; 2]>::try_from(leaves(&spec, rows)).unwrap();
        assert_eq!(key.path, ["h", "key_value", "key"]);
        assert_eq!((key.max_def, key.max_rep), (2, 1));
        assert_eq!((value.max_def, value.max_rep), (3, 1));
        assert_eq!(key.rep_levels.as_deref(), Some(&[0, 1, 0, 0, 0][..]));
        assert_eq!(key.def_levels.as_deref(), Some(&[2, 2, 0, 1, 2][..]));
        assert_eq!(value.def_levels.as_deref(), Some(&[3, 2, 0, 1, 3][..]));
        let s = |v: Option<&str>| v.map(str::to_string);
        assert_eq!(
            key.values,
            ColumnValues::Utf8(vec![s(Some("a")), s(Some("b")), None, None, s(Some("c"))])
        );
        assert_eq!(
            value.values,
            ColumnValues::Utf8(vec![s(Some("1")), None, None, None, s(Some("3"))])
        );
    }

    #[test]
    fn a_required_map_starts_its_levels_at_zero() {
        let spec = ColumnSpec::new("h", ColumnType::MapUtf8, false);
        let rows = ColumnValues::Map(vec![map(&[]), map(&[("a", None)])]);
        let [key, value] = <[Leaf; 2]>::try_from(leaves(&spec, rows)).unwrap();
        assert_eq!((key.max_def, value.max_def), (1, 2));
        assert_eq!(key.def_levels.as_deref(), Some(&[0, 1][..]));
        assert_eq!(value.def_levels.as_deref(), Some(&[0, 1][..]));
    }

    #[test]
    fn a_nullable_list_shreds_into_three_level_list_levels() {
        let spec = ColumnSpec::new("l", ColumnType::ListUtf8, true);
        let item = |v: Option<&str>| v.map(Into::into);
        let rows = ColumnValues::List(vec![
            Some(vec![item(Some("a")), item(None), item(Some("b"))]),
            None,
            Some(vec![]),
            Some(vec![item(Some("c"))]),
        ]);
        let [leaf] = <[Leaf; 1]>::try_from(leaves(&spec, rows)).unwrap();
        assert_eq!(leaf.path, ["l", "list", "element"]);
        assert_eq!((leaf.max_def, leaf.max_rep), (3, 1));
        assert_eq!(leaf.rep_levels.as_deref(), Some(&[0, 1, 1, 0, 0, 0][..]));
        assert_eq!(leaf.def_levels.as_deref(), Some(&[3, 2, 3, 0, 1, 3][..]));
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            leaf.values,
            ColumnValues::Utf8(vec![s("a"), None, s("b"), None, None, s("c")])
        );
    }

    #[test]
    fn a_required_list_starts_its_levels_at_zero() {
        let spec = ColumnSpec::new("l", ColumnType::ListUtf8, false);
        let rows = ColumnValues::List(vec![Some(vec![]), Some(vec![None])]);
        let [leaf] = <[Leaf; 1]>::try_from(leaves(&spec, rows)).unwrap();
        assert_eq!(leaf.max_def, 2);
        assert_eq!(leaf.def_levels.as_deref(), Some(&[0, 1][..]));
    }

    #[test]
    fn a_flat_column_is_one_leaf() {
        let spec = ColumnSpec::new("i", ColumnType::Int32, true);
        let [leaf] =
            <[Leaf; 1]>::try_from(leaves(&spec, ColumnValues::Int32(vec![Some(1), None]))).unwrap();
        assert_eq!(
            (leaf.path, leaf.max_def, leaf.max_rep),
            (vec!["i".to_string()], 1, 0)
        );
        assert_eq!(leaf.def_levels, Some(vec![1, 0]));
        assert_eq!(leaf.rep_levels, None);
        assert_eq!(leaf_count(ColumnType::MapUtf8), 2);
        assert_eq!(leaf_count(ColumnType::Int32), 1);
        assert_eq!(leaf_count(ColumnType::ListUtf8), 1);
    }
}
