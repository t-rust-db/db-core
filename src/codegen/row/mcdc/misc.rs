// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Vectors for the two files db-core itself owns in the moved tree:
//! `dispatch.rs` (the INSERT ... SELECT CTE/view guard) and `row.rs`
//! (`TableSchema::with_computed_rowid_alias`).

use crate::codegen::row::dispatch::{compile_statement, DispatchError};
use crate::codegen::row::{CodegenError, TableSchema, ViewSchema};

fn table(name: &str, root_page: u32, columns: &[&str]) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        root_page,
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        column_types: columns.iter().map(|_| "INTEGER".to_string()).collect(),
        sql: format!("CREATE TABLE {name} ({})", columns.join(", ")),
        ..Default::default()
    }
}

fn catalog() -> (Vec<TableSchema>, Vec<ViewSchema>) {
    let schemas = vec![
        table("t", 2, &["a"]),
        table("u", 3, &["a"]),
        table("w", 4, &["a"]),
    ];
    let views = vec![ViewSchema {
        name: "v".to_string(),
        sql: "CREATE VIEW v AS SELECT a FROM u".to_string(),
    }];
    (schemas, views)
}

fn is_view_source_rejection(result: Result<crate::vm::row::Program, DispatchError>) -> bool {
    matches!(
        result,
        Err(DispatchError::Codegen(CodegenError::Unsupported { reason }))
            if reason.contains("CTE or view source")
    )
}

// dispatch_269: `is_subquery(&from.first) || from.joins.iter().any(|j| is_subquery(&j.table))`

#[test]
fn mcdc__dispatch_269__v1_view_as_first_source_is_rejected() {
    let (schemas, views) = catalog();
    assert!(is_view_source_rejection(compile_statement(
        "INSERT INTO t SELECT a FROM v",
        &schemas,
        &views
    )));
}

#[test]
fn mcdc__dispatch_269__v2_view_as_joined_source_is_rejected() {
    let (schemas, views) = catalog();
    assert!(is_view_source_rejection(compile_statement(
        "INSERT INTO t SELECT u.a FROM u JOIN v ON u.a = v.a",
        &schemas,
        &views
    )));
}

#[test]
fn mcdc__dispatch_269__v3_plain_table_sources_pass_the_guard() {
    let (schemas, views) = catalog();
    let result = compile_statement(
        "INSERT INTO t SELECT u.a FROM u JOIN w ON u.a = w.a",
        &schemas,
        &views,
    );
    assert!(!is_view_source_rejection(result));
}

// row_688: `self.is_virtual || self.without_rowid`

fn ipk_table() -> TableSchema {
    TableSchema {
        name: "t".to_string(),
        columns: vec!["id".to_string(), "x".to_string()],
        sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, x)".to_string(),
        ..Default::default()
    }
}

#[test]
fn mcdc__schema_66__v1_virtual_table_has_no_rowid_alias() {
    let schema = TableSchema {
        is_virtual: true,
        ..ipk_table()
    };
    assert_eq!(schema.with_computed_rowid_alias().rowid_alias, None);
}

#[test]
fn mcdc__schema_66__v2_without_rowid_table_has_no_rowid_alias() {
    let schema = TableSchema {
        without_rowid: true,
        ..ipk_table()
    };
    assert_eq!(schema.with_computed_rowid_alias().rowid_alias, None);
}

#[test]
fn mcdc__schema_66__v3_ordinary_table_computes_the_alias_from_sql() {
    assert_eq!(ipk_table().with_computed_rowid_alias().rowid_alias, Some(0));
}

// row_718: `inline_pk && is_integer(def)`

fn alias_of(sql: &str) -> Option<usize> {
    TableSchema {
        name: "t".to_string(),
        sql: sql.to_string(),
        ..Default::default()
    }
    .with_computed_rowid_alias()
    .rowid_alias
}

#[test]
fn mcdc__schema_105__v1_integer_primary_key_column_is_the_alias() {
    assert_eq!(
        alias_of("CREATE TABLE t (x, id INTEGER PRIMARY KEY)"),
        Some(1)
    );
}

#[test]
fn mcdc__schema_105__v2_non_integer_primary_key_is_not_an_alias() {
    assert_eq!(alias_of("CREATE TABLE t (x, id TEXT PRIMARY KEY)"), None);
}

#[test]
fn mcdc__schema_105__v3_integer_column_without_primary_key_is_not_an_alias() {
    assert_eq!(alias_of("CREATE TABLE t (x, id INTEGER)"), None);
}
