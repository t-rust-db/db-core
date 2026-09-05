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
    fn insert_master_row(
        &mut self,
        name: &str,
        sql: &str,
        root_page: u32,
    ) -> Result<(), SchemaStorageError>;

    /// Deletes `name`'s row from `sqlite_master`.
    fn delete_master_row(&mut self, name: &str) -> Result<(), SchemaStorageError>;

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
