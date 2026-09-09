// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Exercises the `vm::row` storage-hook traits' default methods and error
//! types: `CursorFactory` (db-core#125), `SchemaStorage` (#128) and
//! `Transaction` (#81/#134). These traits are the ADR 0008 boundary a
//! downstream crate implements, so nothing in `src/` ever calls their
//! defaults -- without these tests the three files read as 0% covered.
//! Lives under `tests/unit` (outside `make check-mvl-limit`'s scan) so
//! the `Box<dyn ...>` a minimal implementor needs stays out of `src/`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use db_core::value::Collation;
use db_core::vm::row::{
    AnalyzeTarget, Cursor, CursorFactory, CursorFactoryError, InMemoryCursor, SchemaStorage,
    SchemaStorageError, SortKeyColumn, Transaction, TransactionError,
};

// --- CursorFactory ---------------------------------------------------------

/// Implements only the required `open_read`, so the `open_write`/
/// `open_index` defaults are what gets exercised.
struct ReadOnlyFactory {
    opened: Vec<u32>,
}

impl CursorFactory for ReadOnlyFactory {
    fn open_read(&mut self, root: u32) -> Result<Box<dyn Cursor>, CursorFactoryError> {
        if root == 0 {
            return Err(CursorFactoryError("root page 0 is not a table".to_string()));
        }
        self.opened.push(root);
        Ok(Box::new(InMemoryCursor::new(vec![])))
    }
}

#[test]
fn cursor_factory_open_write_defaults_to_open_read() {
    let mut f = ReadOnlyFactory { opened: vec![] };
    assert!(f.open_write(7).is_ok());
    assert_eq!(f.opened, vec![7]);
}

#[test]
fn cursor_factory_open_index_defaults_to_open_read_ignoring_key() {
    let mut f = ReadOnlyFactory { opened: vec![] };
    let key = [SortKeyColumn {
        index: 0,
        descending: true,
        collation: Collation::Binary,
        nulls_first: false,
    }];
    assert!(f.open_index(9, &key).is_ok());
    assert_eq!(f.opened, vec![9]);
}

#[test]
fn cursor_factory_defaults_propagate_open_read_errors() {
    let mut f = ReadOnlyFactory { opened: vec![] };
    let err = f.open_write(0).err();
    assert_eq!(
        err,
        Some(CursorFactoryError("root page 0 is not a table".to_string()))
    );
    assert!(f.open_index(0, &[]).is_err());
    assert!(f.opened.is_empty());
}

#[test]
fn cursor_factory_error_displays_its_message() {
    let err = CursorFactoryError("no such root".to_string());
    assert_eq!(err.to_string(), "no such root");
    let boxed: Box<dyn std::error::Error> = Box::new(err.clone());
    assert_eq!(boxed.to_string(), "no such root");
    assert_eq!(err.clone(), err);
}

// --- SchemaStorage ---------------------------------------------------------

/// Implements only the required methods, so `autoincrement_rowid`'s
/// refusing default is what gets exercised.
struct MinimalStorage;

impl SchemaStorage for MinimalStorage {
    fn create_table_root(&mut self) -> Result<u32, SchemaStorageError> {
        Ok(2)
    }
    fn create_index_root(&mut self) -> Result<u32, SchemaStorageError> {
        Ok(3)
    }
    fn populate_index(&mut self, _: u32, _: u32, _: &[usize]) -> Result<(), SchemaStorageError> {
        Ok(())
    }
    fn free_root(&mut self, _: u32) -> Result<(), SchemaStorageError> {
        Ok(())
    }
    fn insert_master_row(
        &mut self,
        _: &str,
        _: &str,
        _: &str,
        _: u32,
        _: &str,
    ) -> Result<(), SchemaStorageError> {
        Ok(())
    }
    fn delete_master_row(&mut self, _: &str) -> Result<(), SchemaStorageError> {
        Ok(())
    }
    fn bump_schema_cookie(&mut self) -> Result<(), SchemaStorageError> {
        Ok(())
    }
    fn write_stat1(&mut self, _: &AnalyzeTarget) -> Result<(), SchemaStorageError> {
        Ok(())
    }
}

#[test]
fn schema_storage_autoincrement_rowid_default_refuses() {
    let err = MinimalStorage.autoincrement_rowid("t", 41).err();
    assert!(
        err.as_ref()
            .is_some_and(|e| e.0.contains("AUTOINCREMENT requires a SchemaStorage")),
        "unexpected result: {err:?}"
    );
}

#[test]
fn schema_storage_error_displays_its_message() {
    let err = SchemaStorageError("page allocation failed".to_string());
    assert_eq!(err.to_string(), "page allocation failed");
    let boxed: Box<dyn std::error::Error> = Box::new(err.clone());
    assert_eq!(boxed.to_string(), "page allocation failed");
    assert_eq!(err.clone(), err);
}

// --- Transaction -----------------------------------------------------------

/// Implements only `begin`/`commit`/`rollback`, so every #134 default
/// (`set_journal_mode`, `synchronous`, `set_synchronous`,
/// `integrity_check`) is what gets exercised.
struct MinimalHook {
    log: Vec<&'static str>,
}

impl Transaction for MinimalHook {
    fn begin(&mut self, _mode: i32) -> Result<(), TransactionError> {
        self.log.push("begin");
        Ok(())
    }
    fn commit(&mut self) -> Result<(), TransactionError> {
        self.log.push("commit");
        Ok(())
    }
    fn rollback(&mut self) -> Result<(), TransactionError> {
        self.log.push("rollback");
        Err(TransactionError("nothing to roll back".to_string()))
    }
}

#[test]
fn transaction_required_methods_dispatch_to_the_hook() {
    let mut h = MinimalHook { log: vec![] };
    assert_eq!(h.begin(0), Ok(()));
    assert_eq!(h.commit(), Ok(()));
    assert_eq!(
        h.rollback(),
        Err(TransactionError("nothing to roll back".to_string()))
    );
    assert_eq!(h.log, vec!["begin", "commit", "rollback"]);
}

#[test]
fn transaction_journal_and_synchronous_defaults_are_no_ops() {
    let mut h = MinimalHook { log: vec![] };
    assert_eq!(h.set_journal_mode(1), Ok(()));
    assert_eq!(h.synchronous(), None);
    assert_eq!(h.set_synchronous(2), Ok(()));
    assert!(h.log.is_empty());
}

#[test]
fn transaction_integrity_check_default_is_unimplemented() {
    let mut h = MinimalHook { log: vec![] };
    assert!(h.integrity_check(true).is_none());
    assert!(h.integrity_check(false).is_none());
}

#[test]
fn transaction_error_displays_its_message() {
    let err = TransactionError("disk I/O error".to_string());
    assert_eq!(err.to_string(), "disk I/O error");
    let boxed: Box<dyn std::error::Error> = Box::new(err.clone());
    assert_eq!(boxed.to_string(), "disk I/O error");
    assert_eq!(err.clone(), err);
}
