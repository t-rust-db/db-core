//! Builds a tree from Parquet's flattened `FileMetaData.schema` list and
//! computes each leaf's max definition/repetition level (the Dremel
//! encoding rule: +1 definition level per optional-or-repeated ancestor,
//! +1 repetition level per repeated ancestor).

use crate::storage::parquet::footer::{FooterError, Repetition, SchemaElement};

/// Deepest schema nesting accepted, root excluded. Real nested data stays
/// far below this; a deeper chain is hostile input that would otherwise
/// recurse through [`build_schema_tree`] and the nested reader until the
/// stack overflows (#657).
pub const MAX_SCHEMA_DEPTH: usize = 64;

/// One node of the schema tree: a group (struct, list, map) or a leaf
/// column.
#[derive(Debug, Clone)]
pub struct SchemaNode {
    /// The element's name.
    pub name: String,
    /// The flattened schema entry this node was built from.
    pub element: SchemaElement,
    /// Child nodes in schema order; empty for a leaf.
    pub children: Vec<SchemaNode>,
    /// Max definition level: optional-or-repeated ancestors, self included.
    pub max_def_level: u32,
    /// Max repetition level: repeated ancestors, self included.
    pub max_rep_level: u32,
}

impl SchemaNode {
    /// Whether this node has no children (a column chunk).
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

/// A node's child count as [`build_schema_tree`] reads it: absent or
/// negative is none.
fn child_count(element: &SchemaElement) -> usize {
    usize::try_from(element.num_children.unwrap_or(0)).unwrap_or(0)
}

/// Check that the flattened schema list describes a tree
/// [`build_schema_tree`] can build in full: a root exists, every
/// `num_children` is backed by enough following entries, and nesting stays
/// within [`MAX_SCHEMA_DEPTH`]. Iterative, so a hostile list cannot
/// overflow the stack here either.
pub fn check_schema(schema: &[SchemaElement]) -> Result<(), FooterError> {
    let root = schema
        .first()
        .ok_or(FooterError::InvalidSchema("schema list is empty"))?;
    // Children still to visit, per open group.
    let mut open = vec![child_count(root)];
    let mut rest = schema.iter().skip(1);
    while let Some(left) = open.last_mut() {
        let Some(next_left) = left.checked_sub(1) else {
            open.pop();
            continue;
        };
        *left = next_left;
        let element = rest.next().ok_or(FooterError::InvalidSchema(
            "num_children runs past the end of the schema list",
        ))?;
        let children = child_count(element);
        if children > 0 {
            if open.len() >= MAX_SCHEMA_DEPTH {
                return Err(FooterError::InvalidSchema("schema nested too deeply"));
            }
            open.push(children);
        }
    }
    Ok(())
}

/// Build the schema tree from the flattened schema list. `schema[0]` is
/// the root (message) node.
///
/// Expects a list [`check_schema`] accepts, as `footer::parse_footer`
/// guarantees. On a malformed list it never panics: an empty list yields an
/// empty root, and children past the end of the list or beyond
/// [`MAX_SCHEMA_DEPTH`] are left out.
pub fn build_schema_tree(schema: &[SchemaElement]) -> SchemaNode {
    let mut idx = 0;
    build_node(schema, &mut idx, 0, 0, 0).unwrap_or_else(|| SchemaNode {
        name: String::new(),
        element: SchemaElement::default(),
        children: Vec::new(),
        max_def_level: 0,
        max_rep_level: 0,
    })
}

/// The node at `schema[*idx]` and its subtree at `depth` (0 = root), or
/// `None` when the list has run out.
fn build_node(
    schema: &[SchemaElement],
    idx: &mut usize,
    parent_def: u32,
    parent_rep: u32,
    depth: usize,
) -> Option<SchemaNode> {
    let element = schema.get(*idx)?.clone();
    *idx = idx.checked_add(1)?;

    let (def_level, rep_level) = if depth == 0 {
        (0, 0)
    } else {
        let def =
            parent_def.checked_add(u32::from(element.repetition != Some(Repetition::Required)))?;
        let rep =
            parent_rep.checked_add(u32::from(element.repetition == Some(Repetition::Repeated)))?;
        (def, rep)
    };

    let mut children = Vec::new();
    if depth < MAX_SCHEMA_DEPTH {
        // Each child takes at least one entry, so the rest of the list
        // bounds the allocation whatever `num_children` claims.
        let num_children = child_count(&element).min(schema.len().saturating_sub(*idx));
        children.reserve(num_children);
        let child_depth = depth.checked_add(1)?;
        for _ in 0..num_children {
            match build_node(schema, idx, def_level, rep_level, child_depth) {
                Some(child) => children.push(child),
                None => break,
            }
        }
    }

    Some(SchemaNode {
        name: element.name.clone(),
        element,
        children,
        max_def_level: def_level,
        max_rep_level: rep_level,
    })
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
    use crate::storage::parquet::footer::PhysicalType;

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
        assert!(check_schema(&schema).is_ok());
    }

    /// #657: malformed lists `parse_footer` rejects; a direct caller of
    /// `build_schema_tree` gets a truncated tree, not a panic.
    #[test]
    fn check_schema_rejects_malformed_lists() {
        let short = vec![
            group("root", Repetition::Required, 2),
            leaf("a", Repetition::Required, PhysicalType::Int64),
        ];
        let huge = vec![group("root", Repetition::Required, i32::MAX)];
        for schema in [vec![], short, huge] {
            assert!(matches!(
                check_schema(&schema),
                Err(FooterError::InvalidSchema(_))
            ));
            let tree = build_schema_tree(&schema);
            assert!(tree.children.len() <= schema.len());
        }
    }

    #[test]
    fn negative_num_children_is_a_leaf() {
        let schema = vec![group("root", Repetition::Required, -5)];
        assert!(check_schema(&schema).is_ok());
        assert!(build_schema_tree(&schema).is_leaf());
    }

    /// A deep chain is rejected by the check and cut at the cap by the
    /// builder, which no longer recurses once per element.
    #[test]
    fn deep_chain_is_rejected_and_built_without_overflow() {
        let mut schema = vec![group("root", Repetition::Required, 1)];
        schema.extend((0..200_000).map(|_| group("g", Repetition::Optional, 1)));
        schema.push(leaf("x", Repetition::Required, PhysicalType::Int64));
        assert!(matches!(
            check_schema(&schema),
            Err(FooterError::InvalidSchema(_))
        ));
        let tree = build_schema_tree(&schema);
        let leaves = tree.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].max_def_level, MAX_SCHEMA_DEPTH as u32);
    }
}
