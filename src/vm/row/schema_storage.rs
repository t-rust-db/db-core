//! The schema-write hook a consumer's pager installs so `CreateTable`/
//! `CreateIndex`/`DropTable`/`DropIndex`/`Analyze` can actually run
//! (db-core#128). Each of these opcodes in sqlite-rs drives real
//! b-tree/`sqlite_master` operations (`btree::{create_empty_table_root,
//! create_empty_index_root, populate_index_from_table,
//! free_btree_pages, insert_master_row, delete_master_row,
//! bump_schema_cookie, insert_stat1_row, ...}`) that `db-core` must not
//! depend on directly (ADR 0008) -- this trait is the storage-agnostic
//! boundary a consumer implements over its own schema/master-table
//! storage.

use super::program::AnalyzeTarget;

/// Installed via [`super::vm::Vm::set_schema_storage`] to back
/// `CreateTable`/`CreateIndex`/`DropTable`/`DropIndex`/`Analyze`. With
/// no hook installed, those opcodes fail with
/// [`super::vm::ExecError::SchemaStorageMissing`].
pub trait SchemaStorage {
    /// Allocates a new, empty table b-tree, returning its root page.
    fn create_table_root(&mut self) -> Result<u32, SchemaStorageError>;

    /// Allocates a new, empty index b-tree, returning its root page.
    fn create_index_root(&mut self) -> Result<u32, SchemaStorageError>;

    /// Populates the index rooted at `index_root` from every row of the
    /// table rooted at `table_root`, indexing the columns at
    /// `column_indices` (0-based, table-column order).
    fn populate_index(
        &mut self,
        index_root: u32,
        table_root: u32,
        column_indices: &[usize],
    ) -> Result<(), SchemaStorageError>;

    /// Frees every page of the b-tree rooted at `root` (`DROP TABLE`/
    /// `DROP INDEX`'s storage reclamation).
    fn free_root(&mut self, root: u32) -> Result<(), SchemaStorageError>;

    /// Inserts `name`'s row into `sqlite_master`, with `sql` as its
    /// verbatim source text and `root_page` as its b-tree root.
    /// One `sqlite_master` row: `kind` is `"table"`/`"index"`/`"view"`,
    /// `tbl_name` the table it belongs to (`name` itself for a table or
    /// view, the indexed table for an index), `root_page` 0 for a view
    /// (sqlite-rs `MasterEntry`, #134).
    fn insert_master_row(
        &mut self,
        kind: &str,
        name: &str,
        tbl_name: &str,
        root_page: u32,
        sql: &str,
    ) -> Result<(), SchemaStorageError>;

    /// Inserts the `sqlite_master` row of an implicit `PRIMARY KEY`/
    /// `UNIQUE` index (`sqlite_autoindex_<table>_<n>`): type `index`,
    /// `tbl_name` the indexed table, and a `NULL` `sql` column, as
    /// sqlite3 writes it (#694). The default refuses, so a storage that
    /// predates implicit indexes fails the `CREATE TABLE` instead of
    /// writing a file sqlite3 calls malformed.
    fn insert_autoindex_row(
        &mut self,
        name: &str,
        tbl_name: &str,
        root_page: u32,
    ) -> Result<(), SchemaStorageError> {
        let _ = (name, tbl_name, root_page);
        Err(SchemaStorageError(
            "implicit PRIMARY KEY/UNIQUE indexes require a SchemaStorage that implements \
             insert_autoindex_row"
                .to_string(),
        ))
    }

    /// Deletes the `sqlite_master` row of type `kind` (`table`, `index`,
    /// `view`) named `name` -- by type and name, so a same-named object
    /// of another kind is never the one removed (#658).
    fn delete_master_row(&mut self, kind: &str, name: &str) -> Result<(), SchemaStorageError>;

    /// Bumps the schema cookie, invalidating any cached schema/prepared
    /// statements that read it.
    fn bump_schema_cookie(&mut self) -> Result<(), SchemaStorageError>;

    /// Computes and writes `sqlite_stat1` rows for `target`'s table and
    /// every index on it (`ANALYZE`). Takes the whole
    /// [`AnalyzeTarget`] descriptor -- table/index names and root
    /// pages -- so the hook (which owns the actual b-tree access) can
    /// walk them itself; `db-core` has no way to compute a real stat
    /// row without depending on storage (ADR 0008).
    fn write_stat1(&mut self, target: &AnalyzeTarget) -> Result<(), SchemaStorageError>;

    /// `NewRowid` with `p5 != 0` (AUTOINCREMENT, `P4::Str(table)`): the
    /// next rowid for `table` given the table's current maximum rowid,
    /// honouring and updating `sqlite_sequence` so rowids are never
    /// reused (sqlite-rs `vdbe::cursor::new_rowid`, #134). The default
    /// refuses, since db-core has no schema storage of its own.
    fn autoincrement_rowid(
        &mut self,
        table: &str,
        max_from_table: i64,
    ) -> Result<i64, SchemaStorageError> {
        let _ = (table, max_from_table);
        Err(SchemaStorageError(
            "AUTOINCREMENT requires a SchemaStorage that implements autoincrement_rowid"
                .to_string(),
        ))
    }
}

/// Why a [`SchemaStorage`] call failed -- deliberately just a carried
/// message, same rationale as [`super::transaction::TransactionError`]
/// and [`super::cursor_factory::CursorFactoryError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaStorageError(pub String);

impl std::fmt::Display for SchemaStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SchemaStorageError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A storage that implements only the required methods, so the
    /// provided ones answer with their defaults.
    struct Minimal;

    impl SchemaStorage for Minimal {
        fn create_table_root(&mut self) -> Result<u32, SchemaStorageError> {
            Ok(2)
        }
        fn create_index_root(&mut self) -> Result<u32, SchemaStorageError> {
            Ok(3)
        }
        fn populate_index(
            &mut self,
            _: u32,
            _: u32,
            _: &[usize],
        ) -> Result<(), SchemaStorageError> {
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
        fn delete_master_row(&mut self, _: &str, _: &str) -> Result<(), SchemaStorageError> {
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
    fn the_required_methods_are_the_ones_a_storage_provides() {
        let mut storage = Minimal;
        assert_eq!(storage.create_table_root(), Ok(2));
        assert_eq!(storage.create_index_root(), Ok(3));
        assert_eq!(storage.populate_index(3, 2, &[0]), Ok(()));
        assert_eq!(storage.free_root(3), Ok(()));
        assert_eq!(
            storage.insert_master_row("table", "t", "t", 2, "sql"),
            Ok(())
        );
        assert_eq!(storage.delete_master_row("table", "t"), Ok(()));
        assert_eq!(storage.bump_schema_cookie(), Ok(()));
        let target = AnalyzeTarget {
            table_name: "t".to_string(),
            table_root_page: 2,
            indexes: Vec::new(),
        };
        assert_eq!(storage.write_stat1(&target), Ok(()));
    }

    #[test]
    fn a_storage_without_implicit_index_support_refuses_instead_of_writing_a_malformed_file() {
        let err = Minimal
            .insert_autoindex_row("sqlite_autoindex_t_1", "t", 3)
            .unwrap_err();
        assert!(err.0.contains("insert_autoindex_row"), "{err}");
    }

    #[test]
    fn a_storage_without_autoincrement_support_refuses() {
        let err = Minimal.autoincrement_rowid("t", 7).unwrap_err();
        assert!(err.0.contains("autoincrement_rowid"), "{err}");
    }
}
