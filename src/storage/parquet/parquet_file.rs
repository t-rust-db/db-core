//! Top-level Parquet file reader: parses the footer once, then exposes a
//! lazy iterator over row groups and their columns.

use crate::storage::parquet::compression::{self, CompressionError};
use crate::storage::parquet::footer::{self, FileMetaData, FooterError, Repetition, RowGroup};
use crate::storage::parquet::nested::{self, LeafData, LeafEntries, NestedError, NestedValue};
use crate::storage::parquet::page::{self, Encoding, PageError, PageType};
use crate::storage::parquet::reader::{self, ReadError};
use crate::storage::parquet::schema_tree::{self, SchemaNode};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Why a Parquet file, or one of its column chunks, could not be read.
#[derive(Debug)]
pub enum FileError {
    /// The footer (file metadata) is missing or malformed.
    Footer(FooterError),
    /// A footer/page-header integer is outside the range the file format
    /// allows (negative count, negative or oversize offset/size) -- corrupt
    /// or hostile input, reported instead of wrapped into a huge allocation.
    InvalidMetadata {
        /// Which field.
        field: &'static str,
        /// The value as read.
        value: i64,
    },
    /// A page header is malformed or of an unsupported type.
    Page(PageError),
    /// A page body does not decode.
    Read(ReadError),
    /// A page body does not decompress.
    Compression(CompressionError),
    /// A column (or row group, or field) index past the file's last one.
    ColumnIndexOutOfRange(usize),
    /// A positional read (`read_*_column_at`, ADR-0026) named a row
    /// position past the column's decoded length.
    RowPositionOutOfRange(u32),
    /// A column chunk's byte range, or a page within it, runs past the end
    /// of the file (or chunk).
    ChunkOutOfBounds,
    /// A column chunk without `meta_data`, or a leaf without a physical type.
    MissingColumnMetadata,
    /// A second `DICTIONARY_PAGE` in one column chunk.
    UnexpectedDictionaryPage,
    /// A dictionary-encoded data page with no dictionary page before it.
    MissingDictionaryPage,
    /// A `FIXED_LEN_BYTE_ARRAY` column whose schema has no positive `type_length`.
    MissingTypeLength,
    /// A `DECIMAL` column whose schema has no `scale`.
    MissingDecimalScale,
    /// A `UINT_64` value above `i64::MAX`, which the engine's integers
    /// cannot hold.
    UnsignedOutOfRange(u64),
    /// A timestamp whose value in microseconds since the Unix epoch does not
    /// fit an `i64`.
    TimestampOutOfRange,
    /// A timestamp column stored as neither `INT64` nor `INT96`.
    UnsupportedTimestampPhysicalType(footer::PhysicalType),
    /// A `DECIMAL` column stored as neither `INT32`, `INT64` nor
    /// `FIXED_LEN_BYTE_ARRAY`.
    UnsupportedDecimalPhysicalType(footer::PhysicalType),
    /// A dictionary-encoded nested leaf column the reader cannot decode.
    UnsupportedNestedDictionary,
    /// A nested leaf column encoded other than `PLAIN` or dictionary.
    UnsupportedNestedEncoding(page::Encoding),
    /// A field read as a `MAP<UTF8, UTF8>` that is not one.
    NotAStringMap(String),
    /// A field read as a `LIST<UTF8>` that is not one.
    NotAStringList(String),
    /// Nested values could not be rebuilt from a leaf's levels.
    Nested(NestedError),
    /// A page declares more than its column chunk has left (#671): more
    /// values than the row group's rows, or more uncompressed bytes than
    /// the chunk's `total_uncompressed_size`. Refused before the page is
    /// decompressed or decoded, so a small file cannot expand past what
    /// its metadata states.
    PageExceedsChunk {
        /// Which page-header field.
        field: &'static str,
        /// The page's value.
        declared: u64,
        /// What the chunk had left.
        left: u64,
    },
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::Footer(e) => write!(f, "{e}"),
            FileError::InvalidMetadata { field, value } => {
                write!(f, "invalid {field} in file metadata: {value}")
            }
            FileError::Page(e) => write!(f, "{e}"),
            FileError::Read(e) => write!(f, "{e}"),
            FileError::Compression(e) => write!(f, "{e}"),
            FileError::ColumnIndexOutOfRange(i) => write!(f, "column index {i} out of range"),
            FileError::RowPositionOutOfRange(p) => write!(f, "row position {p} out of range"),
            FileError::ChunkOutOfBounds => write!(f, "column chunk byte range is outside the file"),
            FileError::MissingColumnMetadata => write!(f, "column chunk has no meta_data"),
            FileError::UnexpectedDictionaryPage => {
                write!(f, "column chunk has more than one DICTIONARY_PAGE")
            }
            FileError::MissingDictionaryPage => write!(
                f,
                "dictionary-encoded data page with no preceding DICTIONARY_PAGE"
            ),
            FileError::MissingTypeLength => write!(
                f,
                "FIXED_LEN_BYTE_ARRAY column has no positive type_length in its schema"
            ),
            FileError::MissingDecimalScale => {
                write!(f, "DECIMAL column has no scale in its schema")
            }
            FileError::UnsignedOutOfRange(v) => write!(
                f,
                "unsigned value {v} is above the largest supported integer ({})",
                i64::MAX
            ),
            FileError::TimestampOutOfRange => write!(
                f,
                "timestamp is outside the range of microseconds since the epoch"
            ),
            FileError::UnsupportedTimestampPhysicalType(t) => {
                write!(f, "unsupported physical type for a timestamp column: {t:?}")
            }
            FileError::UnsupportedDecimalPhysicalType(t) => {
                write!(f, "unsupported physical type for a DECIMAL column: {t:?}")
            }
            FileError::NotAStringMap(name) => {
                write!(f, "field {name} is not a MAP<UTF8, UTF8>")
            }
            FileError::NotAStringList(name) => {
                write!(f, "field {name} is not a LIST<UTF8>")
            }
            FileError::UnsupportedNestedDictionary => write!(
                f,
                "dictionary-encoded nested/repeated leaf columns are not supported yet"
            ),
            FileError::UnsupportedNestedEncoding(e) => write!(
                f,
                "unsupported encoding for a nested/repeated leaf column: {e:?}"
            ),
            FileError::Nested(e) => write!(f, "{e}"),
            FileError::PageExceedsChunk {
                field,
                declared,
                left,
            } => write!(
                f,
                "page {field} {declared} is more than its column chunk has left ({left})"
            ),
        }
    }
}

impl From<NestedError> for FileError {
    fn from(e: NestedError) -> Self {
        FileError::Nested(e)
    }
}

impl std::error::Error for FileError {}

impl From<FooterError> for FileError {
    fn from(e: FooterError) -> Self {
        FileError::Footer(e)
    }
}
impl From<PageError> for FileError {
    fn from(e: PageError) -> Self {
        FileError::Page(e)
    }
}
impl From<ReadError> for FileError {
    fn from(e: ReadError) -> Self {
        FileError::Read(e)
    }
}
impl From<CompressionError> for FileError {
    fn from(e: CompressionError) -> Self {
        FileError::Compression(e)
    }
}

/// Result of reading a Parquet file.
pub type Result<T> = std::result::Result<T, FileError>;

/// A `PLAIN` string chunk as `Column::Str`'s `(offsets, data, present)` (#731).
pub type PlainStrings = (Vec<u32>, String, Vec<bool>);

/// A dictionary-encoded column's dictionary plus one raw index per row
/// (`None` for nulls) -- see `read_*_column_dictionary_indices`.
pub type DictionaryIndices<T> = (Vec<T>, Vec<Option<u32>>);

/// The per-type flat readers (`RowGroupReader::read_int64_column` and
/// friends) only work correctly on a flat schema:
/// `RowGroupReader::max_definition_level` maps a leaf column index straight
/// to `schema[column_index + 1]`, which only lines up with the right leaf
/// (or a leaf at all, rather than a struct/list/map group node) when the
/// schema root's children *are* the leaves in file order. A nested or
/// repeated schema would make those methods silently read the wrong
/// definition level and return wrong values with no error (see #61) --
/// nested/repeated fields must go through [`ParquetFile::read_nested_column`]
/// instead, which this flag (see [`ParquetFile::is_flat`]) lets callers detect.
/// A page-header size as `usize`: negative (corrupt/hostile header) is an
/// error, never a wrapped-around allocation.
fn page_size(field: &'static str, value: i32) -> Result<usize> {
    usize::try_from(value).map_err(|_| FileError::InvalidMetadata {
        field,
        value: i64::from(value),
    })
}

/// Decodes the page header at `pos` in `chunk_bytes` and returns it with
/// the page's (still compressed) body and the offset just past that body,
/// where the next page starts. A header or body running past the chunk is
/// an error, never a panic: every offset and size here comes from the file.
fn next_page(chunk_bytes: &[u8], pos: usize) -> Result<(page::PageHeader, &[u8], usize)> {
    let rest = chunk_bytes.get(pos..).ok_or(FileError::ChunkOutOfBounds)?;
    let (header, consumed) = page::decode_page_header(rest)?;
    let page_start = pos
        .checked_add(consumed)
        .ok_or(FileError::ChunkOutOfBounds)?;
    let page_end = page_start
        .checked_add(page_size(
            "compressed_page_size",
            header.compressed_page_size,
        )?)
        .ok_or(FileError::ChunkOutOfBounds)?;
    let compressed = chunk_bytes
        .get(page_start..page_end)
        .ok_or(FileError::ChunkOutOfBounds)?;
    Ok((header, compressed, page_end))
}

/// A column chunk's pages in order (#671), each checked before it is
/// decompressed against what the chunk has left: a data page's
/// `num_values` against the values budget (the row group's rows for a
/// flat column, the chunk's `num_values` for a nested leaf), and every
/// page's `uncompressed_page_size` against the chunk's
/// `total_uncompressed_size`. Decoding then never expands past what the
/// metadata states: an RLE run or a zero-width page fills at most the
/// values left, a ZSTD page at most the bytes left.
struct Pages<'a> {
    chunk: &'a [u8],
    pos: usize,
    codec: footer::Codec,
    values_left: u64,
    bytes_left: u64,
}

impl<'a> Pages<'a> {
    /// The next page's header and (still compressed) body.
    fn next(&mut self) -> Result<Option<(page::PageHeader, &'a [u8])>> {
        if self.pos >= self.chunk.len() {
            return Ok(None);
        }
        let (header, compressed, end) = next_page(self.chunk, self.pos)?;
        let bytes = page_size("uncompressed_page_size", header.uncompressed_page_size)?;
        self.bytes_left = spend(self.bytes_left, bytes as u64, "uncompressed_page_size")?;
        if let PageType::Data(data_page_header) = &header.page_type {
            let values = u64::try_from(data_page_header.num_values).map_err(|_| {
                FileError::InvalidMetadata {
                    field: "data_page.num_values",
                    value: i64::from(data_page_header.num_values),
                }
            })?;
            self.values_left = spend(self.values_left, values, "num_values")?;
        }
        self.pos = end;
        Ok(Some((header, compressed)))
    }

    /// A page body, decompressed to its `uncompressed_page_size`.
    fn body(
        &self,
        compressed: &'a [u8],
        uncompressed_page_size: i32,
    ) -> Result<std::borrow::Cow<'a, [u8]>> {
        Ok(compression::decompress(
            self.codec,
            compressed,
            page_size("uncompressed_page_size", uncompressed_page_size)?,
        )?)
    }
}

/// `left` less a page's `declared` amount, or the page exceeds its chunk.
fn spend(left: u64, declared: u64, field: &'static str) -> Result<u64> {
    left.checked_sub(declared)
        .ok_or(FileError::PageExceedsChunk {
            field,
            declared,
            left,
        })
}

/// A footer count as `u64`: negative is invalid metadata.
fn count(field: &'static str, value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| FileError::InvalidMetadata { field, value })
}

/// ADR-0026 v1 positional read: gathers `positions` out of an
/// already-decoded whole column, in the order given. `positions` are row
/// indices into `full`'s own numbering (0-based within the row group).
fn gather<T: Clone>(full: &[Option<T>], positions: &[u32]) -> Result<Vec<Option<T>>> {
    positions
        .iter()
        .map(|&p| {
            full.get(p as usize)
                .cloned()
                .ok_or(FileError::RowPositionOutOfRange(p))
        })
        .collect()
}

fn is_flat_schema(schema: &[footer::SchemaElement]) -> bool {
    let Some(root) = schema.first() else {
        return true;
    };
    let expected_leaves = usize::try_from(root.num_children.unwrap_or(0)).unwrap_or(0);
    let leaves = schema.get(1..).unwrap_or_default();
    leaves.len() == expected_leaves
        && leaves
            .iter()
            .all(|s| s.physical_type.is_some() && s.repetition != Some(Repetition::Repeated))
}

/// An open Parquet file: the parsed footer plus a borrowed view of the raw
/// file bytes. Get the bytes via `crate::storage::column::vfs::Vfs::open(..)?.mmap()?`
/// (which derefs to `&[u8]`), or any other `&[u8]` source.
pub struct ParquetFile<'a> {
    metadata: FileMetaData,
    data: &'a [u8],
    schema_tree: SchemaNode,
    is_flat: bool,
    /// Leaf (column chunk) `i`'s index in `metadata.schema`. For a flat
    /// schema that is `i + 1`; a group (struct, list, map) shifts every
    /// leaf after it.
    leaf_elements: Vec<usize>,
    /// Leaf `i`'s max definition level, from the schema tree.
    leaf_max_def: Vec<u32>,
}

/// A map's `(key, value)` entries, in order; a value may be NULL. Keys
/// and values are shared: a dictionary string is one allocation however
/// many maps hold it (#671).
pub type MapEntries = Vec<(Arc<str>, Option<Arc<str>>)>;

/// A list's elements, in order; an element may be NULL. Elements are
/// shared as a map's strings are (#801).
pub type ListItems = Vec<Option<Arc<str>>>;

/// One top-level field of a file: a leaf column, or a group (struct, list,
/// map) and the leaves (column chunks) it spans.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The field's name.
    pub name: String,
    /// Its schema element (for a group: the group node, e.g. `MAP`).
    pub element: footer::SchemaElement,
    /// The column chunks it is stored in, as leaf indices.
    pub leaves: std::ops::Range<usize>,
    /// A group rather than a single leaf.
    pub nested: bool,
}

impl<'a> ParquetFile<'a> {
    /// Parse the footer of a Parquet file. No column data is read yet.
    /// Nested/repeated (struct/list/map) schemas are supported via
    /// [`Self::read_nested_column`]; the per-type flat readers
    /// (`RowGroupReader::read_int64_column` and friends) only produce
    /// correct results for a flat schema (see [`Self::is_flat`]).
    pub fn open(data: &'a [u8]) -> Result<Self> {
        let metadata = footer::parse_footer(data)?;
        let is_flat = is_flat_schema(&metadata.schema);
        let schema_tree = schema_tree::build_schema_tree(&metadata.schema);
        let leaf_elements = metadata
            .schema
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, e)| e.physical_type.is_some())
            .map(|(i, _)| i)
            .collect();
        let leaf_max_def = schema_tree
            .leaves()
            .iter()
            .map(|l| l.max_def_level)
            .collect();
        Ok(ParquetFile {
            metadata,
            data,
            schema_tree,
            is_flat,
            leaf_elements,
            leaf_max_def,
        })
    }

    /// The schema element of leaf (column chunk) `column_index`.
    pub fn leaf_element(&self, column_index: usize) -> Option<&footer::SchemaElement> {
        self.metadata
            .schema
            .get(*self.leaf_elements.get(column_index)?)
    }

    /// The top-level fields, in schema order, each with the leaves it is
    /// stored in. Column chunk indices count leaves, not schema elements,
    /// so a field after a group does not start at its element's position.
    pub fn fields(&self) -> Vec<Field> {
        let mut next = 0usize;
        self.schema_tree
            .children
            .iter()
            .map(|child| {
                let count = child.leaves().len();
                let start = next;
                next = next.saturating_add(count);
                Field {
                    name: child.name.clone(),
                    element: child.element.clone(),
                    leaves: start..next,
                    nested: !child.is_leaf(),
                }
            })
            .collect()
    }

    /// Whether every field in the schema is a required/optional leaf
    /// column at the top level (no structs, lists, or maps). Only flat
    /// schemas are safe to read with the per-type `read_*_column` methods.
    pub fn is_flat(&self) -> bool {
        self.is_flat
    }

    /// Reconstruct one top-level field (by name) of one row group into
    /// nested values — one [`NestedValue`] per row. Works for both flat
    /// leaf columns and nested struct/list/map fields.
    pub fn read_nested_column(
        &self,
        row_group_index: usize,
        field_name: &str,
    ) -> Result<Vec<NestedValue>> {
        let row_group = self
            .row_group(row_group_index)
            .ok_or(FileError::ColumnIndexOutOfRange(row_group_index))?;
        if !self
            .schema_tree
            .children
            .iter()
            .any(|c| c.name == field_name)
        {
            return Err(FileError::ColumnIndexOutOfRange(row_group_index));
        }

        let mut leaf_data: LeafData = HashMap::new();
        let leaves = self.schema_tree.leaves();
        let paths =
            schema_tree::leaf_paths(&self.schema_tree).ok_or(FileError::InvalidMetadata {
                field: "schema (leaf paths too long)",
                value: i64::try_from(leaves.len()).unwrap_or(i64::MAX),
            })?;
        for (column_index, (leaf, path)) in leaves.iter().zip(paths.iter()).enumerate() {
            let entries = row_group.read_leaf_entries(column_index, leaf)?;
            leaf_data.insert(path.clone(), entries);
        }

        let num_rows =
            usize::try_from(row_group.num_rows()).map_err(|_| FileError::InvalidMetadata {
                field: "row_group.num_rows",
                value: row_group.num_rows(),
            })?;
        let mut columns = nested::reconstruct_row_group(&self.schema_tree, num_rows, &leaf_data)?;
        let idx = columns
            .iter()
            .position(|(name, _)| name == field_name)
            .ok_or(FileError::ColumnIndexOutOfRange(row_group_index))?;
        Ok(columns.remove(idx).1)
    }

    /// The parsed footer.
    pub fn metadata(&self) -> &FileMetaData {
        &self.metadata
    }

    /// Total row count, as the footer states it.
    pub fn num_rows(&self) -> i64 {
        self.metadata.num_rows
    }

    /// Number of row groups.
    pub fn num_row_groups(&self) -> usize {
        self.metadata.row_groups.len()
    }

    /// Get a reader for one row group by index. Lazy: this only borrows
    /// metadata already parsed into memory — no page data is read.
    pub fn row_group(&self, index: usize) -> Option<RowGroupReader<'a, '_>> {
        let row_group = self.metadata.row_groups.get(index)?;
        Some(RowGroupReader {
            file: self,
            row_group,
        })
    }

    /// Lazily iterate over all row groups in file order.
    pub fn row_groups(&self) -> impl Iterator<Item = RowGroupReader<'a, '_>> {
        // `row_group(i)` can only fail for an out-of-range index, which the
        // range excludes; `flat_map` over the `Result` keeps that without an
        // `expect`.
        (0..self.num_row_groups()).flat_map(move |i| self.row_group(i))
    }

    /// Max definition level for a leaf column: one per optional or
    /// repeated node on its path (for a top-level leaf, `OPTIONAL` -> 1,
    /// `REQUIRED` -> 0).
    fn max_definition_level(&self, column_index: usize) -> u32 {
        self.leaf_max_def.get(column_index).copied().unwrap_or(0)
    }

    /// Fixed byte width for a `FIXED_LEN_BYTE_ARRAY` leaf column. Zero is
    /// refused like a missing width: every value would read no bytes, so a
    /// corrupt page could claim `i32::MAX` values from an empty body.
    fn type_length(&self, column_index: usize) -> Result<usize> {
        self.leaf_element(column_index)
            .and_then(|s| s.type_length)
            .and_then(|len| usize::try_from(len).ok())
            .filter(|&len| len > 0)
            .ok_or(FileError::MissingTypeLength)
    }

    fn converted_type(&self, column_index: usize) -> Option<footer::ConvertedType> {
        self.leaf_element(column_index)
            .and_then(|s| s.converted_type)
    }

    fn decimal_scale(&self, column_index: usize) -> Result<i32> {
        self.leaf_element(column_index)
            .and_then(|s| s.scale)
            .ok_or(FileError::MissingDecimalScale)
    }
}

/// A single row group's columns. Borrows both the parsed metadata (`'m`)
/// and the underlying file bytes (`'a`).
pub struct RowGroupReader<'a, 'm> {
    file: &'m ParquetFile<'a>,
    row_group: &'m RowGroup,
}

impl<'a, 'm> RowGroupReader<'a, 'm> {
    /// This row group's row count, as the footer states it.
    pub fn num_rows(&self) -> i64 {
        self.row_group.num_rows
    }

    /// The schema element of leaf `column_index`: its annotations
    /// (converted type, scale, type length).
    pub fn schema_element(&self, column_index: usize) -> Option<&footer::SchemaElement> {
        self.file.leaf_element(column_index)
    }

    /// The top-level nested field (struct, list, map) whose first leaf is
    /// `column_index`, if any.
    pub fn nested_field_at(&self, column_index: usize) -> Option<Field> {
        self.file
            .fields()
            .into_iter()
            .find(|f| f.nested && f.leaves.start == column_index)
    }

    /// Number of column chunks (leaf columns) in this row group.
    pub fn num_columns(&self) -> usize {
        self.row_group.columns.len()
    }

    fn column_meta(&self, column_index: usize) -> Result<&footer::ColumnMetaData> {
        let chunk = self
            .row_group
            .columns
            .get(column_index)
            .ok_or(FileError::ColumnIndexOutOfRange(column_index))?;
        chunk
            .meta_data
            .as_ref()
            .ok_or(FileError::MissingColumnMetadata)
    }

    /// This row group's footer statistics for one leaf column, if the
    /// writer included them. `None` means "no statistics" -- callers must
    /// treat that as "cannot prune", never as an error: writers are free to
    /// omit them.
    pub fn column_statistics(&self, column_index: usize) -> Result<Option<&footer::Statistics>> {
        Ok(self.column_meta(column_index)?.statistics.as_ref())
    }

    /// Slice of the file covering one column chunk's page data: from its
    /// first page's offset (the dictionary page's, if present, else the
    /// first data page's) through `total_compressed_size` bytes.
    /// `total_compressed_size` spans every page in the chunk including the
    /// dictionary page, so it must be added to that same starting offset —
    /// adding it to `data_page_offset` instead over-extends the slice into
    /// the next column's bytes whenever a dictionary page precedes.
    fn column_chunk_bytes(&self, column_index: usize) -> Result<&'a [u8]> {
        let meta = self.column_meta(column_index)?;
        let start_i64 = meta.dictionary_page_offset.unwrap_or(meta.data_page_offset);
        let start = usize::try_from(start_i64).map_err(|_| FileError::InvalidMetadata {
            field: "column_chunk.page_offset",
            value: start_i64,
        })?;
        let size = usize::try_from(meta.total_compressed_size).map_err(|_| {
            FileError::InvalidMetadata {
                field: "column_chunk.total_compressed_size",
                value: meta.total_compressed_size,
            }
        })?;
        let end = start.checked_add(size).ok_or(FileError::ChunkOutOfBounds)?;
        self.file
            .data
            .get(start..end)
            .ok_or(FileError::ChunkOutOfBounds)
    }

    /// The pages of column chunk `column_index`, budgeted by `values`.
    fn pages_with(&self, column_index: usize, values: u64) -> Result<Pages<'a>> {
        let meta = self.column_meta(column_index)?;
        Ok(Pages {
            chunk: self.column_chunk_bytes(column_index)?,
            pos: 0,
            codec: meta.codec,
            values_left: values,
            bytes_left: count(
                "column_chunk.total_uncompressed_size",
                meta.total_uncompressed_size,
            )?,
        })
    }

    /// The pages of flat column chunk `column_index`: one value per row.
    fn pages(&self, column_index: usize) -> Result<Pages<'a>> {
        self.pages_with(column_index, count("row_group.num_rows", self.num_rows())?)
    }

    /// Decode every page in a column chunk into typed values, concatenating
    /// them in page order. Pages before a dictionary page appears (or when
    /// there is none) go through `decode_plain`; once a `DICTIONARY_PAGE`
    /// has been seen, subsequent `DATA_PAGE`s go through `decode_data`
    /// against it. A `DICTIONARY_PAGE` appearing more than once in a chunk
    /// is rejected rather than silently overwriting the first dictionary
    /// (real encoders emit at most one per chunk).
    fn read_column<T>(
        &self,
        column_index: usize,
        decode_plain: impl Fn(&[u8], &page::DataPageHeader, u32) -> reader::Result<Vec<Option<T>>>,
        decode_dictionary: impl Fn(&[u8], i32) -> reader::Result<Vec<T>>,
        decode_data: impl Fn(&[u8], &page::DataPageHeader, u32, &[T]) -> reader::Result<Vec<Option<T>>>,
    ) -> Result<Vec<Option<T>>> {
        let mut pages = self.pages(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        let mut dictionary: Option<Vec<T>> = None;
        let mut out: Vec<Option<T>> = Vec::new();

        while let Some((header, compressed)) = pages.next()? {
            match header.page_type {
                PageType::Data(data_page_header) => {
                    let page_body = pages.body(compressed, header.uncompressed_page_size)?;
                    let is_dictionary_encoded = matches!(
                        data_page_header.encoding,
                        page::Encoding::PlainDictionary | page::Encoding::RleDictionary
                    );
                    let values = match (is_dictionary_encoded, &dictionary) {
                        (true, Some(dict)) => {
                            decode_data(&page_body, &data_page_header, max_def_level, dict)?
                        }
                        (true, None) => return Err(FileError::MissingDictionaryPage),
                        (false, _) => decode_plain(&page_body, &data_page_header, max_def_level)?,
                    };
                    out.extend(values);
                }
                PageType::Dictionary(dictionary_page_header) => {
                    if dictionary.is_some() {
                        return Err(FileError::UnexpectedDictionaryPage);
                    }
                    let dictionary_body = pages.body(compressed, header.uncompressed_page_size)?;
                    dictionary = Some(decode_dictionary(
                        &dictionary_body,
                        dictionary_page_header.num_values,
                    )?);
                }
            }
        }
        Ok(out)
    }

    /// Decodes a fixed-width primitive column chunk straight into typed
    /// storage (#495): `Ok(Some((values, present)))` holds one value per
    /// row and, for a nullable column, one `present` flag per row
    /// (`present` is empty for a `REQUIRED` column: every row is present).
    /// Only for chunks whose every data page is `PLAIN`-encoded -- a
    /// dictionary page or any other encoding returns `Ok(None)` at the
    /// first such page, and the caller falls back to the `Vec<Option<T>>`
    /// readers ([`Self::read_int64_column`] and friends), which handle
    /// every encoding. Page layout and decompression are exactly
    /// `Self::read_column`'s.
    pub fn read_plain_fixed_column<T: Copy + Default, const N: usize>(
        &self,
        column_index: usize,
        decode: impl Fn([u8; N]) -> T,
    ) -> Result<Option<(Vec<T>, Vec<bool>)>> {
        let mut pages = self.pages(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        let mut values = Vec::new();
        let mut present = Vec::new();
        while let Some((header, compressed)) = pages.next()? {
            let PageType::Data(data_page_header) = header.page_type else {
                return Ok(None);
            };
            if data_page_header.encoding != page::Encoding::Plain {
                return Ok(None);
            }
            let page_body = pages.body(compressed, header.uncompressed_page_size)?;
            reader::read_plain_fixed(
                &page_body,
                &data_page_header,
                max_def_level,
                &decode,
                &mut values,
                &mut present,
            )?;
        }
        Ok(Some((values, present)))
    }

    /// [`Self::read_plain_fixed_column`] for a dictionary-encoded chunk
    /// (#753): the dictionary page is decoded once through `decode`, then
    /// each `PLAIN_DICTIONARY`/`RLE_DICTIONARY` data page's codes gather
    /// their values straight into `(values, present)`, and a `PLAIN`
    /// fallback page partway through is read as `read_plain_fixed_column`
    /// would. `Ok(None)` without a dictionary page before the first data
    /// page, or at a page of any other encoding, for the caller to fall
    /// back to the `Vec<Option<T>>` readers.
    pub fn read_dictionary_fixed_column<T: Copy + Default, const N: usize>(
        &self,
        column_index: usize,
        decode: impl Fn([u8; N]) -> T,
    ) -> Result<Option<(Vec<T>, Vec<bool>)>> {
        let mut pages = self.pages(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        let mut dictionary: Option<Vec<T>> = None;
        let mut values = Vec::new();
        let mut present = Vec::new();
        while let Some((header, compressed)) = pages.next()? {
            match header.page_type {
                PageType::Data(data_page_header) => {
                    let Some(dictionary) = dictionary.as_deref() else {
                        return Ok(None);
                    };
                    let page_body = pages.body(compressed, header.uncompressed_page_size)?;
                    match data_page_header.encoding {
                        page::Encoding::PlainDictionary | page::Encoding::RleDictionary => {
                            reader::read_dictionary_fixed(
                                &page_body,
                                &data_page_header,
                                max_def_level,
                                dictionary,
                                &mut values,
                                &mut present,
                            )?
                        }
                        page::Encoding::Plain => reader::read_plain_fixed(
                            &page_body,
                            &data_page_header,
                            max_def_level,
                            &decode,
                            &mut values,
                            &mut present,
                        )?,
                        _ => return Ok(None),
                    }
                }
                PageType::Dictionary(dictionary_page_header) => {
                    if dictionary.is_some() {
                        return Err(FileError::UnexpectedDictionaryPage);
                    }
                    let body = pages.body(compressed, header.uncompressed_page_size)?;
                    dictionary = Some(reader::decode_dictionary_fixed(
                        &body,
                        dictionary_page_header.num_values,
                        &decode,
                    )?);
                }
            }
        }
        Ok(dictionary.map(|_| (values, present)))
    }

    /// Decodes a string column chunk straight into `Column::Str`'s
    /// layout (#731): `Ok(Some((offsets, data, present)))` with one offset
    /// per row plus the leading `0`, every value's bytes in one `data`
    /// buffer, and `present` as in [`Self::read_plain_fixed_column`]. Only
    /// for chunks whose every data page is `PLAIN`-encoded (`Ok(None)` at a
    /// dictionary page or another encoding, for the caller to fall back to
    /// [`Self::read_string_column`]). UTF-8 is validated once over the
    /// whole buffer; see [`reader::read_plain_strings`].
    pub fn read_plain_string_column(&self, column_index: usize) -> Result<Option<PlainStrings>> {
        let mut pages = self.pages(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        let mut data = Vec::new();
        let mut offsets = vec![0u32];
        let mut present = Vec::new();
        while let Some((header, compressed)) = pages.next()? {
            let PageType::Data(data_page_header) = header.page_type else {
                return Ok(None);
            };
            if data_page_header.encoding != page::Encoding::Plain {
                return Ok(None);
            }
            let page_body = pages.body(compressed, header.uncompressed_page_size)?;
            reader::read_plain_strings(
                &page_body,
                &data_page_header,
                max_def_level,
                &mut data,
                &mut offsets,
                &mut present,
            )?;
        }
        if u32::try_from(data.len()).is_err() {
            return Err(FileError::ChunkOutOfBounds);
        }
        let data = String::from_utf8(data).map_err(|_| ReadError::InvalidUtf8)?;
        Ok(Some((offsets, data, present)))
    }

    /// The named positions of a fixed-width primitive column chunk,
    /// decoded positionally (#513): only the pages holding a wanted row
    /// are decompressed, and within a page only the wanted rows are read
    /// (see [`reader::read_plain_fixed_at`]); pages past the last wanted
    /// row are never touched. Output is in `positions` order, any order
    /// and duplicates allowed, as `(values, present)` in
    /// [`Self::read_plain_fixed_column`]'s shape (`present` empty for a
    /// `REQUIRED` column). `Ok(None)` at the first non-`PLAIN` page, for
    /// the caller to fall back to the `Vec<Option<T>>` `read_*_column_at`
    /// readers; a position past the chunk's rows is
    /// [`FileError::RowPositionOutOfRange`].
    pub fn read_plain_fixed_column_at<T: Copy + Default, const N: usize>(
        &self,
        column_index: usize,
        decode: impl Fn([u8; N]) -> T,
        positions: &[u32],
    ) -> Result<Option<(Vec<T>, Vec<bool>)>> {
        let mut pages = self.pages(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        let mut wanted: Vec<(u32, usize)> = positions
            .iter()
            .enumerate()
            .map(|(slot, &position)| (position, slot))
            .collect();
        wanted.sort_unstable();
        let mut values = vec![T::default(); positions.len()];
        let mut present = vec![false; positions.len()];

        let mut row_base = 0u32;
        // The wanted rows not yet read, still sorted by position.
        let mut remaining: &[(u32, usize)] = &wanted;
        while !remaining.is_empty() {
            let Some((header, compressed)) = pages.next()? else {
                break;
            };
            let PageType::Data(data_page_header) = header.page_type else {
                return Ok(None);
            };
            if data_page_header.encoding != page::Encoding::Plain {
                return Ok(None);
            }
            let page_rows = u32::try_from(data_page_header.num_values).map_err(|_| {
                FileError::InvalidMetadata {
                    field: "data_page.num_values",
                    value: i64::from(data_page_header.num_values),
                }
            })?;
            let page_end_row = row_base
                .checked_add(page_rows)
                .ok_or(FileError::ChunkOutOfBounds)?;
            let in_page = remaining.partition_point(|&(position, _)| position < page_end_row);
            let (page_wanted, rest) = remaining
                .split_at_checked(in_page)
                .ok_or(FileError::ChunkOutOfBounds)?;
            remaining = rest;
            // Every earlier position went to an earlier page, so none is
            // below `row_base`.
            let rows = page_wanted
                .iter()
                .map(|&(position, slot)| {
                    position
                        .checked_sub(row_base)
                        .map(|row| (row, slot))
                        .ok_or(FileError::RowPositionOutOfRange(position))
                })
                .collect::<Result<Vec<(u32, usize)>>>()?;
            if !rows.is_empty() {
                let page_body = pages.body(compressed, header.uncompressed_page_size)?;
                reader::read_plain_fixed_at(
                    &page_body,
                    &data_page_header,
                    max_def_level,
                    &decode,
                    &rows,
                    &mut values,
                    &mut present,
                )?;
            }
            row_base = page_end_row;
        }
        if let Some(&(position, _)) = remaining.first() {
            return Err(FileError::RowPositionOutOfRange(position));
        }
        if max_def_level == 0 {
            present.clear();
        }
        Ok(Some((values, present)))
    }

    /// Read a column chunk's raw dictionary indices plus its dictionary,
    /// *without* resolving each row's value — for callers that only need to
    /// compare/group rows (e.g. `GROUP BY`), where comparing a `u32` index
    /// is far cheaper per row than allocating and comparing e.g. a
    /// `String`. Returns `None` when the column isn't dictionary-encoded
    /// (callers should fall back to the plain `read_*_column` in that case).
    /// Like [`Self::read_column`]'s dictionary path, but keeps indices raw.
    /// Returns `None` (meaning: fall back to `read_*_column`) when the
    /// chunk has no dictionary page before its first data page. A `PLAIN`
    /// page after the dictionary (a writer's dictionary fallback) is
    /// decoded with `decode_plain`, each value appended to the dictionary
    /// as a new entry (#671: memory follows the file's bytes, never a
    /// dictionary entry copied per row); without `decode_plain` it
    /// returns `None` too.
    #[allow(clippy::type_complexity)]
    fn read_column_dictionary_indices<T>(
        &self,
        column_index: usize,
        decode_dictionary: impl Fn(&[u8], i32) -> reader::Result<Vec<T>>,
        decode_plain: Option<
            &dyn Fn(&[u8], &page::DataPageHeader, u32) -> reader::Result<Vec<Option<T>>>,
        >,
    ) -> Result<Option<DictionaryIndices<T>>> {
        let mut pages = self.pages(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        let mut dictionary: Option<Vec<T>> = None;
        let mut indices: Vec<Option<u32>> = Vec::new();

        while let Some((header, compressed)) = pages.next()? {
            match header.page_type {
                PageType::Data(data_page_header) => {
                    let Some(dict) = dictionary.as_mut() else {
                        return Ok(None);
                    };
                    let dictionary_encoded = matches!(
                        data_page_header.encoding,
                        page::Encoding::PlainDictionary | page::Encoding::RleDictionary
                    );
                    let page_body = pages.body(compressed, header.uncompressed_page_size)?;
                    if dictionary_encoded {
                        indices.extend(reader::read_dictionary_index_column(
                            &page_body,
                            &data_page_header,
                            max_def_level,
                        )?);
                        continue;
                    }
                    let Some(decode_plain) = decode_plain else {
                        return Ok(None);
                    };
                    for value in decode_plain(&page_body, &data_page_header, max_def_level)? {
                        let Some(value) = value else {
                            indices.push(None);
                            continue;
                        };
                        let index =
                            u32::try_from(dict.len()).map_err(|_| FileError::InvalidMetadata {
                                field: "dictionary entries",
                                value: i64::try_from(dict.len()).unwrap_or(i64::MAX),
                            })?;
                        dict.push(value);
                        indices.push(Some(index));
                    }
                }
                PageType::Dictionary(dictionary_page_header) => {
                    if dictionary.is_some() {
                        return Err(FileError::UnexpectedDictionaryPage);
                    }
                    let dictionary_body = pages.body(compressed, header.uncompressed_page_size)?;
                    dictionary = Some(decode_dictionary(
                        &dictionary_body,
                        dictionary_page_header.num_values,
                    )?);
                }
            }
        }

        Ok(dictionary.map(|dict| (dict, indices)))
    }

    /// Decode one nested/repeated leaf column's raw levels and values
    /// (PLAIN encoding only — dictionary-encoded nested leaves aren't
    /// supported yet, see [`FileError::UnsupportedNestedDictionary`]).
    fn read_leaf_entries(&self, column_index: usize, leaf: &SchemaNode) -> Result<LeafEntries> {
        // A nested leaf has one value per level entry, not per row.
        let leaf_values = count(
            "column_chunk.num_values",
            self.column_meta(column_index)?.num_values,
        )?;
        let mut pages = self.pages_with(column_index, leaf_values)?;
        let physical_type = leaf
            .element
            .physical_type
            .ok_or(FileError::MissingColumnMetadata)?;

        let mut out = LeafEntries::default();
        let mut dictionary: Option<Vec<reader::LeafScalar>> = None;

        while let Some((header, compressed)) = pages.next()? {
            match header.page_type {
                // #621: DuckDB and pyarrow dictionary-encode nested leaves
                // (a map's keys) by default.
                PageType::Dictionary(dictionary_page_header) => {
                    if dictionary.is_some() {
                        return Err(FileError::UnexpectedDictionaryPage);
                    }
                    let body = pages.body(compressed, header.uncompressed_page_size)?;
                    let count =
                        usize::try_from(dictionary_page_header.num_values).map_err(|_| {
                            FileError::InvalidMetadata {
                                field: "dictionary_page.num_values",
                                value: i64::from(dictionary_page_header.num_values),
                            }
                        })?;
                    let mut buf: &[u8] = &body;
                    let mut bit_pos = 0usize;
                    dictionary = Some(
                        (0..count)
                            .map(|_| {
                                reader::read_plain_scalar(&mut buf, physical_type, &mut bit_pos)
                            })
                            .collect::<reader::Result<_>>()?,
                    );
                }
                PageType::Data(data_page_header) => {
                    let dictionary_encoded = match data_page_header.encoding {
                        Encoding::Plain => false,
                        Encoding::PlainDictionary | Encoding::RleDictionary => true,
                        other => return Err(FileError::UnsupportedNestedEncoding(other)),
                    };
                    let page_body = pages.body(compressed, header.uncompressed_page_size)?;
                    let (rep_levels, def_levels, mut value_bytes) = reader::split_rep_def_levels(
                        &page_body,
                        &data_page_header,
                        leaf.max_rep_level,
                        leaf.max_def_level,
                    )?;

                    let num_values =
                        usize::try_from(data_page_header.num_values).map_err(|_| {
                            FileError::InvalidMetadata {
                                field: "data_page.num_values",
                                value: i64::from(data_page_header.num_values),
                            }
                        })?;
                    let defined = |i: usize| {
                        def_levels.get(i).copied().unwrap_or(leaf.max_def_level)
                            == leaf.max_def_level
                    };
                    if dictionary_encoded {
                        let dict = dictionary
                            .as_ref()
                            .ok_or(FileError::MissingDictionaryPage)?;
                        let present = (0..num_values).filter(|&i| defined(i)).count();
                        let mut indices =
                            reader::read_dictionary_indices(value_bytes, present)?.into_iter();
                        for i in 0..num_values {
                            if !defined(i) {
                                out.values.push(None);
                                continue;
                            }
                            let index = indices.next().ok_or(ReadError::UnexpectedEof)?;
                            let entry = usize::try_from(index)
                                .ok()
                                .and_then(|i| dict.get(i))
                                .ok_or(ReadError::DictionaryIndexOutOfRange(index))?;
                            out.values.push(Some(entry.clone()));
                        }
                    } else {
                        let mut bit_pos = 0usize;
                        for i in 0..num_values {
                            if defined(i) {
                                let scalar = reader::read_plain_scalar(
                                    &mut value_bytes,
                                    physical_type,
                                    &mut bit_pos,
                                )?;
                                out.values.push(Some(scalar));
                            } else {
                                out.values.push(None);
                            }
                        }
                    }
                    if rep_levels.is_empty() {
                        out.rep_levels.extend(std::iter::repeat_n(0, num_values));
                    } else {
                        out.rep_levels.extend(rep_levels);
                    }
                    if def_levels.is_empty() {
                        out.def_levels
                            .extend(std::iter::repeat_n(leaf.max_def_level, num_values));
                    } else {
                        out.def_levels.extend(def_levels);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Field `field` (a `MAP<UTF8, UTF8>`, e.g. DuckDB's `MAP(VARCHAR,
    /// VARCHAR)`) of this row group, one value per row: `None` for a NULL
    /// map, else its `(key, value)` entries in order (#621). Decodes only
    /// the field's two leaves.
    pub fn read_map_utf8_column(&self, field: &Field) -> Result<Vec<Option<MapEntries>>> {
        let not_a_map = || FileError::NotAStringMap(field.name.clone());
        let leaves = self.file.schema_tree.leaves();
        let (start, end) = (field.leaves.start, field.leaves.end);
        let (Some(key), Some(value)) = (leaves.get(start), leaves.get(start.saturating_add(1)))
        else {
            return Err(not_a_map());
        };
        let is_string =
            |n: &SchemaNode| n.element.physical_type == Some(footer::PhysicalType::ByteArray);
        let shape = field.nested
            && end == start.saturating_add(2)
            && key.max_rep_level == 1
            && value.max_rep_level == 1
            && value.max_def_level == key.max_def_level.saturating_add(1);
        if !shape || !is_string(key) || !is_string(value) {
            return Err(not_a_map());
        }
        let keys = self.read_leaf_entries(start, key)?;
        let values = self.read_leaf_entries(start.saturating_add(1), value)?;
        if keys.def_levels.len() != values.def_levels.len() {
            return Err(not_a_map());
        }
        // A key is defined at its max level; one below is an empty map and
        // anything lower a NULL map.
        let entry_def = key.max_def_level;
        let text = |v: Option<&reader::LeafScalar>| match v {
            Some(reader::LeafScalar::Str(s)) => Some(s.clone()),
            _ => None,
        };
        let mut rows: Vec<Option<MapEntries>> = Vec::new();
        for i in 0..keys.def_levels.len() {
            let def = keys.def_levels.get(i).copied().unwrap_or(0);
            let rep = keys.rep_levels.get(i).copied().unwrap_or(0);
            if def < entry_def {
                if rep != 0 {
                    return Err(not_a_map());
                }
                rows.push((def.saturating_add(1) == entry_def).then(Vec::new));
                continue;
            }
            let k = text(keys.values.get(i).and_then(Option::as_ref)).ok_or_else(not_a_map)?;
            let v = text(values.values.get(i).and_then(Option::as_ref));
            match (rep, rows.last_mut()) {
                (0, _) => rows.push(Some(vec![(k, v)])),
                (_, Some(Some(entries))) => entries.push((k, v)),
                _ => return Err(not_a_map()),
            }
        }
        Ok(rows)
    }

    /// Field `field` (a `LIST<UTF8>`, e.g. DuckDB's `VARCHAR[]` or
    /// pyarrow's `list<string>`) of this row group, one value per row:
    /// `None` for a NULL list, else its elements in order (#801). Decodes
    /// only the field's one leaf.
    pub fn read_list_utf8_column(&self, field: &Field) -> Result<Vec<Option<ListItems>>> {
        let not_a_list = || FileError::NotAStringList(field.name.clone());
        let start = field.leaves.start;
        let leaves = self.file.schema_tree.leaves();
        let Some(leaf) = leaves.get(start) else {
            return Err(not_a_list());
        };
        let shape = field.nested
            && field.leaves.len() == 1
            && field.element.converted_type == Some(footer::ConvertedType::Other(3))
            && leaf.max_rep_level == 1;
        if !shape || leaf.element.physical_type != Some(footer::PhysicalType::ByteArray) {
            return Err(not_a_list());
        }
        let items = self.read_leaf_entries(start, leaf)?;
        // An element is present at its max level and, when it is optional,
        // NULL one below; the level below a row's first element is an empty
        // list and anything lower a NULL list.
        let present = leaf.max_def_level;
        let optional = leaf.element.repetition == Some(footer::Repetition::Optional);
        let element = present.saturating_sub(u32::from(optional));
        let mut rows: Vec<Option<ListItems>> = Vec::new();
        for i in 0..items.def_levels.len() {
            let def = items.def_levels.get(i).copied().unwrap_or(0);
            let rep = items.rep_levels.get(i).copied().unwrap_or(0);
            if def < element {
                if rep != 0 {
                    return Err(not_a_list());
                }
                rows.push((def.saturating_add(1) == element).then(Vec::new));
                continue;
            }
            let item = match items.values.get(i).and_then(Option::as_ref) {
                Some(reader::LeafScalar::Str(s)) if def == present => Some(s.clone()),
                _ if def < present => None,
                _ => return Err(not_a_list()),
            };
            match (rep, rows.last_mut()) {
                (0, _) => rows.push(Some(vec![item])),
                (_, Some(Some(list))) => list.push(item),
                _ => return Err(not_a_list()),
            }
        }
        Ok(rows)
    }

    /// A dictionary-encoded INT64 column's dictionary and per-row indices,
    /// or `None` when the chunk is not dictionary-encoded throughout.
    pub fn read_int64_column_dictionary_indices(
        &self,
        column_index: usize,
    ) -> Result<Option<DictionaryIndices<i64>>> {
        self.read_column_dictionary_indices(column_index, reader::decode_dictionary_int64, None)
    }

    /// A dictionary-encoded DOUBLE column's dictionary and per-row indices,
    /// or `None` when the chunk is not dictionary-encoded throughout.
    pub fn read_double_column_dictionary_indices(
        &self,
        column_index: usize,
    ) -> Result<Option<DictionaryIndices<f64>>> {
        self.read_column_dictionary_indices(column_index, reader::decode_dictionary_double, None)
    }

    /// A dictionary-encoded BOOLEAN column's dictionary and per-row indices,
    /// or `None` when the chunk is not dictionary-encoded throughout.
    pub fn read_boolean_column_dictionary_indices(
        &self,
        column_index: usize,
    ) -> Result<Option<DictionaryIndices<bool>>> {
        self.read_column_dictionary_indices(column_index, reader::decode_dictionary_boolean, None)
    }

    /// A BYTE_ARRAY (UTF-8 string) column's dictionary and per-row
    /// indices, or `None` when the chunk has no dictionary page. Values of
    /// `PLAIN` pages after the dictionary are appended to it, so each row's
    /// value is still `dictionary[index]` (#671).
    pub fn read_string_column_dictionary_indices(
        &self,
        column_index: usize,
    ) -> Result<Option<DictionaryIndices<String>>> {
        self.read_column_dictionary_indices(
            column_index,
            reader::decode_dictionary_string,
            Some(&reader::read_string_column),
        )
    }

    /// [`Self::read_string_column_dictionary_indices`] gathered at
    /// `positions` (#699): the codes at `positions`, remapped onto a
    /// dictionary of only the entries they use, so `k` wanted rows build
    /// at most `k` strings however large the chunk's dictionary is. `None`
    /// when the chunk has no dictionary page.
    pub fn read_string_column_dictionary_at(
        &self,
        column_index: usize,
        positions: &[u32],
    ) -> Result<Option<DictionaryIndices<String>>> {
        let mut pages = self.pages(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        // The dictionary page's body and entry count, decoded only once
        // the wanted entries are known.
        let mut dictionary: Option<(_, usize)> = None;
        // #671: values of `PLAIN` pages after the dictionary, numbered
        // after its entries.
        let mut fallback: Vec<String> = Vec::new();
        let mut indices: Vec<Option<u32>> = Vec::new();

        while let Some((header, compressed)) = pages.next()? {
            match header.page_type {
                PageType::Data(data_page_header) => {
                    let Some((_, entries)) = dictionary.as_ref() else {
                        return Ok(None);
                    };
                    let page_body = pages.body(compressed, header.uncompressed_page_size)?;
                    if matches!(
                        data_page_header.encoding,
                        page::Encoding::PlainDictionary | page::Encoding::RleDictionary
                    ) {
                        indices.extend(reader::read_dictionary_index_column(
                            &page_body,
                            &data_page_header,
                            max_def_level,
                        )?);
                        continue;
                    }
                    for value in
                        reader::read_string_column(&page_body, &data_page_header, max_def_level)?
                    {
                        let Some(value) = value else {
                            indices.push(None);
                            continue;
                        };
                        let next = entries.saturating_add(fallback.len());
                        let index =
                            u32::try_from(next).map_err(|_| FileError::InvalidMetadata {
                                field: "dictionary entries",
                                value: i64::try_from(next).unwrap_or(i64::MAX),
                            })?;
                        fallback.push(value);
                        indices.push(Some(index));
                    }
                }
                PageType::Dictionary(dictionary_page_header) => {
                    if dictionary.is_some() {
                        return Err(FileError::UnexpectedDictionaryPage);
                    }
                    let entries =
                        usize::try_from(dictionary_page_header.num_values).map_err(|_| {
                            FileError::InvalidMetadata {
                                field: "dictionary_page.num_values",
                                value: i64::from(dictionary_page_header.num_values),
                            }
                        })?;
                    let body = pages.body(compressed, header.uncompressed_page_size)?;
                    dictionary = Some((body, entries));
                }
            }
        }
        let Some((body, entries)) = dictionary else {
            return Ok(None);
        };

        let picked = positions
            .iter()
            .map(|&p| {
                usize::try_from(p)
                    .ok()
                    .and_then(|i| indices.get(i).copied())
                    .ok_or(FileError::RowPositionOutOfRange(p))
            })
            .collect::<Result<Vec<_>>>()?;
        // Each used code's id in the output dictionary, ascending.
        let mut remap: Vec<Option<u32>> = vec![None; entries.saturating_add(fallback.len())];
        for &code in picked.iter().flatten() {
            *usize::try_from(code)
                .ok()
                .and_then(|i| remap.get_mut(i))
                .ok_or(ReadError::DictionaryIndexOutOfRange(code))? = Some(0);
        }
        let mut next = 0u32;
        for id in remap.iter_mut().flatten() {
            *id = next;
            next = next.saturating_add(1);
        }

        let mut dict = reader::decode_dictionary_string_picked(
            &body,
            remap.iter().take(entries).map(Option::is_some),
        )?;
        dict.extend(
            fallback
                .into_iter()
                .zip(remap.iter().skip(entries))
                .filter_map(|(value, id)| id.map(|_| value)),
        );
        let codes = picked
            .iter()
            .map(|code| {
                code.and_then(|c| usize::try_from(c).ok())
                    .and_then(|i| remap.get(i).copied().flatten())
            })
            .collect();
        Ok(Some((dict, codes)))
    }

    /// A column chunk's dictionary, decoded without reading its data pages,
    /// when every data page is dictionary-encoded; `None` otherwise. Each
    /// row's value is then one of the dictionary's: a value missing from it
    /// is in no row (#589).
    fn column_dictionary<T>(
        &self,
        column_index: usize,
        decode_dictionary: impl Fn(&[u8], i32) -> reader::Result<Vec<T>>,
    ) -> Result<Option<Vec<T>>> {
        let mut pages = self.pages(column_index)?;
        let mut dictionary: Option<Vec<T>> = None;
        while let Some((header, compressed)) = pages.next()? {
            match header.page_type {
                PageType::Data(data_page_header) => {
                    if dictionary.is_none()
                        || !matches!(
                            data_page_header.encoding,
                            page::Encoding::PlainDictionary | page::Encoding::RleDictionary
                        )
                    {
                        return Ok(None);
                    }
                }
                PageType::Dictionary(dictionary_page_header) => {
                    if dictionary.is_some() {
                        return Err(FileError::UnexpectedDictionaryPage);
                    }
                    let dictionary_body = pages.body(compressed, header.uncompressed_page_size)?;
                    dictionary = Some(decode_dictionary(
                        &dictionary_body,
                        dictionary_page_header.num_values,
                    )?);
                }
            }
        }
        Ok(dictionary)
    }

    /// A BYTE_ARRAY (UTF-8 string) column's dictionary, when the chunk is
    /// dictionary-encoded throughout (see [`Self::column_dictionary`]).
    pub fn read_string_column_dictionary(
        &self,
        column_index: usize,
    ) -> Result<Option<Vec<String>>> {
        self.column_dictionary(column_index, reader::decode_dictionary_string)
    }

    /// An INT64 column's dictionary, when the chunk is dictionary-encoded
    /// throughout.
    pub fn read_int64_column_dictionary(&self, column_index: usize) -> Result<Option<Vec<i64>>> {
        self.column_dictionary(column_index, reader::decode_dictionary_int64)
    }

    /// An INT32 column's dictionary, when the chunk is dictionary-encoded
    /// throughout.
    pub fn read_int32_column_dictionary(&self, column_index: usize) -> Result<Option<Vec<i32>>> {
        self.column_dictionary(column_index, reader::decode_dictionary_int32)
    }

    /// Every value of an INT64 column, one per row (`None` for null).
    pub fn read_int64_column(&self, column_index: usize) -> Result<Vec<Option<i64>>> {
        self.read_column(
            column_index,
            reader::read_int64_column,
            reader::decode_dictionary_int64,
            reader::read_int64_column_dictionary,
        )
    }

    /// Every value of a DOUBLE column, one per row (`None` for null).
    pub fn read_double_column(&self, column_index: usize) -> Result<Vec<Option<f64>>> {
        self.read_column(
            column_index,
            reader::read_double_column,
            reader::decode_dictionary_double,
            reader::read_double_column_dictionary,
        )
    }

    /// Every value of a BOOLEAN column, one per row (`None` for null).
    pub fn read_boolean_column(&self, column_index: usize) -> Result<Vec<Option<bool>>> {
        self.read_column(
            column_index,
            reader::read_boolean_column,
            reader::decode_dictionary_boolean,
            reader::read_boolean_column_dictionary,
        )
    }

    /// Every value of a BYTE_ARRAY column as a UTF-8 string, one per row
    /// (`None` for null).
    pub fn read_string_column(&self, column_index: usize) -> Result<Vec<Option<String>>> {
        self.read_column(
            column_index,
            reader::read_string_column,
            reader::decode_dictionary_string,
            reader::read_string_column_dictionary,
        )
    }

    /// Every value of an INT32 column, one per row (`None` for null).
    pub fn read_int32_column(&self, column_index: usize) -> Result<Vec<Option<i32>>> {
        self.read_column(
            column_index,
            reader::read_int32_column,
            reader::decode_dictionary_int32,
            reader::read_int32_column_dictionary,
        )
    }

    /// Every value of a FLOAT column, one per row (`None` for null).
    pub fn read_float_column(&self, column_index: usize) -> Result<Vec<Option<f32>>> {
        self.read_column(
            column_index,
            reader::read_float_column,
            reader::decode_dictionary_float,
            reader::read_float_column_dictionary,
        )
    }

    /// Gathers `positions` (row indices into this row group's own
    /// numbering) out of the full decoded column (ADR-0026 v1: decodes
    /// the whole column via [`Self::read_int64_column`] first, then
    /// selects only the wanted positions -- real savings come from the
    /// caller only materializing `positions.len()` `Value`s/allocations
    /// downstream, not from skipping the underlying page decode. Page-
    /// level skip-decode is a documented follow-up, not this v1).
    pub fn read_int64_column_at(
        &self,
        column_index: usize,
        positions: &[u32],
    ) -> Result<Vec<Option<i64>>> {
        let full = self.read_int64_column(column_index)?;
        gather(&full, positions)
    }

    /// See [`Self::read_int64_column_at`] -- same v1 gather-after-decode
    /// strategy, DOUBLE physical type.
    pub fn read_double_column_at(
        &self,
        column_index: usize,
        positions: &[u32],
    ) -> Result<Vec<Option<f64>>> {
        let full = self.read_double_column(column_index)?;
        gather(&full, positions)
    }

    /// See [`Self::read_int64_column_at`] -- same v1 gather-after-decode
    /// strategy, BYTE_ARRAY (UTF-8 string) physical type.
    pub fn read_string_column_at(
        &self,
        column_index: usize,
        positions: &[u32],
    ) -> Result<Vec<Option<String>>> {
        let full = self.read_string_column(column_index)?;
        gather(&full, positions)
    }

    /// See [`Self::read_int64_column_at`] -- same v1 gather-after-decode
    /// strategy, INT32 physical type.
    pub fn read_int32_column_at(
        &self,
        column_index: usize,
        positions: &[u32],
    ) -> Result<Vec<Option<i32>>> {
        let full = self.read_int32_column(column_index)?;
        gather(&full, positions)
    }

    /// See [`Self::read_int64_column_at`] -- same v1 gather-after-decode
    /// strategy, FLOAT physical type.
    pub fn read_float_column_at(
        &self,
        column_index: usize,
        positions: &[u32],
    ) -> Result<Vec<Option<f32>>> {
        let full = self.read_float_column(column_index)?;
        gather(&full, positions)
    }

    /// See [`Self::read_int64_column_at`] -- same v1 gather-after-decode
    /// strategy, BOOLEAN physical type.
    pub fn read_boolean_column_at(
        &self,
        column_index: usize,
        positions: &[u32],
    ) -> Result<Vec<Option<bool>>> {
        let full = self.read_boolean_column(column_index)?;
        gather(&full, positions)
    }

    /// Every value of a FIXED_LEN_BYTE_ARRAY column, one per row (`None`
    /// for null).
    pub fn read_fixed_len_byte_array_column(
        &self,
        column_index: usize,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let type_length = self.type_length_for(column_index)?;
        self.read_column(
            column_index,
            move |body, header, max_def| {
                reader::read_fixed_len_byte_array_column(body, header, max_def, type_length)
            },
            move |body, num_values| {
                reader::decode_dictionary_fixed_len_byte_array(body, num_values, type_length)
            },
            reader::read_fixed_len_byte_array_column_dictionary,
        )
    }

    /// A BYTE_ARRAY column as raw bytes, whatever its content (no UTF-8
    /// check): `BLOB` and other unannotated binary columns.
    pub fn read_binary_column(&self, column_index: usize) -> Result<Vec<Option<Vec<u8>>>> {
        self.read_column(
            column_index,
            reader::read_bytes_column,
            reader::decode_dictionary_bytes,
            reader::read_fixed_len_byte_array_column_dictionary,
        )
    }

    /// Every raw value of an INT96 column, one per row (`None` for null);
    /// [`Self::read_timestamp_column`] converts them to microseconds.
    pub fn read_int96_column(&self, column_index: usize) -> Result<Vec<Option<reader::Int96>>> {
        self.read_column(
            column_index,
            reader::read_int96_column,
            reader::decode_dictionary_int96,
            reader::read_int96_column_dictionary,
        )
    }

    fn type_length_for(&self, column_index: usize) -> Result<usize> {
        self.file.type_length(column_index)
    }

    /// Read an INT64 (`TIMESTAMP_MILLIS`/`TIMESTAMP_MICROS`) or INT96
    /// (legacy) timestamp column, normalized to microseconds since the Unix
    /// epoch (UTC) regardless of source encoding (#50, #51).
    pub fn read_timestamp_column(&self, column_index: usize) -> Result<Vec<Option<i64>>> {
        match self.column_meta(column_index)?.physical_type {
            footer::PhysicalType::Int64 => {
                let is_millis = matches!(
                    self.file.converted_type(column_index),
                    Some(footer::ConvertedType::TimestampMillis)
                );
                let raw = self.read_int64_column(column_index)?;
                if !is_millis {
                    return Ok(raw);
                }
                raw.into_iter()
                    .map(|v| {
                        v.map(|v| v.checked_mul(1000).ok_or(FileError::TimestampOutOfRange))
                            .transpose()
                    })
                    .collect()
            }
            footer::PhysicalType::Int96 => {
                let raw = self.read_int96_column(column_index)?;
                raw.into_iter()
                    .map(|v| {
                        v.map(|v| int96_to_epoch_micros(v).ok_or(FileError::TimestampOutOfRange))
                            .transpose()
                    })
                    .collect()
            }
            other => Err(FileError::UnsupportedTimestampPhysicalType(other)),
        }
    }

    /// Read a `DECIMAL`-annotated column (INT32, INT64, or
    /// `FIXED_LEN_BYTE_ARRAY` physical type), preserving exact precision via
    /// [`crate::storage::parquet::decimal::Decimal`] rather than converting to `f64` (#52, #53).
    pub fn read_decimal_column(
        &self,
        column_index: usize,
    ) -> Result<Vec<Option<crate::storage::parquet::decimal::Decimal>>> {
        let scale = self.file.decimal_scale(column_index)?;
        match self.column_meta(column_index)?.physical_type {
            footer::PhysicalType::Int32 => {
                let raw = self.read_int32_column(column_index)?;
                Ok(raw
                    .into_iter()
                    .map(|v| {
                        v.map(|v| crate::storage::parquet::decimal::Decimal {
                            unscaled: v as i128,
                            scale,
                        })
                    })
                    .collect())
            }
            footer::PhysicalType::Int64 => {
                let raw = self.read_int64_column(column_index)?;
                Ok(raw
                    .into_iter()
                    .map(|v| {
                        v.map(|v| crate::storage::parquet::decimal::Decimal {
                            unscaled: v as i128,
                            scale,
                        })
                    })
                    .collect())
            }
            footer::PhysicalType::FixedLenByteArray => {
                let raw = self.read_fixed_len_byte_array_column(column_index)?;
                raw.into_iter()
                    .map(|v| {
                        v.map(|bytes| {
                            // More than 16 bytes would lose the high bits in
                            // an i128 (#632): beyond DECIMAL(38), refused.
                            if bytes.len() > 16 {
                                return Err(FileError::InvalidMetadata {
                                    field: "DECIMAL fixed_len_byte_array length",
                                    value: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                                });
                            }
                            Ok(crate::storage::parquet::decimal::Decimal {
                                unscaled: crate::storage::parquet::decimal::from_be_bytes(&bytes),
                                scale,
                            })
                        })
                        .transpose()
                    })
                    .collect()
            }
            other => Err(FileError::UnsupportedDecimalPhysicalType(other)),
        }
    }
}

/// Julian day number for the Unix epoch (1970-01-01T00:00:00Z).
const JULIAN_DAY_UNIX_EPOCH: i64 = 2_440_588;

/// INT96 = 8-byte nanoseconds-within-day + 4-byte Julian day number.
/// `None` when the instant does not fit `i64` microseconds (only a corrupt
/// or far-off value: about 292,000 years from the epoch).
fn int96_to_epoch_micros(v: reader::Int96) -> Option<i64> {
    let days_since_epoch = i64::from(v.julian_day).checked_sub(JULIAN_DAY_UNIX_EPOCH)?;
    days_since_epoch
        .checked_mul(86_400_000_000)?
        .checked_add(v.time_nanos / 1000)
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

    #[allow(non_snake_case)]
    mod mcdc_map_shape {
        //! Tagged MC/DC vectors for `read_map_utf8_column`'s shape check,
        //! over tools/gen_parquet_fixtures.sh's `nested_shapes.parquet`:
        //! `lst LIST(VARCHAR)`, `ik MAP(INT, VARCHAR)`, `iv MAP(VARCHAR,
        //! INT)`, `m MAP(VARCHAR, VARCHAR)`.
        use super::*;

        fn read(field: &str) -> Result<Vec<Option<MapEntries>>> {
            let bytes = std::fs::read("tests/fixtures/parquet/nested_shapes.parquet").unwrap();
            let file = ParquetFile::open(&bytes).unwrap();
            let field = file.fields().into_iter().find(|f| f.name == field).unwrap();
            let rg = file.row_group(0).unwrap();
            rg.read_map_utf8_column(&field)
        }

        // storage_parquet_parquet_file_read_map_utf8_column_62508017:
        // `!shape || !is_string(key) || !is_string(value)`
        #[test]
        fn mcdc__storage_parquet_parquet_file_read_map_utf8_column_62508017__v1_a_list_is_not_a_map(
        ) {
            assert!(matches!(read("lst"), Err(FileError::NotAStringMap(n)) if n == "lst"));
        }

        #[test]
        fn mcdc__storage_parquet_parquet_file_read_map_utf8_column_62508017__v2_integer_keys_are_refused(
        ) {
            assert!(matches!(read("ik"), Err(FileError::NotAStringMap(_))));
        }

        #[test]
        fn mcdc__storage_parquet_parquet_file_read_map_utf8_column_62508017__v3_integer_values_are_refused(
        ) {
            assert!(matches!(read("iv"), Err(FileError::NotAStringMap(_))));
        }

        #[test]
        fn mcdc__storage_parquet_parquet_file_read_map_utf8_column_62508017__v4_a_string_map_reads()
        {
            assert_eq!(
                read("m").unwrap(),
                [Some(vec![("a".into(), Some("1".into()))])]
            );
        }
    }

    #[allow(non_snake_case)]
    mod mcdc_list_shape {
        //! Tagged MC/DC vectors for `read_list_utf8_column`'s shape check
        //! (#801), over tools/gen_parquet_fixtures.sh's
        //! `nested_shapes.parquet`: `lst VARCHAR[]`, `m MAP(VARCHAR,
        //! VARCHAR)`, `il INTEGER[]`.
        use super::*;

        fn read(field: &str) -> Result<Vec<Option<ListItems>>> {
            let bytes = std::fs::read("tests/fixtures/parquet/nested_shapes.parquet").unwrap();
            let file = ParquetFile::open(&bytes).unwrap();
            let field = file.fields().into_iter().find(|f| f.name == field).unwrap();
            let rg = file.row_group(0).unwrap();
            rg.read_list_utf8_column(&field)
        }

        // storage_parquet_parquet_file_read_list_utf8_column_f3c5bfff:
        // `!shape || leaf.element.physical_type != Some(ByteArray)`
        #[test]
        fn mcdc__storage_parquet_parquet_file_read_list_utf8_column_f3c5bfff__v1_a_string_list_reads(
        ) {
            assert_eq!(read("lst").unwrap(), [Some(vec![Some("x".into())])]);
        }

        #[test]
        fn mcdc__storage_parquet_parquet_file_read_list_utf8_column_f3c5bfff__v2_a_map_is_not_a_list(
        ) {
            assert!(matches!(read("m"), Err(FileError::NotAStringList(n)) if n == "m"));
        }

        #[test]
        fn mcdc__storage_parquet_parquet_file_read_list_utf8_column_f3c5bfff__v3_an_integer_list_is_refused(
        ) {
            assert!(matches!(read("il"), Err(FileError::NotAStringList(n)) if n == "il"));
        }
    }

    /// A MAP between two flat columns (tools/gen_parquet_fixtures.sh):
    /// leaves `id`, `h.key_value.key`, `h.key_value.value`, `x`.
    #[test]
    fn leaves_after_a_group_are_indexed_by_leaf_not_by_schema_element() {
        let bytes = std::fs::read("tests/fixtures/parquet/nested_map.parquet").unwrap();
        let file = ParquetFile::open(&bytes).unwrap();
        let fields: Vec<(String, std::ops::Range<usize>, bool)> = file
            .fields()
            .into_iter()
            .map(|f| (f.name, f.leaves, f.nested))
            .collect();
        assert_eq!(
            fields,
            [
                ("id".to_string(), 0..1, false),
                ("h".to_string(), 1..3, true),
                ("x".to_string(), 3..4, false)
            ]
        );
        let names: Vec<&str> = (0..4)
            .map(|i| file.leaf_element(i).unwrap().name.as_str())
            .collect();
        assert_eq!(names, ["id", "key", "value", "x"]);
        assert!(file.leaf_element(4).is_none());
        // optional h, repeated key_value: key 2, value 3; x is top level.
        let levels: Vec<u32> = (0..4).map(|i| file.max_definition_level(i)).collect();
        assert_eq!(levels, [1, 2, 3, 1]);
        // The column after the map reads its own chunk.
        let rg = file.row_group(0).unwrap();
        assert_eq!(
            rg.read_int32_column(3).unwrap(),
            [Some(0), Some(10), Some(20), Some(30), Some(40)]
        );
        assert_eq!(rg.schema_element(3).unwrap().name, "x");
    }

    struct StructWriter {
        buf: Vec<u8>,
        last_field_id: i16,
    }

    impl StructWriter {
        fn new() -> Self {
            StructWriter {
                buf: Vec::new(),
                last_field_id: 0,
            }
        }
        fn write_varint(&mut self, mut v: u64) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                self.buf.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        fn zigzag(v: i64) -> u64 {
            ((v << 1) ^ (v >> 63)) as u64
        }
        fn field_header(&mut self, field_id: i16, ctype: u8) {
            let delta = field_id - self.last_field_id;
            assert!((1..=15).contains(&delta));
            self.buf.push(((delta as u8) << 4) | ctype);
            self.last_field_id = field_id;
        }
        fn i32_field(&mut self, field_id: i16, v: i32) {
            self.field_header(field_id, 0x05);
            self.write_varint(Self::zigzag(v as i64));
        }
        fn i64_field(&mut self, field_id: i16, v: i64) {
            self.field_header(field_id, 0x06);
            self.write_varint(Self::zigzag(v));
        }
        fn string_field(&mut self, field_id: i16, s: &str) {
            self.field_header(field_id, 0x08);
            self.write_varint(s.len() as u64);
            self.buf.extend_from_slice(s.as_bytes());
        }
        fn struct_field(&mut self, field_id: i16, inner: Vec<u8>) {
            self.field_header(field_id, 0x0c);
            self.buf.extend_from_slice(&inner);
        }
        fn list_of_structs_field(&mut self, field_id: i16, items: Vec<Vec<u8>>) {
            self.field_header(field_id, 0x09);
            let n = items.len();
            if n < 15 {
                self.buf.push(((n as u8) << 4) | 0x0c);
            } else {
                self.buf.push((15u8 << 4) | 0x0c);
                self.write_varint(n as u64);
            }
            for item in items {
                self.buf.extend_from_slice(&item);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            self.buf.push(0x00);
            self.buf
        }
    }

    fn build_schema_element(name: &str, physical_type: i32, repetition: i32) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, physical_type);
        w.i32_field(3, repetition);
        w.string_field(4, name);
        w.finish()
    }

    /// The schema root (message) node: no physical type, `num_children`
    /// leaves following it -- real writers always set `num_children` on
    /// group nodes, which `is_flat_schema` relies on.
    fn build_root_schema_element(num_children: i32) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.string_field(4, "schema");
        w.i32_field(5, num_children);
        w.finish()
    }

    fn build_page_header(num_values: i32, page_size: i32) -> Vec<u8> {
        let mut dph = StructWriter::new();
        dph.i32_field(1, num_values);
        dph.i32_field(2, 0); // encoding = PLAIN
        dph.i32_field(3, 3); // def level encoding = RLE
        dph.i32_field(4, 3); // rep level encoding = RLE
        let dph_bytes = dph.finish();

        let mut w = StructWriter::new();
        w.i32_field(1, 0); // DATA_PAGE
        w.i32_field(2, page_size);
        w.i32_field(3, page_size);
        w.struct_field(5, dph_bytes);
        w.finish()
    }

    /// Build one column chunk's page bytes (header + PLAIN INT64 body, no
    /// nulls) and return `(page_bytes, ColumnMetaData thrift bytes)` where
    /// the metadata's offsets are relative to `base_offset`.
    fn build_int64_chunk(values: &[i64], base_offset: i64) -> (Vec<u8>, Vec<u8>) {
        let mut body = Vec::new();
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let header = build_page_header(values.len() as i32, body.len() as i32);
        let mut page_bytes = header.clone();
        page_bytes.extend_from_slice(&body);

        let mut meta = StructWriter::new();
        meta.i32_field(1, 2); // INT64
        meta.field_header(3, 0x09); // path_in_schema list<string>
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i64_field(5, values.len() as i64); // num_values
        meta.i64_field(6, page_bytes.len() as i64); // total_uncompressed_size
        meta.i64_field(7, page_bytes.len() as i64); // total_compressed_size
        meta.i64_field(9, base_offset); // data_page_offset (start of the page header)
        let meta_bytes = meta.finish();

        (page_bytes, meta_bytes)
    }

    /// Build a column chunk's page bytes as *multiple* consecutive
    /// `DATA_PAGE`s (one per slice in `pages`), and its `ColumnMetaData`
    /// covering the whole concatenated span -- regression coverage for #49
    /// (silently truncating a multi-page chunk to its first page).
    fn build_int64_chunk_multi_page(pages: &[&[i64]], base_offset: i64) -> (Vec<u8>, Vec<u8>) {
        let mut page_bytes = Vec::new();
        let mut total_values = 0i64;
        for values in pages {
            let mut body = Vec::new();
            for v in *values {
                body.extend_from_slice(&v.to_le_bytes());
            }
            page_bytes
                .extend_from_slice(&build_page_header(values.len() as i32, body.len() as i32));
            page_bytes.extend_from_slice(&body);
            total_values += values.len() as i64;
        }

        let mut meta = StructWriter::new();
        meta.i32_field(1, 2); // INT64
        meta.field_header(3, 0x09); // path_in_schema list<string>
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i64_field(5, total_values); // num_values
        meta.i64_field(6, page_bytes.len() as i64); // total_uncompressed_size
        meta.i64_field(7, page_bytes.len() as i64); // total_compressed_size
        meta.i64_field(9, base_offset); // data_page_offset
        let meta_bytes = meta.finish();

        (page_bytes, meta_bytes)
    }

    /// Like [`build_int64_chunk`] but for any physical type, given its
    /// already-PLAIN-encoded page body -- one round-trip-through-`ParquetFile`
    /// builder for every `read_*_column` (#407: these otherwise have no
    /// coverage above `reader::read_*_column`'s own unit-level tests).
    fn build_typed_chunk(
        physical_type: i32,
        body: &[u8],
        num_values: i32,
        base_offset: i64,
    ) -> (Vec<u8>, Vec<u8>) {
        let header = build_page_header(num_values, body.len() as i32);
        let mut page_bytes = header.clone();
        page_bytes.extend_from_slice(body);

        let mut meta = StructWriter::new();
        meta.i32_field(1, physical_type);
        meta.field_header(3, 0x09); // path_in_schema list<string>
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i64_field(5, num_values as i64); // num_values
        meta.i64_field(6, page_bytes.len() as i64); // total_uncompressed_size
        meta.i64_field(7, page_bytes.len() as i64); // total_compressed_size
        meta.i64_field(9, base_offset); // data_page_offset
        let meta_bytes = meta.finish();

        (page_bytes, meta_bytes)
    }

    /// Like [`build_file_from_chunk`] but for any physical type (and,
    /// for `FIXED_LEN_BYTE_ARRAY`, its `type_length`).
    fn build_file_from_chunk_typed(
        page_bytes: &[u8],
        meta_bytes: Vec<u8>,
        num_rows: i64,
        physical_type: i32,
        type_length: Option<i32>,
    ) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");
        file.extend_from_slice(page_bytes);

        let column_chunk = build_column_chunk(4, meta_bytes);
        let row_group = build_row_group(vec![column_chunk], page_bytes.len() as i64, num_rows);
        let root = build_root_schema_element(1);
        let mut col = StructWriter::new();
        col.i32_field(1, physical_type);
        if let Some(len) = type_length {
            col.i32_field(2, len);
        }
        col.i32_field(3, 0); // REQUIRED -> max_def_level 0
        col.string_field(4, "v");
        let col = col.finish();
        let metadata = build_file_metadata(vec![root, col], num_rows, vec![row_group]);
        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        file
    }

    /// Like [`build_file_from_chunk_typed`] but also annotates the leaf
    /// with a `converted_type`/`scale` (`TIMESTAMP_MILLIS`, `DECIMAL`).
    fn build_file_from_chunk_annotated(
        page_bytes: &[u8],
        meta_bytes: Vec<u8>,
        num_rows: i64,
        physical_type: i32,
        type_length: Option<i32>,
        converted_type: i32,
        scale: Option<i32>,
    ) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");
        file.extend_from_slice(page_bytes);

        let column_chunk = build_column_chunk(4, meta_bytes);
        let row_group = build_row_group(vec![column_chunk], page_bytes.len() as i64, num_rows);
        let root = build_root_schema_element(1);
        let mut col = StructWriter::new();
        col.i32_field(1, physical_type);
        if let Some(len) = type_length {
            col.i32_field(2, len);
        }
        col.i32_field(3, 0); // REQUIRED -> max_def_level 0
        col.string_field(4, "v");
        col.i32_field(6, converted_type);
        if let Some(s) = scale {
            col.i32_field(7, s);
        }
        let col = col.finish();
        let metadata = build_file_metadata(vec![root, col], num_rows, vec![row_group]);
        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        file
    }

    /// Bit-packs `indices` (one RLE/bit-packed hybrid run, `bit_width`
    /// bits each) the same way a real PLAIN_DICTIONARY/RLE_DICTIONARY data
    /// page body does -- mirrors `reader::tests::encode_dictionary_indices`
    /// but lives here too since that one is private to `reader`'s own test
    /// module.
    fn encode_dictionary_indices(indices: &[u32], bit_width: u32) -> Vec<u8> {
        let n = indices.len();
        let num_groups = n.div_ceil(8);
        let mut out = vec![bit_width as u8];
        out.push(((num_groups as u64) << 1 | 1) as u8);

        let total_bits = num_groups * 8 * bit_width as usize;
        let mut bytes = vec![0u8; total_bits.div_ceil(8)];
        let mut bit_pos = 0usize;
        for g in 0..num_groups {
            for i in 0..8 {
                let value = indices.get(g * 8 + i).copied().unwrap_or(0);
                for bit in 0..bit_width as usize {
                    if (value >> bit) & 1 == 1 {
                        let global_bit = bit_pos + bit;
                        bytes[global_bit / 8] |= 1 << (global_bit % 8);
                    }
                }
                bit_pos += bit_width as usize;
            }
        }
        out.extend_from_slice(&bytes);
        out
    }

    fn build_dictionary_page_header(num_values: i32, page_size: i32) -> Vec<u8> {
        let mut dph = StructWriter::new();
        dph.i32_field(1, num_values);
        dph.i32_field(2, 0); // encoding = PLAIN
        let dph_bytes = dph.finish();

        let mut w = StructWriter::new();
        w.i32_field(1, 2); // DICTIONARY_PAGE
        w.i32_field(2, page_size);
        w.i32_field(3, page_size);
        w.struct_field(7, dph_bytes);
        w.finish()
    }

    fn build_dictionary_encoded_data_page_header(num_values: i32, page_size: i32) -> Vec<u8> {
        let mut dph = StructWriter::new();
        dph.i32_field(1, num_values);
        dph.i32_field(2, 2); // encoding = PLAIN_DICTIONARY
        dph.i32_field(3, 3); // def level encoding = RLE
        dph.i32_field(4, 3); // rep level encoding = RLE
        let dph_bytes = dph.finish();

        let mut w = StructWriter::new();
        w.i32_field(1, 0); // DATA_PAGE
        w.i32_field(2, page_size);
        w.i32_field(3, page_size);
        w.struct_field(5, dph_bytes);
        w.finish()
    }

    /// Build a dictionary-encoded INT64 column chunk: a DICTIONARY_PAGE
    /// (PLAIN-encoded `dict_values`) followed by one PLAIN_DICTIONARY
    /// DATA_PAGE whose body is `indices` bit-packed at the smallest width
    /// that fits `dict_values.len() - 1` (no definition levels, since the
    /// synthetic schema this pairs with is REQUIRED -- max_def_level 0).
    fn build_int64_dictionary_chunk(
        dict_values: &[i64],
        indices: &[u32],
        base_offset: i64,
    ) -> (Vec<u8>, Vec<u8>) {
        let mut dict_body = Vec::new();
        for v in dict_values {
            dict_body.extend_from_slice(&v.to_le_bytes());
        }
        let dict_header =
            build_dictionary_page_header(dict_values.len() as i32, dict_body.len() as i32);
        let mut page_bytes = dict_header;
        page_bytes.extend_from_slice(&dict_body);

        let bit_width =
            (usize::BITS - (dict_values.len().saturating_sub(1)).leading_zeros()).clamp(1, 32);
        let data_body = encode_dictionary_indices(indices, bit_width);
        let data_header =
            build_dictionary_encoded_data_page_header(indices.len() as i32, data_body.len() as i32);
        page_bytes.extend_from_slice(&data_header);
        page_bytes.extend_from_slice(&data_body);

        let mut meta = StructWriter::new();
        meta.i32_field(1, 2); // INT64
        meta.field_header(3, 0x09); // path_in_schema list<string>
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i64_field(5, indices.len() as i64); // num_values
        meta.i64_field(6, page_bytes.len() as i64); // total_uncompressed_size
        meta.i64_field(7, page_bytes.len() as i64); // total_compressed_size
        meta.i64_field(9, base_offset); // dictionary_page_offset==data_page_offset here
        let meta_bytes = meta.finish();

        (page_bytes, meta_bytes)
    }

    fn build_column_chunk(file_offset: i64, meta_data: Vec<u8>) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i64_field(2, file_offset);
        w.struct_field(3, meta_data);
        w.finish()
    }

    fn build_row_group(columns: Vec<Vec<u8>>, total_byte_size: i64, num_rows: i64) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.list_of_structs_field(1, columns);
        w.i64_field(2, total_byte_size);
        w.i64_field(3, num_rows);
        w.finish()
    }

    fn build_file_metadata(
        schema: Vec<Vec<u8>>,
        num_rows: i64,
        row_groups: Vec<Vec<u8>>,
    ) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, 1);
        w.list_of_structs_field(2, schema);
        w.i64_field(3, num_rows);
        w.list_of_structs_field(4, row_groups);
        w.string_field(6, "column-rs");
        w.finish()
    }

    /// Build a full synthetic single-column, multi-row-group Parquet file
    /// from a list of per-row-group INT64 value slices.
    fn build_file(row_group_values: &[&[i64]]) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");

        let mut row_groups = Vec::new();
        let mut total_rows = 0i64;
        for values in row_group_values {
            let base_offset = file.len() as i64;
            let (page_bytes, meta_bytes) = build_int64_chunk(values, base_offset);
            file.extend_from_slice(&page_bytes);

            let column_chunk = build_column_chunk(base_offset, meta_bytes);
            row_groups.push(build_row_group(
                vec![column_chunk],
                page_bytes.len() as i64,
                values.len() as i64,
            ));
            total_rows += values.len() as i64;
        }

        let root = build_root_schema_element(1);
        let col = build_schema_element("v", 2 /* INT64 */, 0 /* REQUIRED */);
        let metadata = build_file_metadata(vec![root, col], total_rows, row_groups);

        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        file
    }

    #[test]
    fn reads_single_row_group_file() {
        let file_bytes = build_file(&[&[1, 2, 3]]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        assert_eq!(file.num_row_groups(), 1);
        assert_eq!(file.num_rows(), 3);

        let rg = file.row_group(0).unwrap();
        assert_eq!(rg.num_rows(), 3);
        let values = rg.read_int64_column(0).unwrap();
        assert_eq!(values, vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn iterates_multiple_row_groups_and_tracks_row_offsets() {
        let file_bytes = build_file(&[&[1, 2], &[3, 4, 5]]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        assert_eq!(file.num_row_groups(), 2);
        assert_eq!(file.num_rows(), 5);

        let mut row_offset = 0i64;
        let mut all_values = Vec::new();
        for rg in file.row_groups() {
            row_offset += rg.num_rows();
            all_values.extend(rg.read_int64_column(0).unwrap());
        }
        assert_eq!(row_offset, file.num_rows());
        assert_eq!(
            all_values,
            vec![Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
    }

    #[test]
    fn reads_all_pages_of_a_multi_page_column_chunk() {
        let (page_bytes, meta_bytes) =
            build_int64_chunk_multi_page(&[&[1, 2, 3], &[4, 5], &[6, 7, 8, 9]], 4);

        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");
        file.extend_from_slice(&page_bytes);

        let column_chunk = build_column_chunk(4, meta_bytes);
        let row_group = build_row_group(vec![column_chunk], page_bytes.len() as i64, 9);
        let root = build_root_schema_element(1);
        let col = build_schema_element("v", 2, 0);
        let metadata = build_file_metadata(vec![root, col], 9, vec![row_group]);
        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");

        let parsed = ParquetFile::open(&file).unwrap();
        let rg = parsed.row_group(0).unwrap();
        let values = rg.read_int64_column(0).unwrap();
        assert_eq!(
            values,
            vec![
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                Some(8),
                Some(9)
            ]
        );
    }

    #[test]
    fn read_plain_fixed_column_concatenates_pages_like_the_option_reader() {
        let (page_bytes, meta_bytes) =
            build_int64_chunk_multi_page(&[&[1, 2, 3], &[4, 5], &[6, 7, 8, 9]], 4);
        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");
        file.extend_from_slice(&page_bytes);
        let column_chunk = build_column_chunk(4, meta_bytes);
        let row_group = build_row_group(vec![column_chunk], page_bytes.len() as i64, 9);
        let root = build_root_schema_element(1);
        let col = build_schema_element("v", 2, 0);
        let metadata = build_file_metadata(vec![root, col], 9, vec![row_group]);
        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");

        let parsed = ParquetFile::open(&file).unwrap();
        let rg = parsed.row_group(0).unwrap();
        let (values, present) = rg
            .read_plain_fixed_column(0, i64::from_le_bytes)
            .unwrap()
            .expect("all pages are PLAIN");
        assert_eq!(values, (1..=9).collect::<Vec<i64>>());
        assert!(present.is_empty(), "REQUIRED column: no per-row flags");
        assert_eq!(
            values.iter().copied().map(Some).collect::<Vec<_>>(),
            rg.read_int64_column(0).unwrap()
        );
    }

    /// One REQUIRED INT64 column in three PLAIN pages: rows 0..9 hold 1..=9.
    fn three_page_int64_file() -> Vec<u8> {
        let (page_bytes, meta_bytes) =
            build_int64_chunk_multi_page(&[&[1, 2, 3], &[4, 5], &[6, 7, 8, 9]], 4);
        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");
        file.extend_from_slice(&page_bytes);
        let column_chunk = build_column_chunk(4, meta_bytes);
        let row_group = build_row_group(vec![column_chunk], page_bytes.len() as i64, 9);
        let root = build_root_schema_element(1);
        let col = build_schema_element("v", 2, 0);
        let metadata = build_file_metadata(vec![root, col], 9, vec![row_group]);
        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        file
    }

    // `read_plain_fixed_column_at` stops when its pages or its wanted rows
    // run out (one condition each since #671).
    #[test]
    #[allow(non_snake_case)]
    fn plain_fixed_at_reads_across_pages_in_positions_order() {
        let file = three_page_int64_file();
        let parsed = ParquetFile::open(&file).unwrap();
        let rg = parsed.row_group(0).unwrap();
        // Unsorted, a duplicate, rows from every page.
        let positions = [8u32, 0, 8, 2, 5];
        let (values, present) = rg
            .read_plain_fixed_column_at(0, i64::from_le_bytes, &positions)
            .unwrap()
            .expect("PLAIN chunk");
        assert_eq!(values, vec![9, 1, 9, 3, 6]);
        assert!(present.is_empty(), "REQUIRED column");
        assert_eq!(
            values.iter().copied().map(Some).collect::<Vec<_>>(),
            rg.read_int64_column_at(0, &positions).unwrap()
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn plain_fixed_at_pages_running_out_with_a_row_still_wanted_is_out_of_range() {
        let file = three_page_int64_file();
        let parsed = ParquetFile::open(&file).unwrap();
        let rg = parsed.row_group(0).unwrap();
        let err = rg
            .read_plain_fixed_column_at(0, i64::from_le_bytes, &[3, 9])
            .unwrap_err();
        assert!(
            matches!(err, FileError::RowPositionOutOfRange(9)),
            "{err:?}"
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn plain_fixed_at_wanted_rows_running_out_stops_before_the_remaining_pages() {
        let file = three_page_int64_file();
        let parsed = ParquetFile::open(&file).unwrap();
        let rg = parsed.row_group(0).unwrap();
        // Only first-page rows: the second and third pages are never
        // reached (and an empty request reads nothing at all).
        let (values, _) = rg
            .read_plain_fixed_column_at(0, i64::from_le_bytes, &[2, 0])
            .unwrap()
            .unwrap();
        assert_eq!(values, vec![3, 1]);
        let (empty, _) = rg
            .read_plain_fixed_column_at(0, i64::from_le_bytes, &[])
            .unwrap()
            .unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn read_plain_fixed_column_at_declines_a_dictionary_encoded_chunk() {
        let (page_bytes, meta_bytes) = build_int64_dictionary_chunk(&[10, 20], &[1, 0, 1], 4);
        let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, 3);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        assert!(rg
            .read_plain_fixed_column_at(0, i64::from_le_bytes, &[2])
            .unwrap()
            .is_none());
    }

    #[test]
    fn read_plain_fixed_column_declines_a_dictionary_encoded_chunk() {
        let (page_bytes, meta_bytes) = build_int64_dictionary_chunk(&[10, 20], &[1, 0, 1], 4);
        let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, 3);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        assert!(rg
            .read_plain_fixed_column(0, i64::from_le_bytes)
            .unwrap()
            .is_none());
        // The fallback the caller then takes still reads the chunk.
        assert_eq!(
            rg.read_int64_column(0).unwrap(),
            vec![Some(20), Some(10), Some(20)]
        );
    }

    /// One data page of [`build_fixed_dictionary_chunk`]: dictionary codes,
    /// or a `PLAIN` fallback page's body and value count.
    enum DictTestPage<'a> {
        Codes(&'a [u32]),
        Plain(&'a [u8], i32),
    }

    /// A REQUIRED fixed-width chunk (#753): a dictionary page holding
    /// `dict_body` (`dict_len` entries), then `pages` in order, the codes
    /// bit-packed at the width `dict_len` needs.
    fn build_fixed_dictionary_chunk(
        physical_type: i32,
        dict_body: &[u8],
        dict_len: usize,
        pages: &[DictTestPage<'_>],
    ) -> Vec<u8> {
        let mut page_bytes = build_dictionary_page_header(dict_len as i32, dict_body.len() as i32);
        page_bytes.extend_from_slice(dict_body);
        let bit_width = (usize::BITS - dict_len.saturating_sub(1).leading_zeros()).clamp(1, 32);
        let mut num_rows = 0i64;
        for page in pages {
            let (header, body) = match page {
                DictTestPage::Codes(codes) => {
                    let body = encode_dictionary_indices(codes, bit_width);
                    let header = build_dictionary_encoded_data_page_header(
                        codes.len() as i32,
                        body.len() as i32,
                    );
                    num_rows += codes.len() as i64;
                    (header, body)
                }
                DictTestPage::Plain(body, n) => {
                    num_rows += i64::from(*n);
                    (build_page_header(*n, body.len() as i32), body.to_vec())
                }
            };
            page_bytes.extend_from_slice(&header);
            page_bytes.extend_from_slice(&body);
        }

        let mut meta = StructWriter::new();
        meta.i32_field(1, physical_type);
        meta.field_header(3, 0x09); // path_in_schema list<string>
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i64_field(5, num_rows); // num_values
        meta.i64_field(6, page_bytes.len() as i64); // total_uncompressed_size
        meta.i64_field(7, page_bytes.len() as i64); // total_compressed_size
        meta.i64_field(9, 4); // data_page_offset
        let meta_bytes = meta.finish();
        build_file_from_chunk_typed(&page_bytes, meta_bytes, num_rows, physical_type, None)
    }

    fn le_bytes<const N: usize>(values: impl IntoIterator<Item = [u8; N]>) -> Vec<u8> {
        values.into_iter().flatten().collect()
    }

    #[test]
    fn read_dictionary_fixed_column_gathers_pages_and_a_plain_fallback_like_the_option_reader() {
        // #753: two dictionary pages, then a PLAIN page after the writer
        // gave up on the dictionary.
        let dict = le_bytes([10i64, 20, 30].map(i64::to_le_bytes));
        let plain = le_bytes([7i64, 8].map(i64::to_le_bytes));
        let file_bytes = build_fixed_dictionary_chunk(
            2,
            &dict,
            3,
            &[
                DictTestPage::Codes(&[2, 0, 1]),
                DictTestPage::Codes(&[1, 1]),
                DictTestPage::Plain(&plain, 2),
            ],
        );
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let (values, present) = rg
            .read_dictionary_fixed_column(0, i64::from_le_bytes)
            .unwrap()
            .expect("dictionary chunk");
        assert_eq!(values, vec![30, 10, 20, 20, 20, 7, 8]);
        assert!(present.is_empty(), "REQUIRED column: no per-row flags");
        assert_eq!(
            values.iter().copied().map(Some).collect::<Vec<_>>(),
            rg.read_int64_column(0).unwrap()
        );
    }

    #[test]
    fn read_dictionary_fixed_column_widens_the_dictionary_through_decode() {
        // INT32 and FLOAT entries widen once, in the dictionary.
        let dict = le_bytes([-5i32, 9].map(i32::to_le_bytes));
        let file_bytes =
            build_fixed_dictionary_chunk(1, &dict, 2, &[DictTestPage::Codes(&[1, 0, 1])]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let (values, _) = rg
            .read_dictionary_fixed_column(0, |b| i64::from(i32::from_le_bytes(b)))
            .unwrap()
            .unwrap();
        assert_eq!(values, vec![9, -5, 9]);
        assert_eq!(
            rg.read_int32_column(0).unwrap(),
            vec![Some(9), Some(-5), Some(9)]
        );

        let dict = le_bytes([1.5f32, -0.25].map(f32::to_le_bytes));
        let file_bytes = build_fixed_dictionary_chunk(4, &dict, 2, &[DictTestPage::Codes(&[0, 1])]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let (values, _) = rg
            .read_dictionary_fixed_column(0, |b| f64::from(f32::from_le_bytes(b)))
            .unwrap()
            .unwrap();
        assert_eq!(values, vec![1.5, -0.25]);
        assert_eq!(
            rg.read_float_column(0).unwrap(),
            vec![Some(1.5), Some(-0.25)]
        );

        let dict = le_bytes([2.5f64, 1e300].map(f64::to_le_bytes));
        let file_bytes =
            build_fixed_dictionary_chunk(5, &dict, 2, &[DictTestPage::Codes(&[1, 1, 0])]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let (values, _) = rg
            .read_dictionary_fixed_column(0, f64::from_le_bytes)
            .unwrap()
            .unwrap();
        assert_eq!(
            values.iter().copied().map(Some).collect::<Vec<_>>(),
            rg.read_double_column(0).unwrap()
        );
    }

    #[test]
    fn read_dictionary_fixed_column_rejects_a_code_past_the_dictionary() {
        let dict = le_bytes([10i64, 20, 30].map(i64::to_le_bytes));
        let file_bytes = build_fixed_dictionary_chunk(2, &dict, 3, &[DictTestPage::Codes(&[0, 3])]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let err = rg
            .read_dictionary_fixed_column(0, i64::from_le_bytes)
            .unwrap_err();
        assert!(
            matches!(
                err,
                FileError::Read(ReadError::DictionaryIndexOutOfRange(3))
            ),
            "{err:?}"
        );
    }

    #[test]
    fn read_dictionary_fixed_column_declines_a_chunk_without_a_dictionary() {
        let file = three_page_int64_file();
        let parsed = ParquetFile::open(&file).unwrap();
        let rg = parsed.row_group(0).unwrap();
        assert!(rg
            .read_dictionary_fixed_column(0, i64::from_le_bytes)
            .unwrap()
            .is_none());
    }

    #[test]
    fn column_index_out_of_range_errors() {
        let file_bytes = build_file(&[&[1]]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let result = rg.read_int64_column(5);
        assert!(matches!(result, Err(FileError::ColumnIndexOutOfRange(5))));
    }

    /// Assembles a one-row-group, one-column INT64 file from already-built
    /// column-chunk page bytes + `ColumnMetaData`, for the
    /// `read_column_dictionary_indices` MC/DC vectors below (which need
    /// direct control over what page types appear in the chunk, unlike
    /// `build_file`'s always-PLAIN chunks).
    fn build_file_from_chunk(page_bytes: &[u8], meta_bytes: Vec<u8>, num_rows: i64) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(b"PAR1");
        file.extend_from_slice(page_bytes);

        let column_chunk = build_column_chunk(4, meta_bytes);
        let row_group = build_row_group(vec![column_chunk], page_bytes.len() as i64, num_rows);
        let root = build_root_schema_element(1);
        let col = build_schema_element("v", 2, 0); // REQUIRED -> max_def_level 0
        let metadata = build_file_metadata(vec![root, col], num_rows, vec![row_group]);
        file.extend_from_slice(&metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(b"PAR1");
        file
    }

    /// A REQUIRED INT64 dictionary chunk of `[10, 20, 30]` whose one data
    /// page claims `num_values` and holds `data_body`; its
    /// `total_uncompressed_size` is its page bytes (#671's crafted files).
    fn dictionary_chunk_claiming(num_values: i32, data_body: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut dict_body = Vec::new();
        for v in [10i64, 20, 30] {
            dict_body.extend_from_slice(&v.to_le_bytes());
        }
        let mut page_bytes = build_dictionary_page_header(3, dict_body.len() as i32);
        page_bytes.extend_from_slice(&dict_body);
        page_bytes.extend_from_slice(&build_dictionary_encoded_data_page_header(
            num_values,
            data_body.len() as i32,
        ));
        page_bytes.extend_from_slice(data_body);
        let mut meta = StructWriter::new();
        meta.i32_field(1, 2); // INT64
        meta.field_header(3, 0x09);
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i64_field(5, 3);
        meta.i64_field(6, page_bytes.len() as i64);
        meta.i64_field(7, page_bytes.len() as i64);
        meta.i64_field(9, 4);
        (page_bytes, meta.finish())
    }

    /// The error reading leaf 0 of a 3-row file built from `chunk`.
    fn three_row_error((page_bytes, meta_bytes): (Vec<u8>, Vec<u8>)) -> FileError {
        let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, 3);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        rg.read_int64_column(0).unwrap_err()
    }

    #[test]
    fn an_rle_run_claiming_more_values_than_rows_is_refused_before_decoding() {
        // Bit width 2, then one RLE run of i32::MAX copies of index 1: a
        // 7-byte body that used to expand to ~8 GiB of indices.
        let mut body = vec![2u8];
        let mut run = (i32::MAX as u64) << 1;
        while run >= 0x80 {
            body.push((run as u8) | 0x80);
            run >>= 7;
        }
        body.push(run as u8);
        body.push(1);
        let err = three_row_error(dictionary_chunk_claiming(i32::MAX, &body));
        assert!(
            matches!(
                err,
                FileError::PageExceedsChunk {
                    field: "num_values",
                    declared: 2_147_483_647,
                    left: 3,
                }
            ),
            "{err}"
        );
        assert_eq!(
            err.to_string(),
            "page num_values 2147483647 is more than its column chunk has left (3)"
        );
    }

    #[test]
    fn a_zero_bit_width_page_claiming_more_values_than_rows_is_refused() {
        // Bit width 0 needs no data at all: one byte claims every value.
        let err = three_row_error(dictionary_chunk_claiming(1 << 30, &[0u8]));
        assert!(
            matches!(
                err,
                FileError::PageExceedsChunk {
                    field: "num_values",
                    left: 3,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn pages_together_claiming_more_values_than_rows_are_refused() {
        // Two pages of two values each in a 3-row group: the second is
        // refused with one value left.
        let (page_bytes, meta_bytes) = build_int64_chunk_multi_page(&[&[1, 2], &[3, 4]], 4);
        let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, 3);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let err = file.row_group(0).unwrap().read_int64_column(0).unwrap_err();
        assert!(
            matches!(
                err,
                FileError::PageExceedsChunk {
                    field: "num_values",
                    declared: 2,
                    left: 1,
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_zstd_page_larger_than_its_chunk_is_refused_before_decompressing() {
        // A data page declaring 1 GiB uncompressed in a chunk whose
        // metadata says it holds a few dozen bytes.
        let body = [0x28u8, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0];
        let mut dph = StructWriter::new();
        dph.i32_field(1, 3);
        dph.i32_field(2, 0);
        dph.i32_field(3, 3);
        dph.i32_field(4, 3);
        let mut header = StructWriter::new();
        header.i32_field(1, 0); // DATA_PAGE
        header.i32_field(2, 1 << 30); // uncompressed_page_size
        header.i32_field(3, body.len() as i32); // compressed_page_size
        header.struct_field(5, dph.finish());
        let mut page_bytes = header.finish();
        page_bytes.extend_from_slice(&body);
        let mut meta = StructWriter::new();
        meta.i32_field(1, 2); // INT64
        meta.field_header(3, 0x09);
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i32_field(4, 6); // codec = ZSTD
        meta.i64_field(5, 3);
        meta.i64_field(6, page_bytes.len() as i64);
        meta.i64_field(7, page_bytes.len() as i64);
        meta.i64_field(9, 4);
        let err = three_row_error((page_bytes, meta.finish()));
        assert!(
            matches!(
                err,
                FileError::PageExceedsChunk {
                    field: "uncompressed_page_size",
                    declared: 1_073_741_824,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn negative_chunk_budgets_are_invalid_metadata() {
        let (page_bytes, meta_bytes) = build_int64_chunk(&[1, 2, 3], 4);
        let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, -1);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let err = file.row_group(0).unwrap().read_int64_column(0).unwrap_err();
        assert!(
            matches!(
                err,
                FileError::InvalidMetadata {
                    field: "row_group.num_rows",
                    value: -1,
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn plain_pages_after_a_string_dictionary_extend_it_rather_than_fall_back() {
        // A dictionary of ["east", "west"], a dictionary page of indices
        // [1, 0], then a writer's PLAIN fallback page ["north", "east"].
        let plain = |values: &[&str]| {
            let mut body = Vec::new();
            for v in values {
                body.extend_from_slice(&(v.len() as u32).to_le_bytes());
                body.extend_from_slice(v.as_bytes());
            }
            body
        };
        let dict_body = plain(&["east", "west"]);
        let mut page_bytes = build_dictionary_page_header(2, dict_body.len() as i32);
        page_bytes.extend_from_slice(&dict_body);
        let indices = encode_dictionary_indices(&[1, 0], 1);
        page_bytes.extend_from_slice(&build_dictionary_encoded_data_page_header(
            2,
            indices.len() as i32,
        ));
        page_bytes.extend_from_slice(&indices);
        let fallback = plain(&["north", "east"]);
        page_bytes.extend_from_slice(&build_page_header(2, fallback.len() as i32));
        page_bytes.extend_from_slice(&fallback);
        let mut meta = StructWriter::new();
        meta.i32_field(1, 6); // BYTE_ARRAY
        meta.field_header(3, 0x09);
        meta.buf.push((1u8 << 4) | 0x08);
        meta.write_varint(1);
        meta.buf.push(b'v');
        meta.i64_field(5, 4);
        meta.i64_field(6, page_bytes.len() as i64);
        meta.i64_field(7, page_bytes.len() as i64);
        meta.i64_field(9, 4);
        let file_bytes = build_file_from_chunk_typed(&page_bytes, meta.finish(), 4, 6, None);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let (dict, codes) = rg
            .read_string_column_dictionary_indices(0)
            .unwrap()
            .unwrap();
        assert_eq!(dict, ["east", "west", "north", "east"]);
        assert_eq!(codes, [Some(1), Some(0), Some(2), Some(3)]);
        let resolved: Vec<Option<String>> = codes
            .iter()
            .map(|c| c.map(|i| dict[i as usize].clone()))
            .collect();
        assert_eq!(resolved, rg.read_string_column(0).unwrap());
        // #699: gathered at positions, only the used dictionary and
        // fallback entries remain, renumbered in their original order.
        let (dict, codes) = rg
            .read_string_column_dictionary_at(0, &[3, 0, 2])
            .unwrap()
            .unwrap();
        assert_eq!(dict, ["west", "north", "east"]);
        assert_eq!(codes, [Some(2), Some(0), Some(1)]);
        let (dict, codes) = rg
            .read_string_column_dictionary_at(0, &[1])
            .unwrap()
            .unwrap();
        assert_eq!(dict, ["east"]);
        assert_eq!(codes, [Some(0)]);
        // Other types keep falling back: their callers have no plain path.
        assert_eq!(
            rg.read_int64_column_dictionary_indices(0).ok().flatten(),
            None
        );
    }

    #[test]
    fn map_strings_from_a_dictionary_are_shared_across_rows() {
        // #671: one long header value in every row's map is one allocation,
        // not a copy per row.
        use crate::storage::parquet::schema::{ColumnSpec, ColumnType, ColumnValues};
        use crate::storage::parquet::writer::{ParquetWriter, WriterOptions};
        let long: Arc<str> = "v".repeat(10_000).into();
        let rows: Vec<Option<MapEntries>> = (0..200)
            .map(|i| {
                Some(vec![
                    ("k".into(), Some(Arc::clone(&long))),
                    (format!("n{}", i % 2).into(), None),
                ])
            })
            .collect();
        let mut w = ParquetWriter::new(
            Vec::new(),
            vec![ColumnSpec::new("h", ColumnType::MapUtf8, true)],
            WriterOptions::default(),
        )
        .unwrap();
        w.write_columns(vec![ColumnValues::Map(rows.clone())])
            .unwrap();
        let (bytes, _) = w.finish().unwrap();
        let file = ParquetFile::open(&bytes).unwrap();
        let field = file.fields().into_iter().find(|f| f.name == "h").unwrap();
        let read = file
            .row_group(0)
            .unwrap()
            .read_map_utf8_column(&field)
            .unwrap();
        assert_eq!(read, rows);
        let value = |row: &Option<MapEntries>| row.as_ref().unwrap()[0].1.clone().unwrap();
        let first = value(&read[0]);
        assert!(read.iter().all(|row| Arc::ptr_eq(&value(row), &first)));
    }

    #[allow(non_snake_case)]
    mod mcdc_vectors {
        //! Tagged MC/DC vectors for this file's multi-leaf decisions
        //! (`mcdc__<id>__vN`, joined to
        //! `tests/mcdc/obligations.json` by `make test-mcdc`; db-core#299
        //! follow-up).

        use super::*;

        // `read_column_dictionary_indices` (no longer a multi-leaf decision
        // since #671; kept as behaviour tests).
        #[test]
        fn dictionary_indices_of_a_dictionary_chunk_return_the_dictionary() {
            let (page_bytes, meta_bytes) =
                build_int64_dictionary_chunk(&[10, 20, 30], &[2, 0, 1], 4);
            let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, 3);
            let file = ParquetFile::open(&file_bytes).unwrap();
            let rg = file.row_group(0).unwrap();

            let result = rg.read_int64_column_dictionary_indices(0).unwrap();
            let (dict, indices) = result.expect("dictionary-encoded chunk must return Some");
            assert_eq!(dict, vec![10, 20, 30]);
            assert_eq!(indices, vec![Some(2), Some(0), Some(1)]);
        }

        #[test]
        fn dictionary_indices_of_a_plain_chunk_are_none() {
            // A plain (non-dictionary) chunk: the first Data page arrives
            // with `dictionary` still `None`, so the `||`'s first leaf
            // alone must short-circuit to `None` regardless of encoding.
            let (page_bytes, meta_bytes) = build_int64_chunk(&[1, 2, 3], 4);
            let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, 3);
            let file = ParquetFile::open(&file_bytes).unwrap();
            let rg = file.row_group(0).unwrap();

            let result = rg.read_int64_column_dictionary_indices(0).unwrap();
            assert!(result.is_none());
        }

        #[test]
        fn dictionary_indices_after_a_plain_fallback_without_a_plain_decoder_are_none() {
            // A dictionary page followed by a *PLAIN* (not
            // PLAIN_DICTIONARY) data page: `dictionary.is_none()` is
            // false, but the encoding check is true, so the `||` must
            // still short-circuit to `None`.
            let file_bytes = dictionary_then_plain_page_file();
            let file = ParquetFile::open(&file_bytes).unwrap();
            let rg = file.row_group(0).unwrap();

            let result = rg.read_int64_column_dictionary_indices(0).unwrap();
            assert!(result.is_none());
        }

        /// A one-column INT64 file whose chunk is a dictionary page then a
        /// PLAIN data page: a dictionary fallback partway through.
        fn dictionary_then_plain_page_file() -> Vec<u8> {
            let mut dict_body = Vec::new();
            for v in [10i64, 20, 30] {
                dict_body.extend_from_slice(&v.to_le_bytes());
            }
            let mut page_bytes = build_dictionary_page_header(3, dict_body.len() as i32);
            page_bytes.extend_from_slice(&dict_body);

            let mut value_body = Vec::new();
            for v in [10i64, 20, 30] {
                value_body.extend_from_slice(&v.to_le_bytes());
            }
            page_bytes.extend_from_slice(&build_page_header(3, value_body.len() as i32));
            page_bytes.extend_from_slice(&value_body);

            let mut meta = StructWriter::new();
            meta.i32_field(1, 2); // INT64
            meta.field_header(3, 0x09);
            meta.buf.push((1u8 << 4) | 0x08);
            meta.write_varint(1);
            meta.buf.push(b'v');
            meta.i64_field(5, 3);
            meta.i64_field(6, page_bytes.len() as i64);
            meta.i64_field(7, page_bytes.len() as i64);
            meta.i64_field(9, 4);
            let meta_bytes = meta.finish();

            build_file_from_chunk(&page_bytes, meta_bytes, 3)
        }

        // storage_parquet_parquet_file_column_dictionary_1b5585c0: the same
        // decision in `column_dictionary` (#589), which skips data bodies.
        #[test]
        fn mcdc__storage_parquet_parquet_file_column_dictionary_1b5585c0__v1_both_false_returns_the_dictionary(
        ) {
            let (page_bytes, meta_bytes) =
                build_int64_dictionary_chunk(&[10, 20, 30], &[2, 0, 1], 4);
            let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, 3);
            let file = ParquetFile::open(&file_bytes).unwrap();
            let rg = file.row_group(0).unwrap();
            assert_eq!(
                rg.read_int64_column_dictionary(0).unwrap(),
                Some(vec![10, 20, 30])
            );
        }

        #[test]
        fn mcdc__storage_parquet_parquet_file_column_dictionary_1b5585c0__v2_no_dictionary_page_returns_none(
        ) {
            let (page_bytes, meta_bytes) = build_int64_chunk(&[1, 2, 3], 4);
            let file_bytes = build_file_from_chunk(&page_bytes, meta_bytes, 3);
            let file = ParquetFile::open(&file_bytes).unwrap();
            let rg = file.row_group(0).unwrap();
            assert_eq!(rg.read_int64_column_dictionary(0).unwrap(), None);
        }

        #[test]
        fn mcdc__storage_parquet_parquet_file_column_dictionary_1b5585c0__v3_dictionary_present_but_plain_data_page_returns_none(
        ) {
            let file_bytes = dictionary_then_plain_page_file();
            let file = ParquetFile::open(&file_bytes).unwrap();
            let rg = file.row_group(0).unwrap();
            assert_eq!(rg.read_int64_column_dictionary(0).unwrap(), None);
        }
    }

    // Physical type ids (thrift `Type` enum): BOOLEAN=0, INT32=1, INT64=2,
    // INT96=3, FLOAT=4, DOUBLE=5, BYTE_ARRAY=6, FIXED_LEN_BYTE_ARRAY=7.
    //
    // `RowGroupReader::read_{int32,float,boolean,string,fixed_len_byte_array,
    // int96}_column` (and `read_timestamp_column`/`read_decimal_column`,
    // which delegate to these) are otherwise only covered indirectly by
    // `reader::read_*_column`'s own byte-level unit tests -- never through a
    // full `ParquetFile::open` -> `RowGroupReader` round trip.
    #[test]
    fn reads_an_int32_column_through_a_full_file_round_trip() {
        let body: Vec<u8> = [7i32, -3, 100]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let (page_bytes, meta_bytes) = build_typed_chunk(1, &body, 3, 4);
        let file_bytes = build_file_from_chunk_typed(&page_bytes, meta_bytes, 3, 1, None);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        assert_eq!(
            rg.read_int32_column(0).unwrap(),
            vec![Some(7), Some(-3), Some(100)]
        );
    }

    #[test]
    fn reads_a_float_column_through_a_full_file_round_trip() {
        let body: Vec<u8> = [1.5f32, -2.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let (page_bytes, meta_bytes) = build_typed_chunk(4, &body, 2, 4);
        let file_bytes = build_file_from_chunk_typed(&page_bytes, meta_bytes, 2, 4, None);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        assert_eq!(
            rg.read_float_column(0).unwrap(),
            vec![Some(1.5), Some(-2.5)]
        );
    }

    #[test]
    fn reads_a_boolean_column_through_a_full_file_round_trip() {
        // LSB-first bit-packing: true, false, true, true -> 0b0000_1101.
        let body = vec![0b0000_1101u8];
        let (page_bytes, meta_bytes) = build_typed_chunk(0, &body, 4, 4);
        let file_bytes = build_file_from_chunk_typed(&page_bytes, meta_bytes, 4, 0, None);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        assert_eq!(
            rg.read_boolean_column(0).unwrap(),
            vec![Some(true), Some(false), Some(true), Some(true)]
        );
    }

    #[test]
    fn reads_a_string_column_through_a_full_file_round_trip() {
        let mut body = Vec::new();
        for s in ["hi", "parquet"] {
            body.extend_from_slice(&(s.len() as u32).to_le_bytes());
            body.extend_from_slice(s.as_bytes());
        }
        let (page_bytes, meta_bytes) = build_typed_chunk(6, &body, 2, 4);
        let file_bytes = build_file_from_chunk_typed(&page_bytes, meta_bytes, 2, 6, None);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        assert_eq!(
            rg.read_string_column(0).unwrap(),
            vec![Some("hi".to_string()), Some("parquet".to_string())]
        );
    }

    #[test]
    fn reads_a_fixed_len_byte_array_column_through_a_full_file_round_trip() {
        let body: Vec<u8> = vec![1, 2, 3, 4, 5, 6]; // two 3-byte values
        let (page_bytes, meta_bytes) = build_typed_chunk(7, &body, 2, 4);
        let file_bytes = build_file_from_chunk_typed(&page_bytes, meta_bytes, 2, 7, Some(3));
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        assert_eq!(
            rg.read_fixed_len_byte_array_column(0).unwrap(),
            vec![Some(vec![1, 2, 3]), Some(vec![4, 5, 6])]
        );
    }

    #[test]
    fn reads_an_int96_column_and_a_timestamp_column_through_a_full_file_round_trip() {
        // INT96 = 8-byte time-of-day nanos (LE) + 4-byte Julian day (LE).
        // Julian day for the Unix epoch is 2_440_588; time_nanos 0 -> epoch.
        let mut body = Vec::new();
        body.extend_from_slice(&0i64.to_le_bytes());
        body.extend_from_slice(&2_440_588i32.to_le_bytes());
        let (page_bytes, meta_bytes) = build_typed_chunk(3, &body, 1, 4);
        let file_bytes = build_file_from_chunk_typed(&page_bytes, meta_bytes, 1, 3, None);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let int96 = rg.read_int96_column(0).unwrap();
        assert_eq!(int96.len(), 1);
        assert_eq!(int96[0].as_ref().unwrap().julian_day, 2_440_588);

        let ts = rg.read_timestamp_column(0).unwrap();
        assert_eq!(ts, vec![Some(0)]);
    }

    #[test]
    fn reads_a_decimal_column_through_a_full_file_round_trip() {
        let body: Vec<u8> = 12345i32.to_le_bytes().to_vec();
        let (page_bytes, meta_bytes) = build_typed_chunk(1, &body, 1, 4);
        let file_bytes = build_file_from_chunk_typed(&page_bytes, meta_bytes, 1, 1, None);
        let file = ParquetFile::open(&file_bytes).unwrap();
        // `read_decimal_column` needs a DECIMAL-annotated schema (scale in
        // the leaf's `SchemaElement`), which `build_file_from_chunk_typed`
        // doesn't set -- so the un-annotated call is the error path this
        // test actually exercises: `FileMetaData::decimal_scale` fails with
        // `MissingDecimalScale` before `read_int32_column` is ever reached.
        let rg = file.row_group(0).unwrap();
        assert!(matches!(
            rg.read_decimal_column(0),
            Err(FileError::MissingDecimalScale)
        ));
    }

    /// `read_nested_column` on a flat (non-nested) schema is otherwise
    /// unreached by every other test in this file, which all go through
    /// the per-type `read_*_column` fast paths instead -- and it's the
    /// only caller of `read_leaf_entries`, so this is also the sole
    /// coverage for that function's PLAIN-encoded, non-dictionary,
    /// non-repeated (`rep_levels`/`def_levels` both empty) path.
    #[test]
    fn read_nested_column_reconstructs_a_flat_int64_column() {
        let file_bytes = build_file(&[&[10, 20, 30]]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let values = file.read_nested_column(0, "v").unwrap();
        assert_eq!(
            values,
            vec![
                NestedValue::Scalar(reader::LeafScalar::Int64(10)),
                NestedValue::Scalar(reader::LeafScalar::Int64(20)),
                NestedValue::Scalar(reader::LeafScalar::Int64(30)),
            ]
        );
    }

    #[test]
    fn read_nested_column_on_an_unknown_field_name_errors() {
        let file_bytes = build_file(&[&[1]]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        assert!(matches!(
            file.read_nested_column(0, "nope"),
            Err(FileError::ColumnIndexOutOfRange(0))
        ));
    }

    #[test]
    fn read_nested_column_on_an_out_of_range_row_group_errors() {
        let file_bytes = build_file(&[&[1]]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        assert!(matches!(
            file.read_nested_column(5, "v"),
            Err(FileError::ColumnIndexOutOfRange(5))
        ));
    }

    #[test]
    fn read_timestamp_column_converts_millis_to_micros() {
        let body: Vec<u8> = 42i64.to_le_bytes().to_vec();
        let (page_bytes, meta_bytes) = build_typed_chunk(2, &body, 1, 4);
        // ConvertedType::TimestampMillis == 9.
        let file_bytes =
            build_file_from_chunk_annotated(&page_bytes, meta_bytes, 1, 2, None, 9, None);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        assert_eq!(rg.read_timestamp_column(0).unwrap(), vec![Some(42_000)]);
    }

    #[test]
    fn read_decimal_column_reads_int32_int64_and_fixed_len_byte_array() {
        // ConvertedType::Decimal == 5.
        let int32_body: Vec<u8> = 123i32.to_le_bytes().to_vec();
        let (page_bytes, meta_bytes) = build_typed_chunk(1, &int32_body, 1, 4);
        let file_bytes =
            build_file_from_chunk_annotated(&page_bytes, meta_bytes, 1, 1, None, 5, Some(2));
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let d = rg.read_decimal_column(0).unwrap();
        assert_eq!(d[0].as_ref().unwrap().unscaled, 123);
        assert_eq!(d[0].as_ref().unwrap().scale, 2);

        let int64_body: Vec<u8> = 456i64.to_le_bytes().to_vec();
        let (page_bytes, meta_bytes) = build_typed_chunk(2, &int64_body, 1, 4);
        let file_bytes =
            build_file_from_chunk_annotated(&page_bytes, meta_bytes, 1, 2, None, 5, Some(3));
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let d = rg.read_decimal_column(0).unwrap();
        assert_eq!(d[0].as_ref().unwrap().unscaled, 456);
        assert_eq!(d[0].as_ref().unwrap().scale, 3);

        let flba_body: Vec<u8> = vec![0, 0, 1, 0]; // big-endian 256
        let (page_bytes, meta_bytes) = build_typed_chunk(7, &flba_body, 1, 4);
        let file_bytes =
            build_file_from_chunk_annotated(&page_bytes, meta_bytes, 1, 7, Some(4), 5, Some(1));
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let d = rg.read_decimal_column(0).unwrap();
        assert_eq!(d[0].as_ref().unwrap().unscaled, 256);
        assert_eq!(d[0].as_ref().unwrap().scale, 1);
    }

    /// #632: a DECIMAL wider than 16 bytes would lose its high bits in an
    /// `i128`, so it is refused rather than silently truncated.
    #[test]
    fn read_decimal_column_refuses_a_fixed_len_byte_array_past_16_bytes() {
        let flba_body = vec![0_u8; 17];
        let (page_bytes, meta_bytes) = build_typed_chunk(7, &flba_body, 1, 4);
        let file_bytes =
            build_file_from_chunk_annotated(&page_bytes, meta_bytes, 1, 7, Some(17), 5, Some(0));
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        match rg.read_decimal_column(0) {
            Err(FileError::InvalidMetadata { field, value }) => {
                assert_eq!(field, "DECIMAL fixed_len_byte_array length");
                assert_eq!(value, 17);
            }
            other => panic!("expected InvalidMetadata, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests_289 {
    use super::*;

    /// #289: a negative page size in a (corrupt or hostile) page header is a
    /// typed error, never a wrapped-around allocation.
    #[test]
    fn negative_page_size_is_invalid_metadata_not_a_huge_allocation() {
        assert_eq!(page_size("compressed_page_size", 4096).unwrap(), 4096);
        assert_eq!(page_size("compressed_page_size", 0).unwrap(), 0);
        match page_size("compressed_page_size", -1) {
            Err(FileError::InvalidMetadata { field, value }) => {
                assert_eq!(field, "compressed_page_size");
                assert_eq!(value, -1);
            }
            other => panic!("expected InvalidMetadata, got {other:?}"),
        }
        assert!(FileError::InvalidMetadata {
            field: "x",
            value: -5
        }
        .to_string()
        .contains("-5"));
    }

    /// Every `FileError` variant's `Display` renders without panicking and
    /// names something specific enough to be useful in an error message --
    /// most variants are only ever constructed deep in a page/column-chunk
    /// parse path that a well-formed test fixture never reaches, so this is
    /// the only place they get exercised at all.
    #[test]
    fn every_file_error_variant_displays_a_useful_message() {
        let cases: Vec<(FileError, &str)> = vec![
            (FileError::Footer(FooterError::FileTooShort), "footer"),
            (
                FileError::Page(PageError::MissingDataPageHeader),
                "DATA_PAGE",
            ),
            (FileError::Read(ReadError::UnexpectedEof), "end of page"),
            (
                FileError::Compression(CompressionError::UnsupportedCodec(99)),
                "99",
            ),
            (FileError::ColumnIndexOutOfRange(7), "7"),
            (FileError::ChunkOutOfBounds, "outside the file"),
            (FileError::MissingColumnMetadata, "meta_data"),
            (FileError::UnexpectedDictionaryPage, "DICTIONARY_PAGE"),
            (FileError::MissingDictionaryPage, "DICTIONARY_PAGE"),
            (FileError::MissingTypeLength, "type_length"),
            (FileError::MissingDecimalScale, "DECIMAL"),
            (FileError::TimestampOutOfRange, "timestamp"),
            (
                FileError::UnsupportedTimestampPhysicalType(footer::PhysicalType::Boolean),
                "timestamp",
            ),
            (
                FileError::UnsupportedDecimalPhysicalType(footer::PhysicalType::ByteArray),
                "DECIMAL",
            ),
            (FileError::UnsupportedNestedDictionary, "dictionary-encoded"),
            (
                FileError::UnsupportedNestedEncoding(Encoding::Plain),
                "encoding",
            ),
            (
                FileError::Nested(NestedError::MissingLeafData("v".to_string())),
                "'v'",
            ),
        ];
        for (err, needle) in cases {
            let msg = err.to_string();
            assert!(
                msg.contains(needle),
                "{err:?} displayed as {msg:?}, expected it to contain {needle:?}"
            );
        }
    }

    /// `ParquetFile::open`'s `?` on `footer::parse_footer` is the only
    /// place `From<FooterError> for FileError` actually fires -- every
    /// other test opens a well-formed file, so a too-short buffer is the
    /// only way to reach it.
    #[test]
    fn open_on_a_too_short_buffer_wraps_the_footer_error() {
        assert!(matches!(
            ParquetFile::open(&[]),
            Err(FileError::Footer(FooterError::FileTooShort))
        ));
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test code fails fast (db-core#230)"
)]
mod tests_657 {
    use super::*;

    /// A data page header: `DATA_PAGE`, both sizes `size`, one value.
    fn page_header_bytes(size: u8) -> Vec<u8> {
        // Thrift compact: field 1 i32 0, field 2 i32 size, field 3 i32
        // size, field 5 struct { field 1 i32 1 }, stop. Sizes are zigzag
        // varints, so `size` must stay below 64.
        vec![
            0x15,
            0x00,
            0x15,
            size * 2,
            0x15,
            size * 2,
            0x2c,
            0x15,
            0x02,
            0x00,
            0x00,
        ]
    }

    /// #657: a page header past the chunk end, or a page body running past
    /// it, is `ChunkOutOfBounds`, never a slice panic.
    #[test]
    fn next_page_bounds_every_offset_by_the_chunk() {
        let mut chunk = page_header_bytes(4);
        chunk.extend_from_slice(&[1, 2, 3, 4]);
        let (header, body, end) = next_page(&chunk, 0).unwrap();
        assert_eq!(header.compressed_page_size, 4);
        assert_eq!(body, [1, 2, 3, 4]);
        assert_eq!(end, chunk.len());

        assert!(matches!(
            next_page(&chunk, chunk.len() + 1),
            Err(FileError::ChunkOutOfBounds)
        ));
        chunk.pop();
        assert!(matches!(
            next_page(&chunk, 0),
            Err(FileError::ChunkOutOfBounds)
        ));
    }

    /// #657: an INT96 or `TIMESTAMP_MILLIS` value whose microseconds do not
    /// fit `i64` was an arithmetic-overflow panic (debug) or a silently
    /// wrapped timestamp (release); now `None`, i.e. `TimestampOutOfRange`.
    #[test]
    fn int96_overflow_is_none_not_a_panic() {
        let epoch = reader::Int96 {
            julian_day: 2_440_588,
            time_nanos: 1_000,
        };
        assert_eq!(int96_to_epoch_micros(epoch), Some(1));
        let far = reader::Int96 {
            julian_day: i32::MAX,
            time_nanos: 0,
        };
        assert_eq!(int96_to_epoch_micros(far), None);
        let before = reader::Int96 {
            julian_day: i32::MIN,
            time_nanos: i64::MIN,
        };
        assert_eq!(int96_to_epoch_micros(before), None);
    }

    /// #657: the flat check reads the leaves with `get(1..)`, not a slice
    /// index; an empty schema (no root) has no leaves and is flat.
    #[test]
    fn is_flat_schema_on_an_empty_schema() {
        assert!(is_flat_schema(&[]));
    }
}
