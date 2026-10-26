//! A read view of a table (ADR 0027, #572): the manifest's files plus the
//! records above its `flushed_lsn`, as pages.
//!
//! Every record is in exactly one place: files hold LSNs `1..=flushed_lsn`,
//! pages the ones above. A snapshot is fixed once taken; a query that
//! wants newer records takes a new one.
//!
//! - In the writer's process, `TableWriter::snapshot` copies the
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

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::error::IngestError;
use super::fields::{add_field, add_page_fields, predefined_specs};
use super::manifest::{FileEntry, Manifest};
use super::page::{decode_record, HeadPage, Page};
use super::record::{check_row, decode_row, pad_row};
use super::recover::{read_from, read_tail, WalPos};
use super::wal::{Record, WAL_DIR};
use crate::storage::column::{MmapRegion, Vfs, VfsFile};
use crate::storage::parquet::footer::PhysicalType;
use crate::storage::parquet::schema::{ColumnSpec, ColumnType, DecimalStorage};
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
    files: Vec<Arc<MmapRegion>>,
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
            let manifest = read_manifest(table)?;
            let Some(records) = read_tail(&table.join(WAL_DIR), manifest.flushed_lsn)? else {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            };
            let pages =
                pages_from_records(table, manifest.format, manifest.schema.as_deref(), records)?;
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
            .map(|f| Ok(Arc::new(PosixVfs.open(&table.join(&f.path))?.mmap()?)))
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
            .zip(self.files.iter().map(|m| &***m))
    }

    /// The table's line format.
    #[cfg(feature = "engine-column")]
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
        let paged: usize = self.pages.iter().map(Page::len).sum();
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
        // A record table's columns are its schema, whatever is flushed.
        if let Some(schema) = &self.manifest.schema {
            return Ok(schema.clone());
        }
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
/// writer seals them: a lines table's lines in `format`, or a record
/// table's rows of `schema`.
pub(crate) fn pages_from_records(
    table: &Path,
    format: Format,
    schema: Option<&[ColumnSpec]>,
    records: Vec<Record>,
) -> Result<Vec<Page>, IngestError> {
    let mut builder = PageBuilder::new(table, format, schema);
    for record in records {
        builder.push(record)?;
    }
    builder.pages()
}

/// Records turned into pages one at a time: full pages are sealed and kept,
/// the rest stays in a head that is re-sealed (and cached) only when it has
/// grown, so a reader that adds records pays for the head, not the tail.
pub(crate) struct PageBuilder {
    parser: DetectedParser,
    source: Source,
    schema: Option<Arc<[ColumnSpec]>>,
    head: HeadPage,
    sealed: Vec<Page>,
    /// `head` as a page, until the next record.
    head_page: Option<Page>,
}

impl PageBuilder {
    pub(crate) fn new(table: &Path, format: Format, schema: Option<&[ColumnSpec]>) -> PageBuilder {
        let schema: Option<Arc<[ColumnSpec]>> = schema.map(Arc::from);
        PageBuilder {
            parser: DetectedParser::for_format(format),
            source: Source::new(SourceKind::Stdin, &table.to_string_lossy()),
            head: schema
                .clone()
                .map_or_else(HeadPage::default, HeadPage::for_records),
            schema,
            sealed: Vec::new(),
            head_page: None,
        }
    }

    fn seal(&self) -> Result<Page, IngestError> {
        self.head
            .snapshot(&self.source, &self.parser)
            .ok_or_else(|| IngestError::Corrupt("the parser rejected a page".to_string()))
    }

    /// Appends WAL record `(lsn, payload)`, which follows the last one.
    pub(crate) fn push(&mut self, (lsn, payload): Record) -> Result<(), IngestError> {
        let corrupt = || IngestError::Corrupt(format!("WAL record {lsn} is not a record"));
        let (observed, body) = decode_record(&payload).ok_or_else(corrupt)?;
        match &self.schema {
            Some(schema) => {
                let row = decode_row(body)
                    .map(|row| pad_row(schema, row))
                    .filter(|row| check_row(schema, row).is_none())
                    .ok_or_else(corrupt)?;
                self.head.push_row(lsn, observed, row);
            }
            None => self.head.push(lsn, observed, body),
        }
        self.head_page = None;
        if self.head.is_full() {
            let page = self.seal()?;
            self.sealed.push(page);
            self.head.clear();
        }
        Ok(())
    }

    /// Every page so far, the head last.
    pub(crate) fn pages(&mut self) -> Result<Vec<Page>, IngestError> {
        let mut pages = self.sealed.clone();
        if self.head.len() > 0 {
            if self.head_page.is_none() {
                self.head_page = Some(self.seal()?);
            }
            pages.extend(self.head_page.clone());
        }
        Ok(pages)
    }
}

/// What a [`TableReader`] has done, for tests and benches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderStats {
    /// Refreshes that returned a snapshot.
    pub refreshes: u64,
    /// Refreshes that read the whole WAL tail again: the first, and those
    /// after a flush moved on or the saved position no longer held.
    pub rebuilds: u64,
    /// WAL records parsed into pages, over all refreshes.
    pub records_parsed: u64,
}

/// A long-lived reader of a table directory from another process (#593).
/// Like [`Snapshot::open`] it takes no lock and writes nothing, but it
/// keeps what it parsed: a refresh reads only the WAL frames written since
/// the last one, and maps only files that are new.
///
/// The held pages stay valid while the manifest's `flushed_lsn`, schema
/// and format are as they were; when one moves, a refresh reads the whole
/// tail again, as [`Snapshot::open`] does. A flush every hour is the
/// worst case, so a reader refreshed within the hour parses each record
/// once.
pub struct TableReader {
    dir: PathBuf,
    /// The manifest the held pages and `pos` were read at.
    manifest: Manifest,
    builder: PageBuilder,
    pos: Option<WalPos>,
    /// The LSN after the last record parsed.
    next_lsn: u64,
    /// Mapped files by path, kept while the manifest lists them.
    mapped: Vec<(String, Arc<MmapRegion>)>,
    snapshot: Snapshot,
    stats: ReaderStats,
}

impl fmt::Debug for TableReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TableReader")
            .field("dir", &self.dir)
            .field("snapshot", &self.snapshot)
            .field("stats", &self.stats)
            .finish()
    }
}

impl TableReader {
    /// Opens the table at `dir` and reads it once.
    pub fn open(dir: &Path) -> Result<TableReader, IngestError> {
        let mut stats = ReaderStats::default();
        let (manifest, builder, pos, next_lsn) = Self::rebuild(dir, &mut stats)?;
        let mut reader = TableReader {
            dir: dir.to_path_buf(),
            snapshot: Snapshot {
                manifest: manifest.clone(),
                files: Vec::new(),
                pages: Vec::new(),
            },
            manifest,
            builder,
            pos,
            next_lsn,
            mapped: Vec::new(),
            stats,
        };
        reader.assemble()?;
        Ok(reader)
    }

    /// The snapshot the last refresh (or the open) took.
    #[must_use]
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// What the reader has done so far.
    #[must_use]
    pub fn stats(&self) -> ReaderStats {
        self.stats
    }

    /// Reads whatever the writer has added since the last call and returns
    /// the new snapshot.
    pub fn refresh(&mut self) -> Result<&Snapshot, IngestError> {
        for _ in 0..ATTEMPTS {
            let manifest = read_manifest(&self.dir)?;
            if !self.holds(&manifest) || !self.advance()? {
                let (manifest, builder, pos, next_lsn) = Self::rebuild(&self.dir, &mut self.stats)?;
                self.manifest = manifest;
                self.builder = builder;
                self.pos = pos;
                self.next_lsn = next_lsn;
            } else {
                self.manifest = manifest;
            }
            match self.assemble() {
                Ok(()) => return Ok(&self.snapshot),
                // A compaction replaced a file and its grace period is
                // over: a newer manifest no longer lists it.
                Err(IngestError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
        Err(IngestError::Corrupt(format!(
            "the WAL or a listed file does not match the manifest after {ATTEMPTS} reads"
        )))
    }

    /// Whether the held pages are still the records above `manifest`'s
    /// `flushed_lsn`, read the way it says to.
    fn holds(&self, manifest: &Manifest) -> bool {
        manifest.flushed_lsn == self.manifest.flushed_lsn
            && manifest.schema == self.manifest.schema
            && manifest.format == self.manifest.format
    }

    /// Reads the frames after the saved position into the held pages;
    /// false if the position no longer holds.
    fn advance(&mut self) -> Result<bool, IngestError> {
        let Some((records, pos)) = read_from(&self.dir.join(WAL_DIR), self.next_lsn, self.pos)?
        else {
            return Ok(false);
        };
        for record in records {
            self.next_lsn = record.0.saturating_add(1);
            self.builder.push(record)?;
            self.stats.records_parsed = self.stats.records_parsed.saturating_add(1);
        }
        self.pos = pos.or(self.pos);
        Ok(true)
    }

    /// The manifest, a builder with every record above its `flushed_lsn`,
    /// the position the read stopped at and the next LSN.
    fn rebuild(
        dir: &Path,
        stats: &mut ReaderStats,
    ) -> Result<(Manifest, PageBuilder, Option<WalPos>, u64), IngestError> {
        for _ in 0..ATTEMPTS {
            let manifest = read_manifest(dir)?;
            let first = manifest.flushed_lsn.saturating_add(1);
            let Some((records, pos)) = read_from(&dir.join(WAL_DIR), first, None)? else {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            };
            let mut builder = PageBuilder::new(dir, manifest.format, manifest.schema.as_deref());
            let mut next = first;
            for record in records {
                next = record.0.saturating_add(1);
                builder.push(record)?;
                stats.records_parsed = stats.records_parsed.saturating_add(1);
            }
            stats.rebuilds = stats.rebuilds.saturating_add(1);
            return Ok((manifest, builder, pos, next));
        }
        Err(IngestError::Corrupt(format!(
            "the WAL or a listed file does not match the manifest after {ATTEMPTS} reads"
        )))
    }

    /// Takes the snapshot of the held manifest and pages, mapping only
    /// the files not mapped yet.
    fn assemble(&mut self) -> Result<(), IngestError> {
        let mut mapped = Vec::with_capacity(self.manifest.files.len());
        for entry in &self.manifest.files {
            let region = match self.mapped.iter().find(|(path, _)| *path == entry.path) {
                Some((_, region)) => Arc::clone(region),
                None => Arc::new(PosixVfs.open(&self.dir.join(&entry.path))?.mmap()?),
            };
            mapped.push((entry.path.clone(), region));
        }
        self.mapped = mapped;
        self.snapshot = Snapshot {
            manifest: self.manifest.clone(),
            files: self.mapped.iter().map(|(_, r)| Arc::clone(r)).collect(),
            pages: self.builder.pages()?,
        };
        self.stats.refreshes = self.stats.refreshes.saturating_add(1);
        Ok(())
    }
}

fn read_manifest(table: &Path) -> Result<Manifest, IngestError> {
    Manifest::read(table)?.ok_or_else(|| {
        IngestError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{}: no manifest.json, not a table", table.display()),
        ))
    })
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
        // A map's leaves; the group has no physical type of its own.
        ColumnType::MapUtf8 => PhysicalType::ByteArray,
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
