//! A read view of a table (ADR 0027, #572): the manifest's files plus the
//! records above its `flushed_lsn`, as pages.
//!
//! Every record is in exactly one place: files hold LSNs `1..=flushed_lsn`,
//! pages the ones above. A snapshot is fixed once taken; a query that
//! wants newer records takes a new one.
//!
//! - In the writer's process, [`TableWriter::snapshot`] copies the
//!   writer's pages (sealed ones are shared, the head is re-parsed), so the
//!   view reaches the last append (`visible`).
//! - From another process, [`Snapshot::open`] reads the manifest, then the
//!   WAL above `flushed_lsn` without changing it. That reaches every frame
//!   the writer has written, which is at least `durable` and may be more:
//!   a frame written but not yet fsynced is read too, and a power cut could
//!   still take it back. If a flush moves on between the two reads (the WAL
//!   no longer starts at `flushed_lsn + 1`), or a file the manifest lists
//!   is gone (a compaction replaced it), it reads both again.
//!
//! [`Snapshot::schema`] is the union of every file's and page's columns,
//! widened as a flush widens them; a reader casts each segment's values
//! to it with [`cast`] and fills missing columns with NULL, so a record
//! reads the same before and after its flush.
//!
//! [`TableWriter::snapshot`]: super::TableWriter::snapshot

use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use super::flush::{add_field, add_page_fields, predefined_specs};
use super::manifest::{FileEntry, Manifest};
use super::page::{decode_record, HeadPage, Page};
use super::recover::read_tail;
use super::table::IngestError;
use super::wal::{Record, WAL_DIR};
use crate::storage::column::{MmapRegion, Vfs, VfsFile};
use crate::storage::parquet::footer::PhysicalType;
use crate::storage::parquet::writer::{ColumnSpec, ColumnType, DecimalStorage};
use crate::storage::parquet::ParquetFile;
use crate::storage::stream::detect::{DetectedParser, Format};
use crate::storage::stream::{Source, SourceKind};
use crate::storage::PosixVfs;
use crate::vm::batch::Value;

/// How often [`Snapshot::open`] re-reads after a flush moved on.
const ATTEMPTS: u32 = 20;

/// A fixed view of a table; see the [module docs](self).
pub struct Snapshot {
    manifest: Manifest,
    /// One per `manifest.files`, same order.
    files: Vec<MmapRegion>,
    pages: Vec<Page>,
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("flushed_lsn", &self.manifest.flushed_lsn)
            .field("files", &self.manifest.files.len())
            .field("pages", &self.pages.len())
            .field("last_lsn", &self.last_lsn())
            .finish()
    }
}

impl Snapshot {
    /// Reads the table at `table` from outside the writer's process. Takes
    /// no lock and writes nothing.
    pub fn open(table: &Path) -> Result<Snapshot, IngestError> {
        for _ in 0..ATTEMPTS {
            let manifest = Manifest::read(table)?.ok_or_else(|| {
                IngestError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{}: no manifest.json, not a table", table.display()),
                ))
            })?;
            let Some(records) = read_tail(&table.join(WAL_DIR), manifest.flushed_lsn)? else {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            };
            let pages = pages_from_records(table, manifest.format, records)?;
            match Snapshot::new(table, manifest, pages) {
                // A compaction replaced the file and its grace period is
                // over: a newer manifest no longer lists it.
                Err(IngestError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                other => return other,
            }
        }
        Err(IngestError::Corrupt(format!(
            "the WAL or a listed file does not match the manifest after {ATTEMPTS} reads"
        )))
    }

    /// A snapshot of `manifest`'s files and `pages`, which hold the records
    /// above its `flushed_lsn`.
    pub(crate) fn new(
        table: &Path,
        manifest: Manifest,
        pages: Vec<Page>,
    ) -> Result<Snapshot, IngestError> {
        let files = manifest
            .files
            .iter()
            .map(|f| PosixVfs.open(&table.join(&f.path))?.mmap())
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Snapshot {
            manifest,
            files,
            pages,
        })
    }

    /// The manifest this snapshot was taken at.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Each flushed file with its bytes, oldest first.
    pub fn files(&self) -> impl Iterator<Item = (&FileEntry, &[u8])> {
        self.manifest
            .files
            .iter()
            .zip(self.files.iter().map(|m| &**m))
    }

    /// The table's line format.
    pub(crate) fn format(&self) -> Format {
        self.manifest.format
    }

    /// The records above `flushed_lsn`, oldest first.
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// The highest LSN in the snapshot.
    pub fn last_lsn(&self) -> u64 {
        self.pages
            .last()
            .map_or(self.manifest.flushed_lsn, |p| p.lsn_hi)
    }

    /// Records in the snapshot.
    pub fn rows(&self) -> u64 {
        let paged: usize = self.pages.iter().map(|p| p.segment.len()).sum();
        self.manifest
            .files
            .iter()
            .map(|f| f.rows)
            .sum::<u64>()
            .saturating_add(u64::try_from(paged).unwrap_or(u64::MAX))
    }

    /// The table's columns: the predefined ones, then every field of every
    /// file and page in LSN order, a field's type widened over all of them.
    pub fn schema(&self) -> Result<Vec<ColumnSpec>, IngestError> {
        let mut specs = predefined_specs();
        for (entry, bytes) in self.files() {
            let file = ParquetFile::open(bytes)
                .map_err(|e| IngestError::Corrupt(format!("{}: {e}", entry.path)))?;
            for element in file.metadata().schema.iter().skip(1) {
                if let Some(t) = element.physical_type.and_then(column_type) {
                    add_field(&mut specs, &element.name, t);
                }
            }
        }
        for page in &self.pages {
            add_page_fields(&mut specs, page);
        }
        Ok(specs)
    }
}

/// Pages holding `records` (WAL payloads), sealed at the page size as the
/// writer seals them.
pub(crate) fn pages_from_records(
    table: &Path,
    format: Format,
    records: Vec<Record>,
) -> Result<Vec<Page>, IngestError> {
    let parser = DetectedParser::for_format(format);
    let source = Source::new(SourceKind::Stdin, &table.to_string_lossy());
    let seal = |head: &HeadPage| {
        head.snapshot(&source, &parser)
            .ok_or_else(|| IngestError::Corrupt("the parser rejected a page".to_string()))
    };
    let mut head = HeadPage::default();
    let mut pages = Vec::new();
    for (lsn, payload) in records {
        let (observed, line) = decode_record(&payload)
            .ok_or_else(|| IngestError::Corrupt(format!("WAL record {lsn} is not a record")))?;
        head.push(lsn, observed, line);
        if head.is_full() {
            pages.push(seal(&head)?);
            head.clear();
        }
    }
    if head.len() > 0 {
        pages.push(seal(&head)?);
    }
    Ok(pages)
}

/// The column type a flushed file's field of physical type `pt` has.
pub(crate) fn column_type(pt: PhysicalType) -> Option<ColumnType> {
    match pt {
        PhysicalType::Boolean => Some(ColumnType::Boolean),
        PhysicalType::Int32 => Some(ColumnType::Int32),
        PhysicalType::Int64 => Some(ColumnType::Int64),
        PhysicalType::Float => Some(ColumnType::Float),
        PhysicalType::Double => Some(ColumnType::Double),
        PhysicalType::ByteArray => Some(ColumnType::Utf8),
        PhysicalType::Int96 | PhysicalType::FixedLenByteArray | PhysicalType::Unknown(_) => None,
    }
}

/// The physical type a column of type `t` is stored as.
pub fn physical_type(t: ColumnType) -> PhysicalType {
    match t {
        ColumnType::Boolean => PhysicalType::Boolean,
        ColumnType::Int32 | ColumnType::UInt8 | ColumnType::UInt16 | ColumnType::UInt32 => {
            PhysicalType::Int32
        }
        ColumnType::Int64
        | ColumnType::TimestampMicros
        | ColumnType::TimestampNanos
        | ColumnType::UInt64 => PhysicalType::Int64,
        ColumnType::Float => PhysicalType::Float,
        ColumnType::Double => PhysicalType::Double,
        ColumnType::Utf8 | ColumnType::Binary => PhysicalType::ByteArray,
        ColumnType::Decimal { precision, .. } => match DecimalStorage::for_precision(precision) {
            DecimalStorage::Int32 => PhysicalType::Int32,
            DecimalStorage::Int64 => PhysicalType::Int64,
            DecimalStorage::Fixed16 => PhysicalType::FixedLenByteArray,
        },
        ColumnType::FixedLenByteArray(_) => PhysicalType::FixedLenByteArray,
    }
}

/// `v` as a value of a `t` column, by the rules a flush writes it with
/// (`flush::convert`): an integer widens to a float, anything renders as
/// a string, and a value `t` cannot hold is NULL.
pub fn cast(v: Value, t: ColumnType) -> Value {
    match (t, v) {
        (_, Value::Null) => Value::Null,
        (ColumnType::Utf8, Value::Str(s)) => Value::Str(s),
        (ColumnType::Utf8, other) => Value::Str(other.to_string().into()),
        (ColumnType::Double, Value::Float(f)) => Value::Float(f),
        (ColumnType::Double, Value::Int(i)) => Value::Float(i as f64),
        (
            ColumnType::Int64 | ColumnType::TimestampMicros | ColumnType::TimestampNanos,
            v @ Value::Int(_),
        ) => v,
        (ColumnType::Int32, Value::Int(i)) if i32::try_from(i).is_ok() => Value::Int(i),
        (ColumnType::Boolean, v @ Value::Bool(_)) => v,
        (ColumnType::UInt8, Value::Int(i)) if u8::try_from(i).is_ok() => Value::Int(i),
        (ColumnType::UInt16, Value::Int(i)) if u16::try_from(i).is_ok() => Value::Int(i),
        (ColumnType::UInt32, Value::Int(i)) if u32::try_from(i).is_ok() => Value::Int(i),
        (ColumnType::UInt64, Value::Int(i)) if i >= 0 => Value::Int(i),
        // As the column engine reads them: a decimal as a float, bytes as
        // lowercase hex.
        (ColumnType::Decimal { .. }, Value::Float(f)) => Value::Float(f),
        (ColumnType::Decimal { .. }, Value::Int(i)) => Value::Float(i as f64),
        (ColumnType::Binary | ColumnType::FixedLenByteArray(_), v @ Value::Str(_)) => v,
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests;
