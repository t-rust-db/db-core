//! Parquet page header decoding (`PageHeader` / `DataPageHeader` /
//! `DictionaryPageHeader` from `parquet.thrift`), covering DATA_PAGE (v1) and
//! DICTIONARY_PAGE — the page types produced for PLAIN- and
//! PLAIN_DICTIONARY/RLE_DICTIONARY-encoded, uncompressed columns.

use crate::column::parquet::thrift::{self, ThriftError, Value};

#[derive(Debug)]
pub enum PageError {
    Thrift(ThriftError),
    UnsupportedPageType(i32),
    MissingDataPageHeader,
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::Thrift(e) => write!(f, "failed to decode page header: {e}"),
            PageError::UnsupportedPageType(t) => write!(f, "unsupported page type: {t}"),
            PageError::MissingDataPageHeader => {
                write!(f, "DATA_PAGE header missing data_page_header field")
            }
        }
    }
}

impl std::error::Error for PageError {}

impl From<ThriftError> for PageError {
    fn from(e: ThriftError) -> Self {
        PageError::Thrift(e)
    }
}

pub type Result<T> = std::result::Result<T, PageError>;

/// `parquet.thrift` `Encoding` enum (values relevant to this reader).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Plain,
    PlainDictionary,
    Rle,
    DeltaBinaryPacked,
    RleDictionary,
    Other(i32),
}

impl From<i32> for Encoding {
    fn from(v: i32) -> Self {
        match v {
            0 => Encoding::Plain,
            2 => Encoding::PlainDictionary,
            3 => Encoding::Rle,
            5 => Encoding::DeltaBinaryPacked,
            8 => Encoding::RleDictionary,
            other => Encoding::Other(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataPageHeader {
    pub num_values: i32,
    pub encoding: Encoding,
    pub definition_level_encoding: Encoding,
    pub repetition_level_encoding: Encoding,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DictionaryPageHeader {
    pub num_values: i32,
    pub encoding: Encoding,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PageType {
    Data(DataPageHeader),
    Dictionary(DictionaryPageHeader),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageHeader {
    pub uncompressed_page_size: i32,
    pub compressed_page_size: i32,
    pub page_type: PageType,
}

const PAGE_TYPE_DATA_PAGE: i32 = 0;
const PAGE_TYPE_DICTIONARY_PAGE: i32 = 2;

/// Decode a `PageHeader` from the start of `buf`. Returns the header and the
/// number of bytes consumed, so callers can locate the page body that
/// immediately follows.
pub fn decode_page_header(buf: &[u8]) -> Result<(PageHeader, usize)> {
    let (fields, consumed) = thrift::decode_struct(buf)?;
    let root = Value::Struct(fields);

    let page_type = root.field(1).and_then(Value::as_i32).unwrap_or(-1);
    let uncompressed_page_size = root.field(2).and_then(Value::as_i32).unwrap_or(0);
    let compressed_page_size = root.field(3).and_then(Value::as_i32).unwrap_or(0);

    let page_type = match page_type {
        PAGE_TYPE_DATA_PAGE => {
            let dph = root.field(5).ok_or(PageError::MissingDataPageHeader)?;
            PageType::Data(DataPageHeader {
                num_values: dph.field(1).and_then(Value::as_i32).unwrap_or(0),
                encoding: dph
                    .field(2)
                    .and_then(Value::as_i32)
                    .map(Encoding::from)
                    .unwrap_or(Encoding::Other(-1)),
                definition_level_encoding: dph
                    .field(3)
                    .and_then(Value::as_i32)
                    .map(Encoding::from)
                    .unwrap_or(Encoding::Other(-1)),
                repetition_level_encoding: dph
                    .field(4)
                    .and_then(Value::as_i32)
                    .map(Encoding::from)
                    .unwrap_or(Encoding::Other(-1)),
            })
        }
        PAGE_TYPE_DICTIONARY_PAGE => {
            let dph = root.field(7).ok_or(PageError::MissingDataPageHeader)?;
            PageType::Dictionary(DictionaryPageHeader {
                num_values: dph.field(1).and_then(Value::as_i32).unwrap_or(0),
                encoding: dph
                    .field(2)
                    .and_then(Value::as_i32)
                    .map(Encoding::from)
                    .unwrap_or(Encoding::Other(-1)),
            })
        }
        other => return Err(PageError::UnsupportedPageType(other)),
    };

    Ok((
        PageHeader {
            uncompressed_page_size,
            compressed_page_size,
            page_type,
        },
        consumed,
    ))
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
        fn struct_field(&mut self, field_id: i16, inner: Vec<u8>) {
            self.field_header(field_id, 0x0c);
            self.buf.extend_from_slice(&inner);
        }
        fn finish(mut self) -> Vec<u8> {
            self.buf.push(0x00);
            self.buf
        }
    }

    fn build_data_page_header(
        num_values: i32,
        encoding: i32,
        def_enc: i32,
        rep_enc: i32,
    ) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, num_values);
        w.i32_field(2, encoding);
        w.i32_field(3, def_enc);
        w.i32_field(4, rep_enc);
        w.finish()
    }

    fn build_page_header(
        page_type: i32,
        uncompressed: i32,
        compressed: i32,
        dph: Vec<u8>,
    ) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, page_type);
        w.i32_field(2, uncompressed);
        w.i32_field(3, compressed);
        w.struct_field(5, dph);
        w.finish()
    }

    fn build_dictionary_page_header(num_values: i32, encoding: i32) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, num_values);
        w.i32_field(2, encoding);
        w.finish()
    }

    fn build_dictionary_page_header_full(
        page_type: i32,
        uncompressed: i32,
        compressed: i32,
        dph: Vec<u8>,
    ) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, page_type);
        w.i32_field(2, uncompressed);
        w.i32_field(3, compressed);
        w.field_header(7, 0x0c);
        w.buf.extend_from_slice(&dph);
        w.finish()
    }

    #[test]
    fn decodes_data_page_header() {
        let dph = build_data_page_header(100, 0, 3, 3);
        let mut buf = build_page_header(0, 900, 900, dph);
        buf.extend_from_slice(b"TRAILING-PAGE-BODY-BYTES");

        let (header, consumed) = decode_page_header(&buf).unwrap();
        assert_eq!(header.uncompressed_page_size, 900);
        assert_eq!(header.compressed_page_size, 900);
        let PageType::Data(data_page_header) = header.page_type else {
            panic!("expected Data page")
        };
        assert_eq!(data_page_header.num_values, 100);
        assert_eq!(data_page_header.encoding, Encoding::Plain);
        assert_eq!(data_page_header.definition_level_encoding, Encoding::Rle);
        assert_eq!(&buf[consumed..consumed + 24], b"TRAILING-PAGE-BODY-BYTES");
    }

    #[test]
    fn decodes_dictionary_page_header() {
        let dph = build_dictionary_page_header(5, 0);
        let buf = build_dictionary_page_header_full(2 /* DICTIONARY_PAGE */, 40, 40, dph);

        let (header, _consumed) = decode_page_header(&buf).unwrap();
        let PageType::Dictionary(dictionary_page_header) = header.page_type else {
            panic!("expected Dictionary page")
        };
        assert_eq!(dictionary_page_header.num_values, 5);
        assert_eq!(dictionary_page_header.encoding, Encoding::Plain);
    }

    #[test]
    fn rejects_unsupported_page_type() {
        let dph = build_data_page_header(10, 0, 3, 3);
        let buf = build_page_header(1 /* DATA_PAGE_V2, unsupported */, 10, 10, dph);
        let result = decode_page_header(&buf);
        assert!(matches!(result, Err(PageError::UnsupportedPageType(1))));
    }
}
