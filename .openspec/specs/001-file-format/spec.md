---

> Moved from t-rust-db/tools `sqlite-rs/.openspec/specs/003-file-format/spec.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.
domain: storage
version: 0.1.0
status: draft
date: 2026-08-13
---

# 003 — File Format

The static half of the compatibility contract: what the bytes of a `.sqlite` file mean. Covers the database header, the record/serial-type format, and the read-only VFS that feeds them. This spec backs V1 steps 1 (#11) and 3 (#9); its scenarios were partially validated by spike 002 (#4/#6).

Everything in this spec is **Tier 0 READ CORE** — never droppable.

## Philosophy

The file format is SQLite's real product — frozen until 2050. We implement it from [fileformat2.html](https://www.sqlite.org/fileformat2.html) and verify byte-by-byte against the pinned oracle (sqlite-rs spec 004). Where spike 002 found traps (page-1 offsets, codec-reserved bytes), the trap is encoded here as a scenario so it cannot regress.

## Requirements

### Requirement 1: VFS Abstraction [MUST]

The system MUST provide a virtual filesystem abstraction with a Unix implementation and an in-memory implementation. Both MUST pass an identical test suite. The trait was subsequently extended with locking (`lock_shared`, `src/storage/row/vfs/lock.rs`) and write methods (`write_at`/`truncate`/`sync`) without breaking consumers, as spike 004 (#8) required.

**Implementation:** `src/storage/row/vfs.rs`

**Tests:** inline #[cfg(test)] in src/storage/row/vfs.rs

#### Scenario: Read at offset

- GIVEN an open database file of 3 pages
- WHEN `read_at(buf, page_size)` is called
- THEN exactly page 2's bytes are returned

**Tests:** `src/storage/row/vfs.rs::unix_vfs_contract`

#### Scenario: Companion file detection

- GIVEN a database `test.db` with an adjacent `test.db-wal`
- WHEN the VFS is asked whether the WAL companion exists
- THEN it MUST report true (and equivalently for `-journal`)

**Tests:** `sqlite-rs:tests/unit/vfs.rs::companion_file_detection_from_public_api`

#### Scenario: In-memory parity

- GIVEN the same byte content in a Unix-backed file and an in-memory file
- WHEN the full VFS test suite runs against both
- THEN results MUST be identical

**Tests:** `src/storage/row/vfs.rs::memory_vfs_contract`

### Requirement 2: Database Header [MUST]

The system MUST parse and validate the 100-byte database header: magic string, page size (including the `1` = 65536 encoding), read/write versions (journal vs WAL mode detection), reserved bytes per page, text encoding, page count, freelist head and count, schema cookie and format, auto-vacuum largest-root page, user version, and application id. Malformed headers MUST produce errors, never panics.

**Implementation:** `src/storage/row/header.rs`

**Tests:** inline #[cfg(test)] in src/storage/row/header.rs

**Corpus:** `sqlite-rs:tests/corpus/fixtures/pagesizes/`

#### Scenario: Magic validation

- GIVEN a file not starting with `SQLite format 3\0`
- WHEN the header is parsed
- THEN a clear "not a SQLite database" error is returned

**Tests:** `src/storage/row/header.rs::bad_magic_is_rejected`

#### Scenario: Page size decoding

- GIVEN headers declaring page sizes 512, 4096, 65536 (encoded as 1)
- WHEN parsed
- THEN the decoded page sizes are 512, 4096, and 65536 respectively

**Tests:** `src/storage/row/header.rs::page_size_512, `src/storage/row/header.rs::page_size_65536_via_one_encoding``

#### Scenario: Reserved bytes reduce usable page size

- GIVEN a database with 12 reserved bytes per page (spike 002: macOS see-cccrypt codec produces these)
- WHEN the usable page size is computed
- THEN it is `page_size - 12`, and cell content is read within the usable region only

**Tests:** `src/storage/row/header.rs::reserved_bytes_12`

#### Scenario: WAL mode detection

- GIVEN a database with read/write version bytes = 2
- WHEN the header is parsed
- THEN the database is reported as WAL-mode

**Tests:** `src/storage/row/header.rs::wal_journal_mode_detected`

#### Scenario: Page-1 offset documentation

- GIVEN page 1 (which contains both the 100-byte header and the schema b-tree root)
- WHEN its b-tree page header is located
- THEN it begins at byte 100, BUT cell pointer offsets within it are relative to byte 0 of the page (spike 002 finding 2)

**Tests:** `sqlite-rs:tests/unit/header.rs::page_1_header_does_not_shift_page_relative_offsets`

### Requirement 3: Varint Decoding [MUST]

The system MUST decode SQLite's 1–9 byte big-endian varints. The 9-byte form carries a full 64 bits. Malformed input (truncated buffer) MUST return an error.

**Implementation:** `src/storage/row/record/varint.rs`

**Tests:** inline #[cfg(test)] in src/storage/row/record/varint.rs

#### Scenario: All lengths decode

- GIVEN varints of every encoded length 1 through 9 bytes
- WHEN decoded
- THEN each yields the correct value and consumed-byte count

**Tests:** `src/storage/row/record/varint.rs::every_length_from_1_to_9_bytes`, `sqlite-rs:tests/proptest/record_proptest.rs::encode_decode_varint_roundtrip`

#### Scenario: Nine-byte full width

- GIVEN the 9-byte varint encoding of `u64::MAX`
- WHEN decoded
- THEN the value is exactly `u64::MAX`

**Tests:** `src/storage/row/record/varint.rs::every_length_from_1_to_9_bytes`

#### Scenario: Truncated input

- GIVEN a buffer ending mid-varint
- WHEN decoded
- THEN an error is returned, no panic

**Tests:** `src/storage/row/record/varint.rs::truncated_input_errors_not_panics`

### Requirement 4: Serial Type Decoding [MUST]

The system MUST decode every SQLite serial type: NULL (0), 1/2/3/4/6/8-byte signed big-endian integers (types 1–6), IEEE-754 f64 (7), integer constants 0 and 1 (8/9), BLOB (N≥12, even), TEXT (N≥13, odd). Floats MUST round-trip bit-exact (`f64::to_bits` equality with the oracle) — display formatting is out of scope (sqlite-rs spec 001 / step 9).

**Implementation:** `src/storage/row/record/decode.rs::decode_serial_value`

**Tests:** inline #[cfg(test)] in src/storage/row/record/decode.rs

**Corpus:** `sqlite-rs:tests/corpus/fixtures/serialtypes/`

#### Scenario: Integer extremes

- GIVEN stored values `i64::MIN`, `-1`, `0`, `1`, `i64::MAX` across all integer widths
- WHEN decoded
- THEN each value is exact

**Tests:** `src/storage/row/record/decode.rs::integer_widths_and_edge_values`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_integer_i8_roundtrip`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_integer_i16_roundtrip`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_integer_i24_roundtrip`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_integer_i32_roundtrip`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_integer_i48_roundtrip`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_integer_i64_roundtrip`

#### Scenario: Float bit-exactness

- GIVEN stored REAL values including `-0.0`, `2.5e300`, and a NaN payload
- WHEN decoded
- THEN `f64::to_bits()` equals the oracle's stored bits

**Tests:** `src/storage/row/record/decode.rs::real_edge_values_bit_identical`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_real_roundtrip_bit_exact_or_nan_to_null`

#### Scenario: Constant serial types

- GIVEN serial types 8 and 9
- WHEN decoded
- THEN they yield integers 0 and 1 with zero payload bytes consumed

**Tests:** `sqlite-rs:tests/unit/record.rs::constant_serial_types_8_and_9`

#### Scenario: Empty blob and text

- GIVEN serial types 12 (empty BLOB) and 13 (empty TEXT)
- WHEN decoded
- THEN empty values are produced, not errors

**Tests:** `src/storage/row/record/decode.rs::blob_including_zero_length`, `src/storage/row/record/decode.rs::text_utf8_including_empty`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_blob_roundtrip`, `sqlite-rs:tests/proptest/record_proptest.rs::prop_text_utf8_roundtrip`

### Requirement 5: Text Encoding [MUST]

The system MUST decode TEXT values in all three database encodings: UTF-8 (1), UTF-16LE (2), UTF-16BE (3), selected by header byte 56.

**Implementation:** `src/storage/row/record/decode.rs`

**Tests:** inline #[cfg(test)] in src/storage/row/record/decode.rs

**Corpus:** `sqlite-rs:tests/corpus/fixtures/encodings/`

#### Scenario: UTF-16 both orders

- GIVEN databases created with `PRAGMA encoding='UTF-16le'` and `'UTF-16be'` containing `héllo→`
- WHEN text values are decoded
- THEN both produce the identical correct string

**Tests:** `src/storage/row/record/decode.rs::text_utf16le_and_utf16be`

### Requirement 6: Record Decoding [MUST]

The system MUST decode complete records: header-size varint, serial-type list, then body values in order. Malformed records (header longer than payload, truncated body) MUST return errors, never panic. The decoder is pure — no I/O.

**Implementation:** `src/storage/row/record/decode.rs::decode_record`

**Tests:** inline #[cfg(test)] in src/storage/row/record/decode.rs

**Corpus:** `sqlite-rs:tests/corpus/fixtures/serialtypes/`

#### Scenario: Mixed-type row

- GIVEN the spike 002 fixture row `(42, 'hello', 3.14, X'DEADBEEF', NULL)`
- WHEN the record is decoded
- THEN five values of the correct types and contents are produced

**Tests:** `sqlite-rs:tests/unit/record.rs::mixed_type_row`

#### Scenario: Fuzz safety

- GIVEN arbitrary malformed byte sequences
- WHEN decoded
- THEN the decoder returns errors and never panics (fuzz target)

**Tests:** `src/storage/row/record/decode.rs::truncated_record_at_every_offset_errors_not_panics`, `sqlite-rs:tests/fuzz/fuzz_targets/decode_record.rs`
