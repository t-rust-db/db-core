//! Top-level Parquet file reader: parses the footer once, then exposes a
//! lazy iterator over row groups and their columns.

use crate::column::parquet::compression::{self, CompressionError};
use crate::column::parquet::footer::{self, FileMetaData, FooterError, Repetition, RowGroup};
use crate::column::parquet::nested::{self, LeafData, LeafEntries, NestedError, NestedValue};
use crate::column::parquet::page::{self, Encoding, PageError, PageType};
use crate::column::parquet::reader::{self, ReadError};
use crate::column::parquet::schema_tree::{self, SchemaNode};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug)]
pub enum FileError {
    Footer(FooterError),
    Page(PageError),
    Read(ReadError),
    Compression(CompressionError),
    ColumnIndexOutOfRange(usize),
    ChunkOutOfBounds,
    MissingColumnMetadata,
    UnexpectedDictionaryPage,
    MissingDictionaryPage,
    MissingTypeLength,
    MissingDecimalScale,
    UnsupportedTimestampPhysicalType(footer::PhysicalType),
    UnsupportedDecimalPhysicalType(footer::PhysicalType),
    UnsupportedNestedDictionary,
    UnsupportedNestedEncoding(page::Encoding),
    Nested(NestedError),
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::Footer(e) => write!(f, "{e}"),
            FileError::Page(e) => write!(f, "{e}"),
            FileError::Read(e) => write!(f, "{e}"),
            FileError::Compression(e) => write!(f, "{e}"),
            FileError::ColumnIndexOutOfRange(i) => write!(f, "column index {i} out of range"),
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
                "FIXED_LEN_BYTE_ARRAY column has no type_length in its schema"
            ),
            FileError::MissingDecimalScale => {
                write!(f, "DECIMAL column has no scale in its schema")
            }
            FileError::UnsupportedTimestampPhysicalType(t) => {
                write!(f, "unsupported physical type for a timestamp column: {t:?}")
            }
            FileError::UnsupportedDecimalPhysicalType(t) => {
                write!(f, "unsupported physical type for a DECIMAL column: {t:?}")
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

pub type Result<T> = std::result::Result<T, FileError>;

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
fn is_flat_schema(schema: &[footer::SchemaElement]) -> bool {
    let Some(root) = schema.first() else {
        return true;
    };
    let expected_leaves = root.num_children.unwrap_or(0) as usize;
    schema.len() == expected_leaves + 1
        && schema[1..]
            .iter()
            .all(|s| s.physical_type.is_some() && s.repetition != Some(Repetition::Repeated))
}

/// An open Parquet file: the parsed footer plus a borrowed view of the raw
/// file bytes. Get the bytes via `crate::column::vfs::Vfs::open(..)?.mmap()?`
/// (which derefs to `&[u8]`), or any other `&[u8]` source.
pub struct ParquetFile<'a> {
    metadata: FileMetaData,
    data: &'a [u8],
    schema_tree: SchemaNode,
    is_flat: bool,
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
        Ok(ParquetFile {
            metadata,
            data,
            schema_tree,
            is_flat,
        })
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
        let paths = schema_tree::leaf_paths(&self.schema_tree);
        for (column_index, (leaf, path)) in leaves.iter().zip(paths.iter()).enumerate() {
            let entries = row_group.read_leaf_entries(column_index, leaf)?;
            leaf_data.insert(path.clone(), entries);
        }

        let num_rows = row_group.num_rows() as usize;
        let mut columns = nested::reconstruct_row_group(&self.schema_tree, num_rows, &leaf_data)?;
        let idx = columns
            .iter()
            .position(|(name, _)| name == field_name)
            .ok_or(FileError::ColumnIndexOutOfRange(row_group_index))?;
        Ok(columns.remove(idx).1)
    }

    pub fn metadata(&self) -> &FileMetaData {
        &self.metadata
    }

    pub fn num_rows(&self) -> i64 {
        self.metadata.num_rows
    }

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

    /// Max definition level for a leaf column, derived from its schema
    /// repetition: `OPTIONAL` -> 1, `REQUIRED` -> 0. The schema list's first
    /// entry is the root (message) node, so leaf column `i` is
    /// `schema[i + 1]` for a flat (non-nested) schema.
    fn max_definition_level(&self, column_index: usize) -> u32 {
        match self
            .metadata
            .schema
            .get(column_index + 1)
            .and_then(|s| s.repetition)
        {
            Some(Repetition::Optional) => 1,
            _ => 0,
        }
    }

    /// Fixed byte width for a `FIXED_LEN_BYTE_ARRAY` leaf column.
    fn type_length(&self, column_index: usize) -> Result<usize> {
        self.metadata
            .schema
            .get(column_index + 1)
            .and_then(|s| s.type_length)
            .map(|len| len as usize)
            .ok_or(FileError::MissingTypeLength)
    }

    fn converted_type(&self, column_index: usize) -> Option<footer::ConvertedType> {
        self.metadata
            .schema
            .get(column_index + 1)
            .and_then(|s| s.converted_type)
    }

    fn decimal_scale(&self, column_index: usize) -> Result<i32> {
        self.metadata
            .schema
            .get(column_index + 1)
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
    pub fn num_rows(&self) -> i64 {
        self.row_group.num_rows
    }

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

    /// Slice of the file covering one column chunk's page data: from its
    /// first page's offset (the dictionary page's, if present, else the
    /// first data page's) through `total_compressed_size` bytes.
    /// `total_compressed_size` spans every page in the chunk including the
    /// dictionary page, so it must be added to that same starting offset —
    /// adding it to `data_page_offset` instead over-extends the slice into
    /// the next column's bytes whenever a dictionary page precedes.
    fn column_chunk_bytes(&self, column_index: usize) -> Result<&'a [u8]> {
        let meta = self.column_meta(column_index)?;
        let start = meta.dictionary_page_offset.unwrap_or(meta.data_page_offset) as usize;
        let end = start + meta.total_compressed_size as usize;
        self.file
            .data
            .get(start..end)
            .ok_or(FileError::ChunkOutOfBounds)
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
        let codec = self.column_meta(column_index)?.codec;
        let chunk_bytes = self.column_chunk_bytes(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        let mut pos = 0usize;
        let mut dictionary: Option<Vec<T>> = None;
        let mut out: Vec<Option<T>> = Vec::new();

        while pos < chunk_bytes.len() {
            let (header, consumed) = page::decode_page_header(&chunk_bytes[pos..])?;
            let page_start = pos + consumed;
            let page_end = page_start + header.compressed_page_size as usize;
            let compressed = chunk_bytes
                .get(page_start..page_end)
                .ok_or(FileError::ChunkOutOfBounds)?;

            match header.page_type {
                PageType::Data(data_page_header) => {
                    let page_body = compression::decompress(
                        codec,
                        compressed,
                        header.uncompressed_page_size as usize,
                    )?;
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
                    let dictionary_body = compression::decompress(
                        codec,
                        compressed,
                        header.uncompressed_page_size as usize,
                    )?;
                    dictionary = Some(decode_dictionary(
                        &dictionary_body,
                        dictionary_page_header.num_values,
                    )?);
                }
            }
            pos = page_end;
        }
        Ok(out)
    }

    /// Read a column chunk's raw dictionary indices plus its dictionary,
    /// *without* resolving each row's value — for callers that only need to
    /// compare/group rows (e.g. `GROUP BY`), where comparing a `u32` index
    /// is far cheaper per row than allocating and comparing e.g. a
    /// `String`. Returns `None` when the column isn't dictionary-encoded
    /// (callers should fall back to the plain `read_*_column` in that case).
    /// Like [`Self::read_column`]'s dictionary path, but keeps indices raw.
    /// Returns `None` (meaning: fall back to `read_*_column`) whenever the
    /// chunk isn't dictionary-encoded throughout — no dictionary page at
    /// all, or a dictionary-fallback to a `PLAIN` page partway through,
    /// which can't be represented as dictionary indices.
    fn read_column_dictionary_indices<T>(
        &self,
        column_index: usize,
        decode_dictionary: impl Fn(&[u8], i32) -> reader::Result<Vec<T>>,
    ) -> Result<Option<DictionaryIndices<T>>> {
        let codec = self.column_meta(column_index)?.codec;
        let chunk_bytes = self.column_chunk_bytes(column_index)?;
        let max_def_level = self.file.max_definition_level(column_index);

        let mut pos = 0usize;
        let mut dictionary: Option<Vec<T>> = None;
        let mut indices: Vec<Option<u32>> = Vec::new();

        while pos < chunk_bytes.len() {
            let (header, consumed) = page::decode_page_header(&chunk_bytes[pos..])?;
            let page_start = pos + consumed;
            let page_end = page_start + header.compressed_page_size as usize;
            let compressed = chunk_bytes
                .get(page_start..page_end)
                .ok_or(FileError::ChunkOutOfBounds)?;

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
                    let page_body = compression::decompress(
                        codec,
                        compressed,
                        header.uncompressed_page_size as usize,
                    )?;
                    indices.extend(reader::read_dictionary_index_column(
                        &page_body,
                        &data_page_header,
                        max_def_level,
                    )?);
                }
                PageType::Dictionary(dictionary_page_header) => {
                    if dictionary.is_some() {
                        return Err(FileError::UnexpectedDictionaryPage);
                    }
                    let dictionary_body = compression::decompress(
                        codec,
                        compressed,
                        header.uncompressed_page_size as usize,
                    )?;
                    dictionary = Some(decode_dictionary(
                        &dictionary_body,
                        dictionary_page_header.num_values,
                    )?);
                }
            }
            pos = page_end;
        }

        Ok(dictionary.map(|dict| (dict, indices)))
    }

    /// Decode one nested/repeated leaf column's raw levels and values
    /// (PLAIN encoding only — dictionary-encoded nested leaves aren't
    /// supported yet, see [`FileError::UnsupportedNestedDictionary`]).
    fn read_leaf_entries(&self, column_index: usize, leaf: &SchemaNode) -> Result<LeafEntries> {
        let codec = self.column_meta(column_index)?.codec;
        let chunk_bytes = self.column_chunk_bytes(column_index)?;
        let physical_type = leaf
            .element
            .physical_type
            .ok_or(FileError::MissingColumnMetadata)?;

        let mut pos = 0usize;
        let mut out = LeafEntries::default();

        while pos < chunk_bytes.len() {
            let (header, consumed) = page::decode_page_header(&chunk_bytes[pos..])?;
            let page_start = pos + consumed;
            let page_end = page_start + header.compressed_page_size as usize;
            let compressed = chunk_bytes
                .get(page_start..page_end)
                .ok_or(FileError::ChunkOutOfBounds)?;

            match header.page_type {
                PageType::Dictionary(_) => return Err(FileError::UnsupportedNestedDictionary),
                PageType::Data(data_page_header) => {
                    if data_page_header.encoding != Encoding::Plain {
                        return Err(FileError::UnsupportedNestedEncoding(
                            data_page_header.encoding,
                        ));
                    }
                    let page_body = compression::decompress(
                        codec,
                        compressed,
                        header.uncompressed_page_size as usize,
                    )?;
                    let (rep_levels, def_levels, mut value_bytes) = reader::split_rep_def_levels(
                        &page_body,
                        &data_page_header,
                        leaf.max_rep_level,
                        leaf.max_def_level,
                    )?;

                    let num_values = data_page_header.num_values as usize;
                    let mut bit_pos = 0usize;
                    for i in 0..num_values {
                        let def = def_levels.get(i).copied().unwrap_or(leaf.max_def_level);
                        if def == leaf.max_def_level {
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
            pos = page_end;
        }
        Ok(out)
    }

    pub fn read_int64_column_dictionary_indices(
        &self,
        column_index: usize,
    ) -> Result<Option<DictionaryIndices<i64>>> {
        self.read_column_dictionary_indices(column_index, reader::decode_dictionary_int64)
    }

    pub fn read_double_column_dictionary_indices(
        &self,
        column_index: usize,
    ) -> Result<Option<DictionaryIndices<f64>>> {
        self.read_column_dictionary_indices(column_index, reader::decode_dictionary_double)
    }

    pub fn read_boolean_column_dictionary_indices(
        &self,
        column_index: usize,
    ) -> Result<Option<DictionaryIndices<bool>>> {
        self.read_column_dictionary_indices(column_index, reader::decode_dictionary_boolean)
    }

    pub fn read_string_column_dictionary_indices(
        &self,
        column_index: usize,
    ) -> Result<Option<DictionaryIndices<String>>> {
        self.read_column_dictionary_indices(column_index, reader::decode_dictionary_string)
    }

    pub fn read_int64_column(&self, column_index: usize) -> Result<Vec<Option<i64>>> {
        self.read_column(
            column_index,
            reader::read_int64_column,
            reader::decode_dictionary_int64,
            reader::read_int64_column_dictionary,
        )
    }

    pub fn read_double_column(&self, column_index: usize) -> Result<Vec<Option<f64>>> {
        self.read_column(
            column_index,
            reader::read_double_column,
            reader::decode_dictionary_double,
            reader::read_double_column_dictionary,
        )
    }

    pub fn read_boolean_column(&self, column_index: usize) -> Result<Vec<Option<bool>>> {
        self.read_column(
            column_index,
            reader::read_boolean_column,
            reader::decode_dictionary_boolean,
            reader::read_boolean_column_dictionary,
        )
    }

    pub fn read_string_column(&self, column_index: usize) -> Result<Vec<Option<String>>> {
        self.read_column(
            column_index,
            reader::read_string_column,
            reader::decode_dictionary_string,
            reader::read_string_column_dictionary,
        )
    }

    pub fn read_int32_column(&self, column_index: usize) -> Result<Vec<Option<i32>>> {
        self.read_column(
            column_index,
            reader::read_int32_column,
            reader::decode_dictionary_int32,
            reader::read_int32_column_dictionary,
        )
    }

    pub fn read_float_column(&self, column_index: usize) -> Result<Vec<Option<f32>>> {
        self.read_column(
            column_index,
            reader::read_float_column,
            reader::decode_dictionary_float,
            reader::read_float_column_dictionary,
        )
    }

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
                Ok(raw
                    .into_iter()
                    .map(|v| v.map(|v| if is_millis { v * 1000 } else { v }))
                    .collect())
            }
            footer::PhysicalType::Int96 => {
                let raw = self.read_int96_column(column_index)?;
                Ok(raw
                    .into_iter()
                    .map(|v| v.map(int96_to_epoch_micros))
                    .collect())
            }
            other => Err(FileError::UnsupportedTimestampPhysicalType(other)),
        }
    }

    /// Read a `DECIMAL`-annotated column (INT32, INT64, or
    /// `FIXED_LEN_BYTE_ARRAY` physical type), preserving exact precision via
    /// [`crate::column::parquet::decimal::Decimal`] rather than converting to `f64` (#52, #53).
    pub fn read_decimal_column(
        &self,
        column_index: usize,
    ) -> Result<Vec<Option<crate::column::parquet::decimal::Decimal>>> {
        let scale = self.file.decimal_scale(column_index)?;
        match self.column_meta(column_index)?.physical_type {
            footer::PhysicalType::Int32 => {
                let raw = self.read_int32_column(column_index)?;
                Ok(raw
                    .into_iter()
                    .map(|v| {
                        v.map(|v| crate::column::parquet::decimal::Decimal {
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
                        v.map(|v| crate::column::parquet::decimal::Decimal {
                            unscaled: v as i128,
                            scale,
                        })
                    })
                    .collect())
            }
            footer::PhysicalType::FixedLenByteArray => {
                let raw = self.read_fixed_len_byte_array_column(column_index)?;
                Ok(raw
                    .into_iter()
                    .map(|v| {
                        v.map(|bytes| crate::column::parquet::decimal::Decimal {
                            unscaled: crate::column::parquet::decimal::from_be_bytes(&bytes),
                            scale,
                        })
                    })
                    .collect())
            }
            other => Err(FileError::UnsupportedDecimalPhysicalType(other)),
        }
    }
}

/// Julian day number for the Unix epoch (1970-01-01T00:00:00Z).
const JULIAN_DAY_UNIX_EPOCH: i64 = 2_440_588;

/// INT96 = 8-byte nanoseconds-within-day + 4-byte Julian day number.
fn int96_to_epoch_micros(v: reader::Int96) -> i64 {
    let days_since_epoch = v.julian_day as i64 - JULIAN_DAY_UNIX_EPOCH;
    days_since_epoch * 86_400_000_000 + v.time_nanos / 1000
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
    fn column_index_out_of_range_errors() {
        let file_bytes = build_file(&[&[1]]);
        let file = ParquetFile::open(&file_bytes).unwrap();
        let rg = file.row_group(0).unwrap();
        let result = rg.read_int64_column(5);
        assert!(matches!(result, Err(FileError::ColumnIndexOutOfRange(5))));
    }
}
