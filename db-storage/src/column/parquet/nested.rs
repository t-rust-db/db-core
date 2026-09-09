//! Reconstructs nested (struct/list/map) values from Parquet's flattened
//! leaf columns using Dremel definition/repetition levels.
//!
//! Scope: PLAIN-encoded leaves only (dictionary-encoded nested leaves are
//! not supported yet — tracked as a follow-up), structs of scalars/structs,
//! 2- and 3-level `LIST`s of scalars, and `MAP`s of scalar keys/values.
//! Deeper repetition (a list of lists, a list of structs) is out of scope.

use crate::column::parquet::footer::ConvertedType;
use crate::column::parquet::reader::LeafScalar;
use crate::column::parquet::schema_tree::SchemaNode;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum NestedValue {
    Null,
    Scalar(LeafScalar),
    List(Vec<NestedValue>),
    Struct(Vec<(String, NestedValue)>),
}

/// One leaf column's decoded levels and values, one entry per physical
/// Dremel record (which for a repeated column can be more than one entry
/// per output row). `values[i]` is `Some` exactly when
/// `def_levels[i] == leaf.max_def_level`.
#[derive(Debug, Clone, Default)]
pub struct LeafEntries {
    pub rep_levels: Vec<u32>,
    pub def_levels: Vec<u32>,
    pub values: Vec<Option<LeafScalar>>,
}

#[derive(Debug)]
pub enum NestedError {
    MissingLeafData(String),
    UnsupportedListElement(String),
    UnsupportedMapShape(String),
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
        }
    }
}

impl std::error::Error for NestedError {}

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

    if node.element.repetition == Some(crate::column::parquet::footer::Repetition::Repeated) {
        // Reached directly on a repeated node only via recursion from a
        // list/map wrapper below -- top-level repeated fields go through
        // `build_list_field` instead.
        return build_list_field(node, path, num_rows, leaves);
    }

    if is_map(node) {
        return build_map_field(node, path, num_rows, leaves);
    }
    if node.children.len() == 1
        && node.children[0].element.repetition
            == Some(crate::column::parquet::footer::Repetition::Repeated)
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

    let mut out = Vec::with_capacity(num_rows);
    for row in 0..num_rows {
        if driver.def_levels[row] < node.max_def_level {
            out.push(NestedValue::Null);
        } else {
            let fields = children_values
                .iter()
                .map(|(name, vals)| (name.clone(), vals[row].clone()))
                .collect();
            out.push(NestedValue::Struct(fields));
        }
    }
    Ok(out)
}

fn first_leaf_path(node: &SchemaNode, path: &str) -> String {
    if node.is_leaf() {
        return path.to_string();
    }
    let child = &node.children[0];
    first_leaf_path(child, &format!("{path}.{}", child.name))
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
    let key_value = &node.children[0];
    if key_value.children.len() != 2 {
        return Err(NestedError::UnsupportedMapShape(path.to_string()));
    }
    let key_node = &key_value.children[0];
    let value_node = &key_value.children[1];
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
        num_rows,
        &keys.rep_levels,
        &keys.def_levels,
        |i| {
            NestedValue::Struct(vec![
                (
                    "key".to_string(),
                    keys.values[i]
                        .clone()
                        .map_or(NestedValue::Null, NestedValue::Scalar),
                ),
                (
                    "value".to_string(),
                    values.values[i]
                        .clone()
                        .map_or(NestedValue::Null, NestedValue::Scalar),
                ),
            ])
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
    let repeated = &node.children[0];
    let element = if repeated.is_leaf() {
        repeated
    } else {
        &repeated.children[0]
    };
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
        num_rows,
        &entries.rep_levels,
        &entries.def_levels,
        |i| {
            entries.values[i]
                .clone()
                .map_or(NestedValue::Null, NestedValue::Scalar)
        },
    )
}

/// Shared repetition-level walk for `LIST`/`MAP`: `wrapper` is the
/// enclosing (possibly optional) field, `repeated` is its one repeated
/// child. Groups entries by repetition-level-0 boundaries (one boundary
/// per output row) and calls `element_at(i)` to build each element from
/// whichever leaf column(s) actually hold the data.
fn build_repeated(
    wrapper: &SchemaNode,
    repeated: &SchemaNode,
    num_rows: usize,
    rep_levels: &[u32],
    def_levels: &[u32],
    element_at: impl Fn(usize) -> NestedValue,
) -> Result<Vec<NestedValue>> {
    let wrapper_def = wrapper.max_def_level;
    let repeated_def = repeated.max_def_level;
    let mut out = Vec::with_capacity(num_rows);
    let mut i = 0;
    for _ in 0..num_rows {
        let d = def_levels[i];
        if d < wrapper_def {
            out.push(NestedValue::Null);
            i += 1;
        } else if d < repeated_def {
            out.push(NestedValue::List(Vec::new()));
            i += 1;
        } else {
            let mut elems = Vec::new();
            loop {
                elems.push(element_at(i));
                i += 1;
                if i >= rep_levels.len() || rep_levels[i] == 0 {
                    break;
                }
            }
            out.push(NestedValue::List(elems));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use crate::column::parquet::footer::{PhysicalType, Repetition, SchemaElement};
    use crate::column::parquet::schema_tree::build_schema_tree;

    fn group(name: &str, repetition: Repetition, num_children: i32) -> SchemaElement {
        SchemaElement {
            name: name.to_string(),
            physical_type: None,
            repetition: Some(repetition),
            num_children: Some(num_children),
            type_length: None,
            converted_type: None,
            scale: None,
            precision: None,
        }
    }

    fn map_group(name: &str, repetition: Repetition, num_children: i32) -> SchemaElement {
        SchemaElement {
            converted_type: Some(ConvertedType::Other(1)),
            ..group(name, repetition, num_children)
        }
    }

    fn leaf_elem(name: &str, repetition: Repetition, physical_type: PhysicalType) -> SchemaElement {
        SchemaElement {
            name: name.to_string(),
            physical_type: Some(physical_type),
            repetition: Some(repetition),
            num_children: None,
            type_length: None,
            converted_type: None,
            scale: None,
            precision: None,
        }
    }

    fn entries(
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
