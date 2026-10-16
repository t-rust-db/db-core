//! Why a table operation failed: shared by the writer ([`super::table`])
//! and the read side ([`super::snapshot`]).

use std::fmt;
use std::io;

use super::layout::LayoutKind;
use super::manifest::ManifestError;
use super::wal::WalError;
#[cfg(feature = "ingest")]
use crate::storage::parquet::writer::WriteError;
use crate::storage::stream::detect::Format;

/// Why a table operation failed.
#[derive(Debug)]
pub enum IngestError {
    /// The WAL failed.
    Wal(WalError),
    /// The manifest could not be read, written or validated.
    Manifest(ManifestError),
    /// Writing a Parquet file failed.
    #[cfg(feature = "ingest")]
    Parquet(WriteError),
    /// File system error.
    Io(io::Error),
    /// The record is not one non-empty line.
    InvalidRecord(String),
    /// The table exists with another record format.
    FormatMismatch {
        /// The table's format.
        table: Format,
        /// The format asked for.
        requested: Format,
    },
    /// The table exists with another layout.
    LayoutMismatch {
        /// The table's layout.
        table: LayoutKind,
        /// The layout asked for.
        requested: LayoutKind,
    },
    /// A record table's schema (or a tiered one's sort key) is not valid.
    InvalidSchema(String),
    /// The table exists with another schema, or is a lines table opened
    /// as a record table or the other way round.
    SchemaMismatch,
    /// The operation is not available for this table's layout or kind.
    Unsupported(String),
    /// A replayed WAL payload is not a record, or the parser rejected a
    /// page.
    Corrupt(String),
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestError::Wal(e) => write!(f, "{e}"),
            IngestError::Manifest(e) => write!(f, "{e}"),
            #[cfg(feature = "ingest")]
            IngestError::Parquet(e) => write!(f, "{e}"),
            IngestError::Io(e) => write!(f, "table: {e}"),
            IngestError::InvalidRecord(why) => write!(f, "invalid record: {why}"),
            IngestError::FormatMismatch { table, requested } => write!(
                f,
                "the table's format is {}, not {}",
                table.as_str(),
                requested.as_str()
            ),
            IngestError::LayoutMismatch { table, requested } => write!(
                f,
                "the table's layout is {}, not {}",
                table.as_str(),
                requested.as_str()
            ),
            IngestError::InvalidSchema(why) => write!(f, "invalid record schema: {why}"),
            IngestError::SchemaMismatch => {
                write!(f, "the table's schema is not the one asked for")
            }
            IngestError::Unsupported(why) => write!(f, "unsupported: {why}"),
            IngestError::Corrupt(why) => write!(f, "table: {why}"),
        }
    }
}

impl std::error::Error for IngestError {}

impl From<WalError> for IngestError {
    fn from(e: WalError) -> Self {
        IngestError::Wal(e)
    }
}

impl From<ManifestError> for IngestError {
    fn from(e: ManifestError) -> Self {
        IngestError::Manifest(e)
    }
}

#[cfg(feature = "ingest")]
impl From<WriteError> for IngestError {
    fn from(e: WriteError) -> Self {
        IngestError::Parquet(e)
    }
}

impl From<io::Error> for IngestError {
    fn from(e: io::Error) -> Self {
        IngestError::Io(e)
    }
}
