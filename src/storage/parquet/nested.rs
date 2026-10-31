//! Reconstructs nested (struct/list/map) values from Parquet's flattened
//! leaf columns using Dremel definition/repetition levels.
//!
//! Scope: PLAIN-encoded leaves only (dictionary-encoded nested leaves are
//! not supported yet — tracked as a follow-up), structs of scalars/structs,
//! 2- and 3-level `LIST`s of scalars, and `MAP`s of scalar keys/values.
//! Deeper repetition (a list of lists, a list of structs) is out of scope.

use crate::storage::parquet::footer::ConvertedType;
use crate::storage::parquet::reader::LeafScalar;
use crate::storage::parquet::schema_tree::SchemaNode;
use std::collections::HashMap;
use std::fmt;

/// One reconstructed value of a (possibly nested) field.
#[derive(Debug, Clone, PartialEq)]
pub enum NestedValue {
    /// A null at this level (the value or one of its ancestors is absent).
    Null,
    /// A leaf value.
    Scalar(LeafScalar),
    /// A `LIST`'s elements, or a `MAP`'s key/value structs.
    List(Vec<NestedValue>),
    /// A struct's fields, in schema order.
    Struct(Vec<(String, NestedValue)>),
}

/// One leaf column's decoded levels and values, one entry per physical
/// Dremel record (which for a repeated column can be more than one entry
/// per output row). `values[i]` is `Some` exactly when
/// `def_levels[i] == leaf.max_def_level`.
#[derive(Debug, Clone, Default)]
pub struct LeafEntries {
    /// Repetition level per entry (empty for a non-repeated leaf).
    pub rep_levels: Vec<u32>,
    /// Definition level per entry.
    pub def_levels: Vec<u32>,
    /// The value per entry, `None` where the entry is null.
    pub values: Vec<Option<LeafScalar>>,
}

/// Why a row group's nested fields could not be reconstructed.
#[derive(Debug)]
pub enum NestedError {
    /// No decoded data for the leaf column at this path.
    MissingLeafData(String),
    /// A `LIST` element shape this reader does not support.
    UnsupportedListElement(String),
    /// A `MAP` that is not `key_value(key, value)` of two leaves.
    UnsupportedMapShape(String),
    /// The leaf data under this path has fewer levels or values than its
    /// rows and levels require: the file is corrupt.
    TruncatedLeafData(String),
}

impl fmt::Display for NestedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NestedError::MissingLeafData(path) => {
                write!(f, "no decoded data for leaf column '{path}'")
            }
            NestedError::UnsupportedListElement(path) => write!(
                f,
                "list element at '{path}' is not a supported shape (scalar or flat struct)"
            ),
            NestedError::UnsupportedMapShape(path) => write!(
                f,
                "MAP at '{path}' does not have the expected key_value(key, value) shape"
            ),
            NestedError::TruncatedLeafData(path) => write!(
                f,
                "leaf data under '{path}' has fewer levels or values than its rows"
            ),
        }
    }
}

impl std::error::Error for NestedError {}

/// Result alias for nested reconstruction.
pub type Result<T> = std::result::Result<T, NestedError>;

/// Decoded leaf data, keyed by leaf column path (dot-joined names from the
/// schema root's children down to the leaf).
pub type LeafData = HashMap<String, LeafEntries>;

/// Reconstruct every top-level field of `root` into one `Vec<NestedValue>`
/// (length `num_rows`) per field, given already-decoded leaf column data.
pub fn reconstruct_row_group(
    root: &SchemaNode,
    num_rows: usize,
    leaves: &LeafData,
) -> Result<Vec<(String, Vec<NestedValue>)>> {
    root.children
        .iter()
        .map(|field| {
            Ok((
                field.name.clone(),
                build_field(field, &field.name, num_rows, leaves)?,
            ))
        })
        .collect()
}

fn build_field(
    node: &SchemaNode,
    path: &str,
    num_rows: usize,
    leaves: &LeafData,
) -> Result<Vec<NestedValue>> {
    if node.is_leaf() {
        let entries = leaves
            .get(path)
            .ok_or_else(|| NestedError::MissingLeafData(path.to_string()))?;
        return Ok(entries
            .values
            .iter()
            .map(|v| v.clone().map_or(NestedValue::Null, NestedValue::Scalar))
            .collect());
    }

    if node.element.repetition == Some(crate::storage::parquet::footer::Repetition::Repeated) {
        // Reached directly on a repeated node only via recursion from a
        // list/map wrapper below -- top-level repeated fields go through
        // `build_list_field` instead.
        return build_list_field(node, path, num_rows, leaves);
    }

    if is_map(node) {
        return build_map_field(node, path, num_rows, leaves);
    }
    if node.children.len() == 1
        && node.children.first().is_some_and(|child| {
            child.element.repetition == Some(crate::storage::parquet::footer::Repetition::Repeated)
        })
    {
        return build_list_field(node, path, num_rows, leaves);
    }

    // A non-repeated group (struct): presence per row is determined by the
    // first descendant leaf's definition level relative to this node's own
    // max_def_level (every leaf under a struct-only subtree shares the same
    // row count, since nothing here repeats).
    let driver_path = first_leaf_path(node, path);
    let driver = leaves
        .get(&driver_path)
        .ok_or(NestedError::MissingLeafData(driver_path))?;
    let mut children_values = Vec::with_capacity(node.children.len());
    for child in &node.children {
        let child_path = format!("{path}.{}", child.name);
        children_values.push((
            child.name.clone(),
            build_field(child, &child_path, num_rows, leaves)?,
        ));
    }

    let truncated = || NestedError::TruncatedLeafData(path.to_string());
    // Every row needs a driver level, so a short driver fails before a
    // forged `num_rows` can size the output.
    if driver.def_levels.len() < num_rows {
        return Err(truncated());
    }
    let mut out = Vec::with_capacity(num_rows);
    for (row, &def) in driver.def_levels.iter().take(num_rows).enumerate() {
        if def < node.max_def_level {
            out.push(NestedValue::Null);
        } else {
            let fields = children_values
                .iter()
                .map(|(name, vals)| Some((name.clone(), vals.get(row)?.clone())))
                .collect::<Option<_>>()
                .ok_or_else(truncated)?;
            out.push(NestedValue::Struct(fields));
        }
    }
    Ok(out)
}

/// The path of the first leaf under `node` (a node without children is
/// itself a leaf).
fn first_leaf_path(node: &SchemaNode, path: &str) -> String {
    match node.children.first() {
        None => path.to_string(),
        Some(child) => first_leaf_path(child, &format!("{path}.{}", child.name)),
    }
}

fn is_map(node: &SchemaNode) -> bool {
    matches!(node.element.converted_type, Some(ConvertedType::Other(1))) && node.children.len() == 1
}

/// `MAP` schema: `node (MAP) { repeated group key_value { required key; optional value } }`.
fn build_map_field(
    node: &SchemaNode,
    path: &str,
    num_rows: usize,
    leaves: &LeafData,
) -> Result<Vec<NestedValue>> {
    let [key_value] = node.children.as_slice() else {
        return Err(NestedError::UnsupportedMapShape(path.to_string()));
    };
    let [key_node, value_node] = key_value.children.as_slice() else {
        return Err(NestedError::UnsupportedMapShape(path.to_string()));
    };
    if !key_node.is_leaf() || !value_node.is_leaf() {
        return Err(NestedError::UnsupportedMapShape(path.to_string()));
    }
    let key_path = format!("{path}.{}.{}", key_value.name, key_node.name);
    let value_path = format!("{path}.{}.{}", key_value.name, value_node.name);
    let keys = leaves
        .get(&key_path)
        .ok_or_else(|| NestedError::MissingLeafData(key_path.clone()))?;
    let values = leaves
        .get(&value_path)
        .ok_or_else(|| NestedError::MissingLeafData(value_path.clone()))?;

    build_repeated(
        node,
        key_value,
        path,
        num_rows,
        &keys.rep_levels,
        &keys.def_levels,
        |i| {
            Some(NestedValue::Struct(vec![
                ("key".to_string(), scalar_or_null(keys.values.get(i)?)),
                ("value".to_string(), scalar_or_null(values.values.get(i)?)),
            ]))
        },
    )
}

/// `LIST` schema, either 3-level (`node (LIST) { repeated group list { element } }`)
/// or 2-level (`node { repeated <type> element }` directly).
fn build_list_field(
    node: &SchemaNode,
    path: &str,
    num_rows: usize,
    leaves: &LeafData,
) -> Result<Vec<NestedValue>> {
    // Callers reach here only for a group (which has children), so the
    // first arm is defensive.
    let Some(repeated) = node.children.first() else {
        return Err(NestedError::UnsupportedListElement(path.to_string()));
    };
    // A repeated node without children is itself the (2-level) element.
    let element = repeated.children.first().unwrap_or(repeated);
    if !element.is_leaf() {
        return Err(NestedError::UnsupportedListElement(path.to_string()));
    }
    let element_path = if repeated.is_leaf() {
        format!("{path}.{}", repeated.name)
    } else {
        format!("{path}.{}.{}", repeated.name, element.name)
    };
    let entries = leaves
        .get(&element_path)
        .ok_or_else(|| NestedError::MissingLeafData(element_path.clone()))?;

    build_repeated(
        node,
        repeated,
        path,
        num_rows,
        &entries.rep_levels,
        &entries.def_levels,
        |i| entries.values.get(i).map(scalar_or_null),
    )
}

fn scalar_or_null(value: &Option<LeafScalar>) -> NestedValue {
    value.clone().map_or(NestedValue::Null, NestedValue::Scalar)
}

/// Shared repetition-level walk for `LIST`/`MAP`: `wrapper` is the
/// enclosing (possibly optional) field, `repeated` is its one repeated
/// child. Groups entries by repetition-level-0 boundaries (one boundary
/// per output row) and calls `element_at(i)` to build each element from
/// whichever leaf column(s) actually hold the data (`None` when that
/// column has no entry `i`). Levels and values that run out before
/// `num_rows` rows are built are a [`NestedError::TruncatedLeafData`].
fn build_repeated(
    wrapper: &SchemaNode,
    repeated: &SchemaNode,
    path: &str,
    num_rows: usize,
    rep_levels: &[u32],
    def_levels: &[u32],
    element_at: impl Fn(usize) -> Option<NestedValue>,
) -> Result<Vec<NestedValue>> {
    let truncated = || NestedError::TruncatedLeafData(path.to_string());
    let wrapper_def = wrapper.max_def_level;
    let repeated_def = repeated.max_def_level;
    // Each row takes at least one level, so a forged `num_rows` cannot
    // reserve more than the levels actually decoded.
    let mut out = Vec::with_capacity(num_rows.min(def_levels.len()));
    // `i` only advances past an entry `def_levels` has, so it stays at
    // most `def_levels.len()` and `saturating_add` never saturates.
    let mut i = 0usize;
    for _ in 0..num_rows {
        let d = *def_levels.get(i).ok_or_else(truncated)?;
        if d < wrapper_def {
            out.push(NestedValue::Null);
            i = i.saturating_add(1);
        } else if d < repeated_def {
            out.push(NestedValue::List(Vec::new()));
            i = i.saturating_add(1);
        } else {
            let mut elems = Vec::new();
            loop {
                elems.push(element_at(i).ok_or_else(truncated)?);
                i = i.saturating_add(1);
                if i >= rep_levels.len() || rep_levels.get(i) == Some(&0) {
                    break;
                }
            }
            out.push(NestedValue::List(elems));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::storage::parquet::footer::{PhysicalType, Repetition, SchemaElement};
    use crate::storage::parquet::schema_tree::build_schema_tree;

    pub(super) fn group(name: &str, repetition: Repetition, num_children: i32) -> SchemaElement {
        SchemaElement {
            name: name.to_string(),
            physical_type: None,
            repetition: Some(repetition),
            num_children: Some(num_children),
            type_length: None,
            converted_type: None,
            scale: None,
            precision: None,
            timestamp_units: None,
            logical_type: None,
        }
    }

    pub(super) fn map_group(
        name: &str,
        repetition: Repetition,
        num_children: i32,
    ) -> SchemaElement {
        SchemaElement {
            converted_type: Some(ConvertedType::Other(1)),
            ..group(name, repetition, num_children)
        }
    }

    pub(super) fn leaf_elem(
        name: &str,
        repetition: Repetition,
        physical_type: PhysicalType,
    ) -> SchemaElement {
        SchemaElement {
            name: name.to_string(),
            physical_type: Some(physical_type),
            repetition: Some(repetition),
            num_children: None,
            type_length: None,
            converted_type: None,
            scale: None,
            precision: None,
            timestamp_units: None,
            logical_type: None,
        }
    }

    pub(super) fn entries(
        rep_levels: Vec<u32>,
        def_levels: Vec<u32>,
        values: Vec<Option<LeafScalar>>,
    ) -> LeafEntries {
        LeafEntries {
            rep_levels,
            def_levels,
            values,
        }
    }

    #[test]
    fn flat_required_leaf_reconstructs_plain_values() {
        let schema = vec![
            group("root", Repetition::Required, 1),
            leaf_elem("id", Repetition::Required, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        leaves.insert(
            "id".to_string(),
            entries(
                vec![0, 0],
                vec![0, 0],
                vec![Some(LeafScalar::Int64(1)), Some(LeafScalar::Int64(2))],
            ),
        );

        let result = reconstruct_row_group(&tree, 2, &leaves).unwrap();
        assert_eq!(
            result,
            vec![(
                "id".to_string(),
                vec![
                    NestedValue::Scalar(LeafScalar::Int64(1)),
                    NestedValue::Scalar(LeafScalar::Int64(2))
                ]
            )]
        );
    }

    #[test]
    fn optional_leaf_missing_value_is_null() {
        let schema = vec![
            group("root", Repetition::Required, 1),
            leaf_elem("name", Repetition::Optional, PhysicalType::ByteArray),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        leaves.insert(
            "name".to_string(),
            entries(
                vec![0, 0],
                vec![1, 0],
                vec![Some(LeafScalar::Str("hi".into())), None],
            ),
        );

        let result = reconstruct_row_group(&tree, 2, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![
                NestedValue::Scalar(LeafScalar::Str("hi".into())),
                NestedValue::Null
            ]
        );
    }

    #[test]
    fn missing_leaf_data_errors() {
        let schema = vec![
            group("root", Repetition::Required, 1),
            leaf_elem("id", Repetition::Required, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let leaves = LeafData::new();

        let err = reconstruct_row_group(&tree, 1, &leaves).unwrap_err();
        assert_eq!(err.to_string(), "no decoded data for leaf column 'id'");
        assert!(matches!(err, NestedError::MissingLeafData(p) if p == "id"));
    }

    #[test]
    fn optional_struct_reconstructs_and_nulls_by_driver_leaf() {
        // message { optional group person { required int64 id; optional binary name } }
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("person", Repetition::Optional, 2),
            leaf_elem("id", Repetition::Required, PhysicalType::Int64),
            leaf_elem("name", Repetition::Optional, PhysicalType::ByteArray),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        // person.id: def_level 1 = present (struct max_def_level is 1)
        leaves.insert(
            "person.id".to_string(),
            entries(
                vec![0, 0],
                vec![1, 0],
                vec![Some(LeafScalar::Int64(7)), None],
            ),
        );
        leaves.insert(
            "person.name".to_string(),
            entries(
                vec![0, 0],
                vec![2, 0],
                vec![Some(LeafScalar::Str("bob".into())), None],
            ),
        );

        let result = reconstruct_row_group(&tree, 2, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![
                NestedValue::Struct(vec![
                    ("id".to_string(), NestedValue::Scalar(LeafScalar::Int64(7))),
                    (
                        "name".to_string(),
                        NestedValue::Scalar(LeafScalar::Str("bob".into()))
                    )
                ]),
                NestedValue::Null,
            ]
        );
    }

    #[test]
    fn three_level_list_of_scalars_groups_by_repetition_boundary() {
        // message { optional group tags (LIST) { repeated group list { optional binary element } } }
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("tags", Repetition::Optional, 1),
            group("list", Repetition::Repeated, 1),
            leaf_elem("element", Repetition::Optional, PhysicalType::ByteArray),
        ];
        let tree = build_schema_tree(&schema);
        // tags (optional) max_def=1, list (repeated) max_def=2, element (optional) max_def=3
        let mut leaves = LeafData::new();
        // row0: ["a", "b"], row1: [] (empty list, present but zero elements), row2: null (tags absent)
        leaves.insert(
            "tags.list.element".to_string(),
            entries(
                vec![0, 1, 0, 0],
                vec![3, 3, 1, 0],
                vec![
                    Some(LeafScalar::Str("a".into())),
                    Some(LeafScalar::Str("b".into())),
                    None,
                    None,
                ],
            ),
        );

        let result = reconstruct_row_group(&tree, 3, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![
                NestedValue::List(vec![
                    NestedValue::Scalar(LeafScalar::Str("a".into())),
                    NestedValue::Scalar(LeafScalar::Str("b".into()))
                ]),
                NestedValue::List(vec![]),
                NestedValue::Null,
            ]
        );
    }

    #[test]
    fn two_level_list_of_scalars() {
        // message { required group nums { repeated int32 element } }
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("nums", Repetition::Required, 1),
            leaf_elem("element", Repetition::Repeated, PhysicalType::Int32),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        leaves.insert(
            "nums.element".to_string(),
            entries(
                vec![0, 1, 1],
                vec![1, 1, 1],
                vec![
                    Some(LeafScalar::Int32(1)),
                    Some(LeafScalar::Int32(2)),
                    Some(LeafScalar::Int32(3)),
                ],
            ),
        );

        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![NestedValue::List(vec![
                NestedValue::Scalar(LeafScalar::Int32(1)),
                NestedValue::Scalar(LeafScalar::Int32(2)),
                NestedValue::Scalar(LeafScalar::Int32(3))
            ])]
        );
    }

    #[test]
    fn map_of_scalars_reconstructs_key_value_pairs() {
        // message { optional group m (MAP) { repeated group key_value { required binary key; optional int64 value } } }
        let schema = vec![
            group("root", Repetition::Required, 1),
            map_group("m", Repetition::Optional, 1),
            group("key_value", Repetition::Repeated, 2),
            leaf_elem("key", Repetition::Required, PhysicalType::ByteArray),
            leaf_elem("value", Repetition::Optional, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        leaves.insert(
            "m.key_value.key".to_string(),
            entries(
                vec![0, 1],
                vec![2, 2],
                vec![
                    Some(LeafScalar::Str("k1".into())),
                    Some(LeafScalar::Str("k2".into())),
                ],
            ),
        );
        leaves.insert(
            "m.key_value.value".to_string(),
            entries(
                vec![0, 1],
                vec![3, 3],
                vec![Some(LeafScalar::Int64(1)), Some(LeafScalar::Int64(2))],
            ),
        );

        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![NestedValue::List(vec![
                NestedValue::Struct(vec![
                    (
                        "key".to_string(),
                        NestedValue::Scalar(LeafScalar::Str("k1".into()))
                    ),
                    (
                        "value".to_string(),
                        NestedValue::Scalar(LeafScalar::Int64(1))
                    )
                ]),
                NestedValue::Struct(vec![
                    (
                        "key".to_string(),
                        NestedValue::Scalar(LeafScalar::Str("k2".into()))
                    ),
                    (
                        "value".to_string(),
                        NestedValue::Scalar(LeafScalar::Int64(2))
                    )
                ]),
            ])]
        );
    }

    #[test]
    fn map_with_wrong_shape_errors() {
        // key_value group with only one child -- not a valid map shape.
        let schema = vec![
            group("root", Repetition::Required, 1),
            map_group("m", Repetition::Optional, 1),
            group("key_value", Repetition::Repeated, 1),
            leaf_elem("key", Repetition::Required, PhysicalType::ByteArray),
        ];
        let tree = build_schema_tree(&schema);
        let leaves = LeafData::new();

        let err = reconstruct_row_group(&tree, 1, &leaves).unwrap_err();
        assert!(matches!(err, NestedError::UnsupportedMapShape(p) if p == "m"));
    }

    #[test]
    fn nested_error_display_messages() {
        assert_eq!(
            NestedError::MissingLeafData("a.b".into()).to_string(),
            "no decoded data for leaf column 'a.b'"
        );
        assert_eq!(
            NestedError::UnsupportedListElement("a.b".into()).to_string(),
            "list element at 'a.b' is not a supported shape (scalar or flat struct)"
        );
        assert_eq!(
            NestedError::UnsupportedMapShape("a.b".into()).to_string(),
            "MAP at 'a.b' does not have the expected key_value(key, value) shape"
        );
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#299 MC/DC backfill).

    use super::tests::{entries, group, leaf_elem, map_group};
    use super::*;
    use crate::storage::parquet::footer::{PhysicalType, Repetition};
    use crate::storage::parquet::schema_tree::build_schema_tree;

    // storage_parquet_nested_build_field_36f864d7: `node.children.len() == 1
    //     && node.children.first().is_some_and(|child| child.element.repetition == Some(Repeated))`
    #[test]
    fn mcdc__storage_parquet_nested_build_field_36f864d7__v1_both_true_is_a_two_level_list() {
        // "nums { repeated int32 element }": exactly one child, and that
        // child is repeated -- both leafs true, so it's a 2-level list.
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("nums", Repetition::Required, 1),
            leaf_elem("element", Repetition::Repeated, PhysicalType::Int32),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        leaves.insert(
            "nums.element".to_string(),
            entries(vec![0], vec![1], vec![Some(LeafScalar::Int32(1))]),
        );
        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![NestedValue::List(vec![NestedValue::Scalar(
                LeafScalar::Int32(1)
            )])]
        );
    }

    #[test]
    fn mcdc__storage_parquet_nested_build_field_36f864d7__v2_more_than_one_child_is_a_struct() {
        // Two children -- first leaf (`len() == 1`) is false, so this
        // falls through to the struct path regardless of repetition.
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("pair", Repetition::Required, 2),
            leaf_elem("a", Repetition::Required, PhysicalType::Int64),
            leaf_elem("b", Repetition::Required, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        leaves.insert(
            "pair.a".to_string(),
            entries(vec![0], vec![0], vec![Some(LeafScalar::Int64(1))]),
        );
        leaves.insert(
            "pair.b".to_string(),
            entries(vec![0], vec![0], vec![Some(LeafScalar::Int64(2))]),
        );
        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert!(matches!(result[0].1[0], NestedValue::Struct(_)));
    }

    #[test]
    fn mcdc__storage_parquet_nested_build_field_36f864d7__v3_one_child_not_repeated_is_a_struct() {
        // Exactly one child (first leaf true), but that child is not
        // repeated (second leaf false) -- still a struct, not a list.
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("wrapper", Repetition::Required, 1),
            leaf_elem("a", Repetition::Required, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        leaves.insert(
            "wrapper.a".to_string(),
            entries(vec![0], vec![0], vec![Some(LeafScalar::Int64(9))]),
        );
        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert!(matches!(result[0].1[0], NestedValue::Struct(_)));
    }

    // storage_parquet_nested_build_map_field_dd34e07a: `!key_node.is_leaf() || !value_node.is_leaf()`
    #[test]
    fn mcdc__storage_parquet_nested_build_map_field_dd34e07a__v1_key_not_leaf_is_unsupported() {
        let schema = vec![
            group("root", Repetition::Required, 1),
            map_group("m", Repetition::Optional, 1),
            group("key_value", Repetition::Repeated, 2),
            group("key", Repetition::Required, 1), // not a leaf
            leaf_elem("inner", Repetition::Required, PhysicalType::Int64),
            leaf_elem("value", Repetition::Optional, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let leaves = LeafData::new();
        let err = reconstruct_row_group(&tree, 1, &leaves).unwrap_err();
        assert!(matches!(err, NestedError::UnsupportedMapShape(p) if p == "m"));
    }

    #[test]
    fn mcdc__storage_parquet_nested_build_map_field_dd34e07a__v2_key_leaf_value_not_leaf_is_unsupported(
    ) {
        let schema = vec![
            group("root", Repetition::Required, 1),
            map_group("m", Repetition::Optional, 1),
            group("key_value", Repetition::Repeated, 2),
            leaf_elem("key", Repetition::Required, PhysicalType::ByteArray),
            group("value", Repetition::Optional, 1), // not a leaf
            leaf_elem("inner", Repetition::Required, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let leaves = LeafData::new();
        let err = reconstruct_row_group(&tree, 1, &leaves).unwrap_err();
        assert!(matches!(err, NestedError::UnsupportedMapShape(p) if p == "m"));
    }

    #[test]
    fn mcdc__storage_parquet_nested_build_map_field_dd34e07a__v3_both_leaves_is_supported() {
        let schema = vec![
            group("root", Repetition::Required, 1),
            map_group("m", Repetition::Optional, 1),
            group("key_value", Repetition::Repeated, 2),
            leaf_elem("key", Repetition::Required, PhysicalType::ByteArray),
            leaf_elem("value", Repetition::Optional, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let mut leaves = LeafData::new();
        leaves.insert(
            "m.key_value.key".to_string(),
            entries(vec![0], vec![2], vec![Some(LeafScalar::Str("k".into()))]),
        );
        leaves.insert(
            "m.key_value.value".to_string(),
            entries(vec![0], vec![3], vec![Some(LeafScalar::Int64(1))]),
        );
        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert!(matches!(result[0].1[0], NestedValue::List(_)));
    }

    // storage_parquet_nested_build_repeated_f001886e: `i >= rep_levels.len() || rep_levels.get(i) == Some(&0)`
    fn two_level_list_tree() -> SchemaNode {
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("nums", Repetition::Required, 1),
            leaf_elem("element", Repetition::Repeated, PhysicalType::Int32),
        ];
        build_schema_tree(&schema)
    }

    #[test]
    fn mcdc__storage_parquet_nested_build_repeated_f001886e__v1_reaches_end_of_rep_levels() {
        // Single-element list: after pushing the one element, `i` equals
        // `rep_levels.len()` -- the first leaf is true (short-circuits).
        let tree = two_level_list_tree();
        let mut leaves = LeafData::new();
        leaves.insert(
            "nums.element".to_string(),
            entries(vec![0], vec![1], vec![Some(LeafScalar::Int32(1))]),
        );
        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![NestedValue::List(vec![NestedValue::Scalar(
                LeafScalar::Int32(1)
            )])]
        );
    }

    #[test]
    fn mcdc__storage_parquet_nested_build_repeated_f001886e__v2_hits_a_row_boundary_before_the_end()
    {
        // Two entries, but the second starts a new row (`rep_levels[1] ==
        // 0`) -- `i < len` so the first leaf is false, the second is true.
        let tree = two_level_list_tree();
        let mut leaves = LeafData::new();
        leaves.insert(
            "nums.element".to_string(),
            entries(
                vec![0, 0],
                vec![1, 1],
                vec![Some(LeafScalar::Int32(1)), Some(LeafScalar::Int32(2))],
            ),
        );
        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![NestedValue::List(vec![NestedValue::Scalar(
                LeafScalar::Int32(1)
            )])]
        );
    }

    #[test]
    fn mcdc__storage_parquet_nested_build_repeated_f001886e__v3_neither_leaf_true_keeps_accumulating(
    ) {
        // Two entries in the same list (`rep_levels[1] == 1`): both leafs
        // false at the first check, so the loop continues to a second
        // element before eventually breaking at the true end.
        let tree = two_level_list_tree();
        let mut leaves = LeafData::new();
        leaves.insert(
            "nums.element".to_string(),
            entries(
                vec![0, 1],
                vec![1, 1],
                vec![Some(LeafScalar::Int32(1)), Some(LeafScalar::Int32(2))],
            ),
        );
        let result = reconstruct_row_group(&tree, 1, &leaves).unwrap();
        assert_eq!(
            result[0].1,
            vec![NestedValue::List(vec![
                NestedValue::Scalar(LeafScalar::Int32(1)),
                NestedValue::Scalar(LeafScalar::Int32(2)),
            ])]
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "test code: fixed schemas and a test-local PRNG"
)]
mod malformed_levels {
    //! #657: levels and values from a corrupt file must fail
    //! reconstruction with an error, never index out of bounds.

    use super::tests::{entries, group, leaf_elem, map_group};
    use super::*;
    use crate::storage::parquet::footer::{PhysicalType, Repetition};
    use crate::storage::parquet::schema_tree::build_schema_tree;

    /// A struct, a 3-level list, a 2-level list and a map, side by side.
    fn tree() -> SchemaNode {
        build_schema_tree(&[
            group("root", Repetition::Required, 4),
            group("s", Repetition::Optional, 2),
            leaf_elem("a", Repetition::Optional, PhysicalType::Int64),
            leaf_elem("b", Repetition::Required, PhysicalType::Int64),
            group("l", Repetition::Optional, 1),
            group("list", Repetition::Repeated, 1),
            leaf_elem("element", Repetition::Optional, PhysicalType::Int32),
            group("nums", Repetition::Required, 1),
            leaf_elem("element", Repetition::Repeated, PhysicalType::Int32),
            map_group("m", Repetition::Optional, 1),
            group("key_value", Repetition::Repeated, 2),
            leaf_elem("key", Repetition::Required, PhysicalType::Int32),
            leaf_elem("value", Repetition::Optional, PhysicalType::Int64),
        ])
    }

    const LEAVES: [&str; 6] = [
        "s.a",
        "s.b",
        "l.list.element",
        "nums.element",
        "m.key_value.key",
        "m.key_value.value",
    ];

    /// xorshift64, so a failure reproduces.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n.max(1)
        }
    }

    #[test]
    fn random_levels_and_short_values_error_or_succeed_never_panic() {
        let tree = tree();
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..3_000 {
            let mut leaves = LeafData::new();
            for path in LEAVES {
                let n = rng.below(12) as usize;
                let rep = (0..n).map(|_| rng.below(3) as u32).collect();
                let def = (0..n).map(|_| rng.below(5) as u32).collect();
                let values = (0..rng.below(12))
                    .map(|i| (rng.below(3) != 0).then_some(LeafScalar::Int32(i as i32)))
                    .collect();
                leaves.insert(path.to_string(), entries(rep, def, values));
            }
            let num_rows = rng.below(10) as usize;
            let _outcome = reconstruct_row_group(&tree, num_rows, &leaves);
        }
    }

    #[test]
    fn a_struct_driver_shorter_than_the_rows_is_truncated_leaf_data() {
        let mut leaves = LeafData::new();
        for path in LEAVES {
            leaves.insert(path.to_string(), entries(vec![0], vec![2], vec![None]));
        }
        // One level per leaf but three rows: before #657 this indexed
        // `def_levels[1]` and panicked.
        let err = reconstruct_row_group(&tree(), 3, &leaves).unwrap_err();
        assert!(matches!(&err, NestedError::TruncatedLeafData(p) if p == "s"));
        assert_eq!(
            err.to_string(),
            "leaf data under 's' has fewer levels or values than its rows"
        );
    }

    #[test]
    fn a_list_with_levels_but_no_values_is_truncated_leaf_data() {
        let schema = [
            group("root", Repetition::Required, 1),
            group("nums", Repetition::Required, 1),
            leaf_elem("element", Repetition::Repeated, PhysicalType::Int32),
        ];
        let mut leaves = LeafData::new();
        leaves.insert(
            "nums.element".to_string(),
            entries(vec![0, 1], vec![1, 1], vec![]),
        );
        let err = reconstruct_row_group(&build_schema_tree(&schema), 1, &leaves).unwrap_err();
        assert!(matches!(err, NestedError::TruncatedLeafData(p) if p == "nums"));
    }

    #[test]
    fn a_map_whose_values_run_short_is_truncated_leaf_data() {
        let schema = [
            group("root", Repetition::Required, 1),
            map_group("m", Repetition::Optional, 1),
            group("key_value", Repetition::Repeated, 2),
            leaf_elem("key", Repetition::Required, PhysicalType::Int32),
            leaf_elem("value", Repetition::Optional, PhysicalType::Int64),
        ];
        let mut leaves = LeafData::new();
        leaves.insert(
            "m.key_value.key".to_string(),
            entries(vec![0, 1], vec![2, 2], vec![Some(LeafScalar::Int32(1)); 2]),
        );
        leaves.insert(
            "m.key_value.value".to_string(),
            entries(vec![0], vec![3], vec![Some(LeafScalar::Int64(1))]),
        );
        let err = reconstruct_row_group(&build_schema_tree(&schema), 1, &leaves).unwrap_err();
        assert!(matches!(err, NestedError::TruncatedLeafData(p) if p == "m"));
    }

    #[test]
    fn a_forged_row_count_fails_without_reserving_for_it() {
        let schema = [
            group("root", Repetition::Required, 1),
            group("nums", Repetition::Required, 1),
            leaf_elem("element", Repetition::Repeated, PhysicalType::Int32),
        ];
        let mut leaves = LeafData::new();
        leaves.insert(
            "nums.element".to_string(),
            entries(vec![0], vec![1], vec![Some(LeafScalar::Int32(1))]),
        );
        let tree = build_schema_tree(&schema);
        let err = reconstruct_row_group(&tree, usize::MAX, &leaves).unwrap_err();
        assert!(matches!(err, NestedError::TruncatedLeafData(_)));
    }
}
