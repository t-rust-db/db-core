// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The row schema catalog types -- `TableSchema`, `IndexSchema`,
//! `IndexedColumn`, `ViewSchema` -- defined once here and consumed by both
//! `codegen::row` (which re-exports them) and db-storage's DDL reader
//! (which re-exports them the way it re-exports [`crate::value::Value`],
//! ADR 0010). One type means a schema read from a database file by
//! db-storage is handed to the planner without conversion (ADR 0014,
//! t-rust-db/sqlite-rs#19).
//!
//! A leaf module like [`crate::value`]: no feature gate, no dependency on
//! the parser or the VM. Only [`TableSchema::with_computed_rowid_alias`]
//! needs the SQL parser (it re-parses the `CREATE TABLE` text to find an
//! `INTEGER PRIMARY KEY`), so that impl is gated on `parser-row`.

/// A table's schema as codegen sees it -- the same shape as
/// `db_storage::row::schema::TableSchema` (ADR 0012 keeps the two in
/// lock-step, field for field, because db-core may not depend on
/// db-storage -- ADR 0008). Callers build it by literal construction or
/// convert from db-storage's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableSchema {
    /// The table's name.
    pub name: String,
    /// The table's b-tree root page (`sqlite_master.rootpage`).
    pub root_page: u32,
    /// Column names, in declared order.
    pub columns: Vec<String>,
    /// Whether the table was declared `WITHOUT ROWID`.
    pub without_rowid: bool,
    /// Whether the table was declared `STRICT`.
    pub strict: bool,
    /// Each column's declared type text, position-for-position with
    /// `columns` (empty string when a column has none) -- what
    /// [`crate::vm::row::affinity_of`] derives column affinity from.
    pub column_types: Vec<String>,
    /// Each column's declared `COLLATE` (default `Collation::Binary`
    /// when absent), position-for-position with `columns`.
    pub column_collations: Vec<crate::value::Collation>,
    /// `CREATE VIRTUAL TABLE ...`: `columns` is empty and `root_page`
    /// is `0`.
    pub is_virtual: bool,
    /// The verbatim `CREATE TABLE` statement text.
    pub sql: String,
    /// Every `CREATE INDEX`/`CREATE UNIQUE INDEX` on this table.
    pub indexes: Vec<IndexSchema>,
    /// The rowid-alias column index (0-based into `columns`) -- SQLite's
    /// single-`INTEGER PRIMARY KEY` special case. Codegen reads this
    /// field per column reference, so it is a field, not a re-parse of
    /// `sql` on every call.
    pub rowid_alias: Option<usize>,
}

#[cfg(feature = "parser-row")]
impl TableSchema {
    /// Recomputes [`TableSchema::rowid_alias`] from `sql`/`without_rowid`
    /// -- for callers (tests, synthetic schemas) that build a
    /// `TableSchema` literal by hand instead of decoding one from
    /// `sqlite_master`. Unlike db-storage's string-scanning original,
    /// this one asks the crate's own parser: a column-level `INTEGER
    /// PRIMARY KEY`, or a table-level `PRIMARY KEY(c)` naming the one and
    /// only column when that column is `INTEGER`-typed. Unparseable SQL
    /// yields `None`.
    #[must_use]
    pub fn with_computed_rowid_alias(mut self) -> Self {
        self.rowid_alias = if self.is_virtual || self.without_rowid {
            None
        } else {
            rowid_alias_from_sql(&self.sql)
        };
        self
    }
}

#[cfg(feature = "parser-row")]
fn is_ascending_primary_key(constraint: &crate::parser::ast::ColumnConstraint) -> bool {
    match constraint {
        crate::parser::ast::ColumnConstraint::PrimaryKey { desc, .. } => *desc != Some(true),
        _ => false,
    }
}

#[cfg(feature = "parser-row")]
fn rowid_alias_from_sql(sql: &str) -> Option<usize> {
    use crate::parser::ast::{ExprKind, TableConstraint};
    use crate::parser::row::error::ParseOutcome;

    let create = match crate::parser::row::parse_create_table(sql) {
        ParseOutcome::Accepted(create) => *create,
        _ => return None,
    };
    if create.without_rowid {
        return None;
    }
    let is_integer = |def: &crate::parser::ast::ColumnDef| {
        def.type_name
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("INTEGER"))
    };
    for (idx, def) in create.columns.iter().enumerate() {
        // `INTEGER PRIMARY KEY DESC` is *not* a rowid alias: SQLite gives it
        // its own index and stores the column normally ("ROWIDs and the
        // INTEGER PRIMARY KEY"), so only an ASC/unspecified key qualifies.
        let inline_pk = def.constraints.iter().any(is_ascending_primary_key);
        if inline_pk && is_integer(def) {
            return Some(idx);
        }
    }
    if let [only] = create.columns.as_slice() {
        if is_integer(only) {
            let named = create.constraints.iter().any(|c| match c {
                TableConstraint::PrimaryKey(cols) => match cols.as_slice() {
                    [col] => matches!(&col.expr.kind, ExprKind::Column { name, .. } if name.eq_ignore_ascii_case(&only.name)),
                    _ => false,
                },
                _ => false,
            });
            if named {
                return Some(0);
            }
        }
    }
    None
}

/// A `CREATE INDEX` entry, same shape as
/// `db_storage::row::schema::IndexSchema` (ADR 0012).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexSchema {
    /// The index's name.
    pub name: String,
    /// Whether the index was declared `UNIQUE`.
    pub unique: bool,
    /// The indexed columns, in declared key order.
    pub columns: Vec<IndexedColumn>,
    /// The index b-tree's root page (`sqlite_master.rootpage`).
    pub root_page: u32,
}

/// One column (or expression, kept as raw text) in an index's key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexedColumn {
    /// The column's (unquoted) name, or raw expression text for an
    /// expression index.
    pub name: String,
    /// Whether this key part is sorted `DESC`.
    pub desc: bool,
    /// The key part's declared `COLLATE` (default `Collation::Binary`).
    pub collation: crate::value::Collation,
}

/// A `CREATE VIEW` entry, same shape as
/// `db_storage::row::schema::ViewSchema` (ADR 0012): kept as verbatim
/// SQL and re-parsed by [`subquery::resolve_views`] on demand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewSchema {
    /// The view's name.
    pub name: String,
    /// The verbatim `CREATE VIEW ...` source text.
    pub sql: String,
}

#[cfg(all(test, feature = "parser-row"))]
mod tests {
    use super::TableSchema;

    fn alias(sql: &str) -> Option<usize> {
        TableSchema {
            sql: sql.to_string(),
            ..TableSchema::default()
        }
        .with_computed_rowid_alias()
        .rowid_alias
    }

    #[test]
    fn integer_primary_key_is_the_alias() {
        assert_eq!(
            alias("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)"),
            Some(0)
        );
    }

    #[test]
    fn integer_primary_key_desc_is_not_an_alias() {
        // Found by sqlite-rs's dump tests after the type moved here (ADR 0014).
        assert_eq!(
            alias("CREATE TABLE t (id INTEGER PRIMARY KEY DESC, name TEXT)"),
            None
        );
    }

    #[test]
    fn string_literal_table_name_still_yields_the_alias() {
        // FTS5 shadow tables are created as `CREATE TABLE 't_data'(...)`.
        assert_eq!(
            alias("CREATE TABLE 't_data'(id INTEGER PRIMARY KEY, block BLOB)"),
            Some(0)
        );
    }
}
