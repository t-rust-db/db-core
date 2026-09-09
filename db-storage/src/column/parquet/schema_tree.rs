//! Builds a tree from Parquet's flattened `FileMetaData.schema` list and
//! computes each leaf's max definition/repetition level (the Dremel
//! encoding rule: +1 definition level per optional-or-repeated ancestor,
//! +1 repetition level per repeated ancestor).

use crate::column::parquet::footer::{Repetition, SchemaElement};

#[derive(Debug, Clone)]
pub struct SchemaNode {
    pub name: String,
    pub element: SchemaElement,
    pub children: Vec<SchemaNode>,
    pub max_def_level: u32,
    pub max_rep_level: u32,
}

impl SchemaNode {
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// Leaves in schema (on-disk column) order.
    pub fn leaves(&self) -> Vec<&SchemaNode> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a SchemaNode>) {
        if self.is_leaf() {
            out.push(self);
        } else {
            for child in &self.children {
                child.collect_leaves(out);
            }
        }
    }
}

/// Dot-joined path (from each root child's own name down to the leaf) for
/// every leaf, in the same order as [`SchemaNode::leaves`] (i.e. on-disk
/// column order).
pub fn leaf_paths(root: &SchemaNode) -> Vec<String> {
    let mut out = Vec::new();
    for child in &root.children {
        collect_paths(child, child.name.clone(), &mut out);
    }
    out
}

fn collect_paths(node: &SchemaNode, path: String, out: &mut Vec<String>) {
    if node.is_leaf() {
        out.push(path);
    } else {
        for child in &node.children {
            collect_paths(child, format!("{path}.{}", child.name), out);
        }
    }
}

/// Build the schema tree from the flattened schema list. `schema[0]` is
/// the root (message) node.
pub fn build_schema_tree(schema: &[SchemaElement]) -> SchemaNode {
    let mut idx = 0;
    build_node(schema, &mut idx, 0, 0, true)
}

fn build_node(
    schema: &[SchemaElement],
    idx: &mut usize,
    parent_def: u32,
    parent_rep: u32,
    is_root: bool,
) -> SchemaNode {
    let element = schema[*idx].clone();
    *idx += 1;

    let (def_level, rep_level) = if is_root {
        (0, 0)
    } else {
        let def = parent_def + u32::from(element.repetition != Some(Repetition::Required));
        let rep = parent_rep + u32::from(element.repetition == Some(Repetition::Repeated));
        (def, rep)
    };

    let num_children = element.num_children.unwrap_or(0) as usize;
    let mut children = Vec::with_capacity(num_children);
    for _ in 0..num_children {
        children.push(build_node(schema, idx, def_level, rep_level, false));
    }

    SchemaNode {
        name: element.name.clone(),
        element,
        children,
        max_def_level: def_level,
        max_rep_level: rep_level,
    }
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
    use crate::column::parquet::footer::PhysicalType;

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

    fn leaf(name: &str, repetition: Repetition, physical_type: PhysicalType) -> SchemaElement {
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

    #[test]
    fn flat_required_column_has_zero_levels() {
        let schema = vec![
            group("root", Repetition::Required, 1),
            leaf("id", Repetition::Required, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let leaves = tree.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].max_def_level, 0);
        assert_eq!(leaves[0].max_rep_level, 0);
    }

    #[test]
    fn optional_column_has_def_level_one() {
        let schema = vec![
            group("root", Repetition::Required, 1),
            leaf("id", Repetition::Optional, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let leaves = tree.leaves();
        assert_eq!(leaves[0].max_def_level, 1);
        assert_eq!(leaves[0].max_rep_level, 0);
    }

    #[test]
    fn repeated_field_under_optional_group_matches_dremel_example() {
        // message { optional group a { repeated int32 b } }
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("a", Repetition::Optional, 1),
            leaf("b", Repetition::Repeated, PhysicalType::Int32),
        ];
        let tree = build_schema_tree(&schema);
        let leaves = tree.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].name, "b");
        // a is optional (+1 def), b is repeated (+1 def, +1 rep)
        assert_eq!(leaves[0].max_def_level, 2);
        assert_eq!(leaves[0].max_rep_level, 1);
    }

    #[test]
    fn deeply_nested_struct_accumulates_levels() {
        let schema = vec![
            group("root", Repetition::Required, 1),
            group("a", Repetition::Optional, 1),
            group("b", Repetition::Optional, 1),
            leaf("c", Repetition::Required, PhysicalType::Int64),
        ];
        let tree = build_schema_tree(&schema);
        let leaves = tree.leaves();
        assert_eq!(leaves[0].max_def_level, 2);
        assert_eq!(leaves[0].max_rep_level, 0);
    }
}
