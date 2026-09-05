//! The cursor-opening hook a consumer's pager installs so `Opcode::
//! OpenRead`/`OpenWrite` can resolve a `p2` root page to a real
//! [`super::cursor::Cursor`] at run time (db-core#125). Without this, a
//! consumer would have to pre-wire every cursor slot a program might
//! ever open via `Vm::open_cursor` before running it -- impossible in
//! general, since sqlite-rs's codegen opens cursors at arbitrary points
//! in a program, sometimes re-opening the same slot for a different
//! root page. With a factory installed (`Vm::set_cursor_factory`),
//! `OpenRead`/`OpenWrite` call it with the root page instead; with none
//! installed, the pre-wired path (`Vm::open_cursor` called ahead of
//! time, matching earlier phases) keeps working unchanged.

use super::program::SortKeyColumn;

/// Installed via [`super::vm::Vm::set_cursor_factory`] to resolve
/// `OpenRead`/`OpenWrite`'s `p2` root page (and `OpenDup`'s re-open of
/// an already-open slot's root) to a real [`super::cursor::Cursor`].
/// `db-core` has no pager of its own (ADR 0008/0006) -- a consumer
/// (e.g. t-rust-db/sqlite-rs) implements this over its own b-tree.
pub trait CursorFactory {
    /// Opens a read-only cursor onto the table/index rooted at `root`.
    fn open_read(
        &mut self,
        root: u32,
    ) -> Result<Box<dyn super::cursor::Cursor>, CursorFactoryError>;

    /// Opens a read/write cursor onto the table/index rooted at `root`.
    /// Default: same as [`Self::open_read`] -- a consumer with no
    /// separate read/write cursor kind (or a read-only adapter under
    /// test) doesn't need to override this.
    fn open_write(
        &mut self,
        root: u32,
    ) -> Result<Box<dyn super::cursor::Cursor>, CursorFactoryError> {
        self.open_read(root)
    }

    /// Opens a read-only cursor onto the *index* rooted at `root`,
    /// whose entries are keyed per `key`
    /// (`OpenRead`/`OpenWrite`'s `p4` carrying a [`super::program::P4::
    /// SortKey`] descriptor signals index mode, per this module's own
    /// doc). Default: falls back to [`Self::open_read`] -- a consumer
    /// that has no distinct index-cursor kind yet (or doesn't need one
    /// for the programs it runs) doesn't have to override this either.
    fn open_index(
        &mut self,
        root: u32,
        key: &[SortKeyColumn],
    ) -> Result<Box<dyn super::cursor::Cursor>, CursorFactoryError> {
        let _ = key;
        self.open_read(root)
    }
}

/// Why a [`CursorFactory`] call failed -- deliberately just a carried
/// message, same rationale as [`super::transaction::TransactionError`]:
/// the factory lives in the consumer, which knows its own failure modes
/// far better than this crate could model generically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorFactoryError(pub String);

impl std::fmt::Display for CursorFactoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CursorFactoryError {}
